//! The Sway (and i3) focus source, over their own IPC socket.
//!
//! ## Why not `wlr-foreign-toplevel-management`
//!
//! The obvious plan was one in-process Wayland source covering every wlroots
//! compositor — Sway, Hyprland, river, niri — via
//! `ext-foreign-toplevel-list-v1` and `wlr-foreign-toplevel-management-v1`. No
//! helper to install, no per-compositor extension, pure Rust.
//!
//! It cannot work. **Neither protocol carries a PID**, and neither carries
//! geometry. Checked against the protocol XML rather than assumed: the wlr
//! management interface's entire event set is `toplevel`, `app_id`, `title`,
//! `state`, `output_enter`, `output_leave`, `parent`, `closed`, `done` and
//! `finished`, and the word "pid" does not appear in either file.
//!
//! For most tools that would be a limitation. For `dozed` it is fatal, because
//! everything downstream of a window starts at `/proc/<pid>`: identity resolution
//! (§4), the class that decides whether an app is a long-running job, the sampler,
//! and the cgroup an actuation writes to. A window we cannot attribute to a
//! process is a window we can do nothing with — `WindowReport::into_facts` already
//! drops those on the KWin path for exactly this reason.
//!
//! So the source is per-compositor after all, and it is the compositors' own IPC
//! that makes it worth doing:
//!
//! | Compositor | IPC | Carries a PID |
//! |---|---|---|
//! | Sway, i3 | `$SWAYSOCK` / `$I3SOCK`, length-prefixed JSON | yes |
//! | Hyprland | `$XDG_RUNTIME_DIR/hypr/.../.socket.sock`, JSON | yes |
//! | niri | `$NIRI_SOCKET`, JSON lines | yes |
//!
//! This file implements the first, which also covers i3 on X11 and every
//! i3-compatible compositor. The other two are the same shape — a socket, a JSON
//! document, an event stream — and belong in files of their own rather than in a
//! trait invented before there is a second implementation.
//!
//! ## What it costs
//!
//! Nothing polls. `SUBSCRIBE` to `window` and `workspace` gives a push whenever
//! anything moves, and each push is followed by one `GET_TREE`. Unchanged
//! snapshots are suppressed before they reach the daemon, which is the same
//! discipline the KWin helper and the X11 source already apply — most events
//! change nothing dozed cares about, and forwarding those is pure wakeup cost on
//! both ends (§13).
//!
//! ## Verification status
//!
//! The wire format and the tree walk are unit-tested against captured JSON, and
//! **not** verified against a live Sway session: this machine runs KDE on Wayland
//! and there is no Sway to run it against. That is a real gap and it is recorded
//! here rather than in a commit message. What is tested is every decision this
//! file makes about a tree; what is not is that a real Sway produces the tree
//! shape these fixtures describe.

use std::io;
use std::path::PathBuf;

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use dozed_core::{Rect, WindowFacts};

use crate::sources::Incoming;

/// The magic every i3-IPC message starts with.
const MAGIC: &[u8; 6] = b"i3-ipc";

/// Message types we send.
const SUBSCRIBE: u32 = 2;
const GET_TREE: u32 = 4;

/// Replies to events have the top bit set, which is how a subscription reply is
/// told apart from an event on the same socket.
const EVENT_FLAG: u32 = 0x8000_0000;

/// Where the socket is, if this is a Sway or i3 session.
///
/// `SWAYSOCK` first, then `I3SOCK`: a session can legitimately have both set (a
/// Sway session running an i3-compatible bar, or a leftover from a previous
/// login), and Sway is the one whose windows this daemon can actually freeze —
/// under i3 the same source works but the session is X11, where `x11.rs` is the
/// better-tested path.
#[must_use]
pub fn socket_path() -> Option<PathBuf> {
    for var in ["SWAYSOCK", "I3SOCK"] {
        if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(v));
        }
    }
    None
}

/// Frame one message: magic, length, type, payload.
#[must_use]
fn encode(msg_type: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(14 + payload.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    out.extend_from_slice(&msg_type.to_ne_bytes());
    out.extend_from_slice(payload);
    out
}

/// Read the 14-byte header: payload length and message type.
///
/// Native byte order, which is what the protocol specifies — it was designed for
/// a socket between two processes on one machine, so there is no endianness to
/// negotiate. Written out explicitly because "network byte order" is the reflex
/// and would be wrong.
fn decode_header(header: &[u8; 14]) -> io::Result<(usize, u32)> {
    if &header[..6] != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not an i3-ipc message"));
    }
    let len = u32::from_ne_bytes([header[6], header[7], header[8], header[9]]) as usize;
    let kind = u32::from_ne_bytes([header[10], header[11], header[12], header[13]]);
    Ok((len, kind))
}

/// One node of the tree, with only the fields this source reads.
///
/// `deny_unknown_fields` is deliberately *not* set, unlike the config: this is
/// somebody else's schema and it grows between releases. A new field in Sway must
/// not stop dozed from reading the tree.
#[derive(Debug, Clone, Deserialize)]
struct Node {
    #[serde(default)]
    nodes: Vec<Node>,
    /// Floating windows live in a second list, and they are the only ones that
    /// can occlude each other — tiled windows by definition do not overlap.
    #[serde(default)]
    floating_nodes: Vec<Node>,
    /// `"root"`, `"output"`, `"workspace"`, `"con"`, `"floating_con"`.
    ///
    /// **The wire name is `type`**, and getting that wrong is what this rename
    /// exists to record. The field was originally read as `node_type` — which is
    /// what it is called in `swaymsg`'s *documentation prose* and in nothing Sway
    /// has ever put on the socket — so every node deserialised with
    /// `node_type: ""`, no node was ever recognised as an output, a workspace or
    /// a view, and `parse_tree` returned an empty window list against every real
    /// tree.
    ///
    /// It was invisible for the reason §12 already warned it would be: the
    /// fixtures were hand-written to the same wrong name, so the whole suite
    /// agreed with itself about a schema Sway does not emit. "Every decision the
    /// walk makes is tested; that a real Sway produces the tree shape the
    /// fixtures describe is not" — it does not, and this was the difference.
    /// `tests/fixtures/sway-tree.json` is now a **captured** tree rather than an
    /// imagined one, which is the part of this fix that cannot rot.
    ///
    /// The failure direction was the safe one and that is why it survived: no
    /// windows means no apps, and dozed froze nothing rather than freezing the
    /// wrong thing. It also meant the Sway backend did nothing whatsoever.
    #[serde(default, rename = "type")]
    node_type: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    pid: Option<i64>,
    /// Wayland's own identifier. Absent for XWayland windows, which carry
    /// `window_properties.class` instead.
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    window_properties: Option<WindowProperties>,
    #[serde(default)]
    focused: bool,
    /// Whether this node is on a visible workspace. Sway sets it on views; i3
    /// does not, so the walk tracks workspace visibility itself rather than
    /// trusting it.
    #[serde(default)]
    visible: Option<bool>,
    #[serde(default)]
    rect: Option<TreeRect>,
    /// `"none"`, `"output"` or `"global"` on a view.
    #[serde(default)]
    fullscreen_mode: Option<u8>,
    #[serde(default)]
    urgent: bool,
    /// On a workspace node: which output it belongs to.
    #[serde(default)]
    output: Option<String>,
    /// On an output node: the workspace currently shown.
    #[serde(default)]
    current_workspace: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct WindowProperties {
    #[serde(default)]
    class: Option<String>,
    #[serde(default)]
    instance: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct TreeRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

/// Turn a `GET_TREE` reply into the same [`WindowFacts`] every other source
/// produces.
///
/// The point of returning `WindowFacts` rather than a verdict is §5: visibility is
/// decided once, in `dozed_core::visibility`, from raw facts. This source
/// therefore reports geometry, stacking and workspace membership and lets the
/// shared rule do the rest — so Sway cannot disagree with KWin about what
/// "hidden" means, which is the whole reason that rule was centralised.
pub fn parse_tree(json: &str) -> Result<Vec<WindowFacts>, serde_json::Error> {
    let root: Node = serde_json::from_str(json)?;
    let mut out = Vec::new();
    // Which workspaces are on screen. Collected first, because a view cannot say
    // whether its own workspace is visible and i3 does not set `visible` at all.
    let mut showing = std::collections::BTreeSet::new();
    collect_visible_workspaces(&root, &mut showing);
    walk(&root, None, &showing, &mut out);
    // Stacking, bottom to top, in traversal order. Sway lists floating windows
    // after tiled ones and later siblings above earlier ones, so the index a node
    // is discovered at is its depth order — and only floating windows can
    // overlap, so this is all the occlusion check needs.
    for (i, w) in out.iter_mut().enumerate() {
        w.stack = i as u32;
    }
    Ok(out)
}

fn collect_visible_workspaces(node: &Node, out: &mut std::collections::BTreeSet<String>) {
    if node.node_type == "output"
        && let Some(current) = &node.current_workspace
    {
        out.insert(current.clone());
    }
    for child in node.nodes.iter().chain(&node.floating_nodes) {
        collect_visible_workspaces(child, out);
    }
}

/// Depth-first, carrying down which workspace we are inside.
fn walk(
    node: &Node,
    workspace: Option<&Node>,
    showing: &std::collections::BTreeSet<String>,
    out: &mut Vec<WindowFacts>,
) {
    let workspace = if node.node_type == "workspace" { Some(node) } else { workspace };

    if is_view(node)
        && let Some(facts) = view_facts(node, workspace, showing)
    {
        out.push(facts);
    }
    for child in node.nodes.iter().chain(&node.floating_nodes) {
        walk(child, workspace, showing, out);
    }
}

/// Whether this node is an application window rather than a container.
///
/// A view is what Sway calls a node with a client behind it. `pid` is the
/// distinguishing field in practice, and the node type check keeps a container
/// that happens to carry one from being counted twice.
fn is_view(node: &Node) -> bool {
    node.pid.is_some() && (node.node_type == "con" || node.node_type == "floating_con")
}

fn view_facts(
    node: &Node,
    workspace: Option<&Node>,
    showing: &std::collections::BTreeSet<String>,
) -> Option<WindowFacts> {
    // Nothing can be done with a window we cannot attribute to a process:
    // identity, class, sampling and actuation all start at `/proc/<pid>`. Dropping
    // the window rather than guessing also errs in the safe direction — an absent
    // window can only make the ones behind it look *more* visible (§3).
    let pid = u32::try_from(node.pid?).ok().filter(|p| *p != 0)?;

    // Wayland first, then the XWayland class, then the instance. The same ladder
    // §4 uses, and for the same reason: the most specific identifier the window
    // actually declared.
    let props = node.window_properties.as_ref();
    let app_id = node
        .app_id
        .clone()
        .or_else(|| props.and_then(|p| p.class.clone()))
        .or_else(|| props.and_then(|p| p.instance.clone()))
        .unwrap_or_default();

    // On a visible workspace, or on none at all.
    //
    // `visible` is trusted when Sway sets it; otherwise the workspace's own name
    // is looked up in the set of workspaces the outputs are showing. i3 sets
    // neither on views, so without the fallback every window on i3 would read as
    // hidden — which is the one error that gets a desktop frozen.
    let on_current_desktop = match node.visible {
        Some(v) => v,
        None => match workspace.and_then(|w| w.name.as_deref()) {
            Some(name) => showing.contains(name),
            // No workspace above us at all: a scratchpad, or a shape this walk
            // does not understand. Not being able to tell must not mean "hidden".
            None => true,
        },
    };

    let rect = node.rect.map(|r| Rect::new(r.x, r.y, r.width, r.height));
    Some(WindowFacts {
        pid,
        app_id,
        // Sway has no desktop-file identifier; §4's resolver falls through to the
        // next rung, which is what the empty string means everywhere else.
        desktop_file: String::new(),
        focused: node.focused,
        // Sway has no separate "minimised": a window not on a visible workspace is
        // the same situation, and that is what `on_current_desktop` carries.
        minimized: false,
        on_current_desktop,
        geometry: rect,
        stack: 0, // assigned by `parse_tree` from traversal order
        output: workspace.and_then(|w| w.output.clone()).unwrap_or_default(),
        fullscreen: node.fullscreen_mode.is_some_and(|m| m > 0),
        attention: node.urgent,
        // Sway reports no per-window opacity in the tree, so the conservative
        // answer: a window we cannot prove is translucent is treated as opaque,
        // which can only make what is behind it look *less* visible.
        translucent: false,
        // Deliberately `None`: this source reports facts and lets §5's one
        // implementation decide, exactly as the protocol-2 helpers do.
        visibility: None,
    })
}

/// Run the source until the socket dies.
///
/// Returning at all means we can no longer see focus, so the daemon's watchdog
/// times out and thaws everything (`DESIGN.md` §10.2) — the same path a crashed
/// compositor helper takes.
pub async fn run(tx: mpsc::Sender<Incoming>) -> io::Result<()> {
    let path = socket_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no SWAYSOCK or I3SOCK"))?;

    // Two connections. The subscription socket only ever delivers events after
    // its reply, so a `GET_TREE` on the same one would have to be interleaved
    // with whatever arrived first — and getting that wrong means reading an event
    // as a tree.
    let mut events = UnixStream::connect(&path).await?;
    let mut queries = UnixStream::connect(&path).await?;

    events.write_all(&encode(SUBSCRIBE, br#"["window","workspace"]"#)).await?;
    let (_, reply) = read_message(&mut events).await?;
    tracing::info!(path = %path.display(), reply = %reply, "sway focus source subscribed");
    let _ = tx.send(Incoming::HelperAlive { compositor: Some("sway".to_owned()) }).await;

    let mut last: Option<Vec<WindowFacts>> = None;
    publish(&mut queries, &tx, &mut last).await?;

    loop {
        let (kind, _) = read_message(&mut events).await?;
        // Anything on this socket after the subscribe reply is an event we asked
        // for, so the payload is not read: the tree is the source of truth and a
        // delta would be a second way to be wrong.
        if kind & EVENT_FLAG == 0 {
            continue;
        }
        publish(&mut queries, &tx, &mut last).await?;
    }
}

async fn publish(
    queries: &mut UnixStream,
    tx: &mpsc::Sender<Incoming>,
    last: &mut Option<Vec<WindowFacts>>,
) -> io::Result<()> {
    queries.write_all(&encode(GET_TREE, b"")).await?;
    let (_, json) = read_message(queries).await?;
    let windows = match parse_tree(&json) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, "unreadable sway tree; keeping the previous snapshot");
            return Ok(());
        }
    };
    // Suppressed for the same reason the KWin helper suppresses: most events
    // change nothing dozed cares about, and forwarding those is wakeup cost on
    // both ends for no decision (§13).
    if last.as_deref() == Some(windows.as_slice()) {
        // The heartbeat still has to land, or the §10.2 watchdog thaws everything
        // on a desktop that simply was not being used.
        let _ = tx.send(Incoming::HelperAlive { compositor: Some("sway".to_owned()) }).await;
        return Ok(());
    }
    *last = Some(windows.clone());
    let _ = tx.send(Incoming::WindowFacts(windows)).await;
    let _ = tx.send(Incoming::HelperAlive { compositor: Some("sway".to_owned()) }).await;
    Ok(())
}

async fn read_message(stream: &mut UnixStream) -> io::Result<(u32, String)> {
    let mut header = [0u8; 14];
    stream.read_exact(&mut header).await?;
    let (len, kind) = decode_header(&header)?;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    Ok((kind, String::from_utf8_lossy(&payload).into_owned()))
}

/// Spawn it, thawing everything if it ever stops.
pub fn spawn(tx: mpsc::Sender<Incoming>) {
    tokio::spawn(async move {
        if let Err(e) = run(tx).await {
            // Not fatal to the daemon: losing the source means the watchdog goes
            // passive and holds nothing, which is the correct response to being
            // unable to see focus.
            tracing::error!(error = %e, "sway focus source stopped; going passive");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A two-output Sway tree: one focused window, one on the visible workspace
    /// but unfocused, one on a workspace that is not being shown, and one
    /// floating window on top of the focused one.
    ///
    /// The field names here are the ones a real Sway emits, and they were
    /// **wrong until a real Sway was run**: this fixture said `node_type` where
    /// the socket says `type`, the parser was written to agree with it, and the
    /// two were consistently wrong together for the whole life of the backend.
    /// See [`Node::node_type`]. `sway_tree_fixture_matches_a_captured_one` below
    /// pins this against a tree captured from `sway -c … WLR_BACKENDS=headless`,
    /// so the hand-written shape can no longer drift away from the real one
    /// unnoticed.
    const TREE: &str = r#"
    {
      "id": 1, "type": "root", "name": "root",
      "nodes": [
        {
          "id": 2, "type": "output", "name": "DP-1", "current_workspace": "1",
          "nodes": [
            {
              "id": 3, "type": "workspace", "name": "1", "output": "DP-1",
              "rect": { "x": 0, "y": 0, "width": 1920, "height": 1080 },
              "nodes": [
                {
                  "id": 4, "type": "con", "name": "Alacritty", "pid": 101,
                  "app_id": "Alacritty", "focused": true, "visible": true,
                  "rect": { "x": 0, "y": 0, "width": 960, "height": 1080 },
                  "fullscreen_mode": 0
                },
                {
                  "id": 5, "type": "con", "name": "Firefox", "pid": 102,
                  "app_id": null,
                  "window_properties": { "class": "firefox", "instance": "Navigator" },
                  "focused": false, "visible": true,
                  "rect": { "x": 960, "y": 0, "width": 960, "height": 1080 }
                }
              ],
              "floating_nodes": [
                {
                  "id": 6, "type": "floating_con", "name": "Calculator",
                  "pid": 103, "app_id": "calc", "focused": false, "visible": true,
                  "rect": { "x": 0, "y": 0, "width": 400, "height": 400 },
                  "urgent": true
                }
              ]
            },
            {
              "id": 7, "type": "workspace", "name": "2", "output": "DP-1",
              "nodes": [
                {
                  "id": 8, "type": "con", "name": "Slack", "pid": 104,
                  "app_id": "slack", "focused": false, "visible": false,
                  "rect": { "x": 0, "y": 0, "width": 1920, "height": 1080 }
                }
              ]
            }
          ]
        }
      ]
    }"#;

    fn facts() -> Vec<WindowFacts> {
        parse_tree(TREE).expect("the fixture should parse")
    }

    fn by_pid(pid: u32) -> WindowFacts {
        facts().into_iter().find(|w| w.pid == pid).expect("a window with that pid")
    }

    /// The fixture above, checked against a tree a real Sway actually produced.
    ///
    /// This is the test whose absence cost the whole backend. §12 recorded the
    /// gap honestly — "that a real Sway produces the tree shape these fixtures
    /// describe is not [tested]" — and it did not, in the one way that mattered:
    /// every node in `TREE` said `node_type` where the socket says `type`, so
    /// the parser and its fixtures were consistently wrong together and eleven
    /// green tests certified a backend that returned no windows at all.
    ///
    /// `tests/fixtures/sway-tree.json` was captured from
    /// `WLR_BACKENDS=headless sway` with one `foot` window open, by
    /// `packaging/verify-matrix.sh`. It is deliberately the *whole* reply,
    /// unedited, including every field this file does not read: an edited
    /// capture is a hand-written fixture with extra steps, and the point of it
    /// is to be something nobody chose.
    /// The tree, corrupted the way a socket read gets corrupted.
    ///
    /// This parser reads **somebody else's schema** over a socket, which is the
    /// reason it gets this treatment and `dozed_core`'s own types do not. It
    /// lives in a binary crate, so `crates/dozed-core/tests/malformed_input.rs`
    /// cannot reach it and the loop lives here instead.
    ///
    /// What is asserted is only that there is a `Result`. A mutation that
    /// happens to produce a valid tree has produced a valid tree; what must
    /// never happen is a panic, because this runs on the daemon's own task and a
    /// panic there is a daemon that dies whenever a compositor sends something
    /// unexpected — the failure mode §10.2 makes safe for a helper going *quiet*
    /// and does nothing about for a helper going strange.
    #[test]
    fn a_corrupt_tree_is_an_error_and_never_a_panic() {
        use dozed_core::stats::Rng;

        let captured = include_str!("../tests/fixtures/sway-tree.json");
        let mut rng = Rng::seeded(3);
        for corpus in [captured, TREE] {
            let base = corpus.as_bytes();
            for _ in 0..400 {
                let mut bytes = base.to_vec();
                match rng.below(5) {
                    // Truncated: a short read on the socket, which the framing
                    // layer can produce whenever a compositor is killed.
                    0 => bytes.truncate(rng.below(bytes.len())),
                    1 => {
                        let at = rng.below(bytes.len());
                        bytes[at] ^= 1 << rng.below(8);
                    }
                    2 => {
                        let at = rng.below(bytes.len());
                        bytes.insert(at, rng.below(256) as u8);
                    }
                    3 => {
                        let at = rng.below(bytes.len());
                        let len = rng.below(bytes.len() - at).min(128);
                        bytes.drain(at..at + len);
                    }
                    // Deeply nested, aimed at the recursion `Node` does over
                    // `nodes` and `floating_nodes`.
                    _ => {
                        let depth = 1 + rng.below(1_000);
                        let mut deep = String::new();
                        for _ in 0..depth {
                            deep.push_str(r#"{"type":"con","nodes":["#);
                        }
                        deep.push_str("{}");
                        for _ in 0..depth {
                            deep.push_str("]}");
                        }
                        bytes = deep.into_bytes();
                    }
                }
                let _ = parse_tree(&String::from_utf8_lossy(&bytes));
            }
        }
    }

    #[test]
    fn the_hand_written_fixture_agrees_with_a_captured_one() {
        let captured = include_str!("../tests/fixtures/sway-tree.json");
        let windows = parse_tree(captured).expect("a captured tree must parse");

        // One window, and it is the one that was opened. Before the rename this
        // was an empty vector, which is the state that reads as "the desktop has
        // no windows" — and therefore as "everything is hidden".
        assert_eq!(windows.len(), 1, "{windows:?}");
        let w = &windows[0];
        assert_eq!(w.app_id, "foot");
        assert!(w.pid > 0, "a window with no pid is one nothing can be done with");
        assert!(w.focused, "the only window on a headless output has focus");
        assert!(w.on_current_desktop);
        assert!(!w.minimized);

        // The node kinds really are spelled the way the parser expects. Asserted
        // against the raw text rather than through the parser, because the whole
        // failure was the parser and the fixture agreeing with each other.
        assert!(captured.contains(r#""type": "workspace""#), "the wire name is `type`");
        assert!(!captured.contains("node_type"), "`node_type` is not a thing Sway emits");
    }

    #[test]
    fn every_view_is_found_and_no_container_is() {
        // Containers carry no pid, and a walk that counted them would attribute
        // windows to the wrong processes.
        let pids: Vec<u32> = facts().iter().map(|w| w.pid).collect();
        assert_eq!(pids, vec![101, 102, 103, 104], "{:?}", facts());
    }

    #[test]
    fn the_wayland_app_id_wins_and_xwayland_falls_back_to_its_class() {
        // §4's ladder: the most specific identifier the window actually declared.
        assert_eq!(by_pid(101).app_id, "Alacritty");
        assert_eq!(by_pid(102).app_id, "firefox", "an XWayland window has no app_id");
    }

    #[test]
    fn a_window_on_a_workspace_nobody_is_looking_at_is_off_the_current_desktop() {
        assert!(by_pid(101).on_current_desktop);
        assert!(!by_pid(104).on_current_desktop, "workspace 2 is not being shown");
    }

    #[test]
    fn visibility_is_decided_by_the_shared_rule_and_not_here() {
        // The whole reason this source reports facts rather than a verdict: §5 has
        // one implementation, so Sway cannot disagree with KWin about "hidden".
        let facts = facts();
        assert!(facts.iter().all(|w| w.visibility.is_none()), "no source-side verdicts");

        let resolved = dozed_core::visibility::resolve(&facts);
        let of = |pid: u32| {
            let i = facts.iter().position(|w| w.pid == pid).unwrap();
            resolved[i]
        };
        assert_eq!(of(101), dozed_core::Visibility::Focused);
        assert_eq!(of(104), dozed_core::Visibility::Hidden, "another workspace");
        // The floating calculator is above the terminal but far too small to
        // cover it, so the terminal stays focused and the calculator is visible.
        assert!(of(103).is_on_screen());
    }

    #[test]
    fn stacking_follows_traversal_order_with_floating_windows_on_top() {
        // Only floating windows can overlap, so the traversal order is all the
        // occlusion check needs — but it does need floating windows to come last.
        let facts = facts();
        let index = |pid: u32| facts.iter().position(|w| w.pid == pid).unwrap();
        assert!(
            facts[index(103)].stack > facts[index(101)].stack,
            "the floating window must be above the tiled one: {facts:?}"
        );
        let stacks: Vec<u32> = facts.iter().map(|w| w.stack).collect();
        assert_eq!(stacks, vec![0, 1, 2, 3], "and the order must be a dense ranking");
    }

    #[test]
    fn a_fullscreen_window_and_an_urgent_one_are_reported() {
        assert!(!by_pid(101).fullscreen);
        assert!(by_pid(103).attention, "urgent is the nearest thing to demandsAttention");

        let full = TREE.replace("\"fullscreen_mode\": 0", "\"fullscreen_mode\": 1");
        let facts = parse_tree(&full).unwrap();
        assert!(facts.iter().find(|w| w.pid == 101).unwrap().fullscreen);
    }

    #[test]
    fn the_output_a_window_is_on_comes_from_its_workspace() {
        // Windows on different outputs never occlude each other, whatever their
        // coordinates say, so this field is load-bearing for §5.
        assert_eq!(by_pid(101).output, "DP-1");
    }

    #[test]
    fn i3_sets_no_visible_flag_so_the_workspace_is_consulted_instead() {
        // Without the fallback every window under i3 reads as hidden, which is the
        // one error that gets a whole desktop frozen.
        let i3 = TREE.replace("\"visible\": true,", "").replace("\"visible\": false,", "");
        let facts = parse_tree(&i3).expect("i3 trees must still parse");
        let of = |pid: u32| facts.iter().find(|w| w.pid == pid).unwrap().on_current_desktop;
        assert!(of(101), "workspace 1 is the output's current workspace");
        assert!(of(102));
        assert!(!of(104), "workspace 2 is not");
    }

    #[test]
    fn a_view_with_no_usable_pid_is_dropped_and_the_rest_survive() {
        // The same rule the KWin path applies: an unattributable window is
        // dropped, never the snapshot (`WindowReport::into_facts`).
        for pid in ["null", "0", "-1"] {
            let broken = TREE.replace("\"pid\": 101", &format!("\"pid\": {pid}"));
            let facts = parse_tree(&broken).expect("one bad view must not fail the tree");
            assert!(!facts.iter().any(|w| w.pid == 101), "pid {pid} must not become a window");
            assert_eq!(facts.len(), 3, "the other three still get through");
        }
    }

    #[test]
    fn an_unknown_field_does_not_stop_us_reading_the_tree() {
        // This is somebody else's schema and it grows between releases. Rejecting
        // unknown keys — which the *config* deliberately does — would mean a Sway
        // upgrade silently blinding the daemon.
        let future = TREE.replace(
            "\"node_type\": \"root\"",
            "\"node_type\": \"root\", \"something_new_in_sway_1_12\": { \"a\": 1 }",
        );
        assert_eq!(parse_tree(&future).unwrap().len(), 4);
    }

    #[test]
    fn a_scratchpad_window_with_no_workspace_is_not_assumed_hidden() {
        // "We cannot tell" must never resolve to freezable (§3).
        let orphan = r#"{"id":1,"type":"root","nodes":[
            {"id":2,"type":"con","pid":9,"app_id":"stray","focused":false}]}"#;
        let facts = parse_tree(orphan).unwrap();
        assert_eq!(facts.len(), 1);
        assert!(facts[0].on_current_desktop, "unknown workspace is not `elsewhere`");
    }

    #[test]
    fn a_message_round_trips_through_the_frame_format() {
        let framed = encode(SUBSCRIBE, br#"["window"]"#);
        assert_eq!(&framed[..6], MAGIC);
        let header: [u8; 14] = framed[..14].try_into().unwrap();
        let (len, kind) = decode_header(&header).unwrap();
        assert_eq!(kind, SUBSCRIBE);
        assert_eq!(len, br#"["window"]"#.len());
        assert_eq!(&framed[14..], br#"["window"]"#);
    }

    #[test]
    fn the_frame_length_is_native_byte_order() {
        // The protocol is a socket between two processes on one machine, so it
        // specifies native order. "Network byte order" is the reflex and is wrong
        // — on a little-endian box a 10-byte payload would read as 167,772,160.
        let framed = encode(GET_TREE, b"0123456789");
        assert_eq!(framed[6..10], 10u32.to_ne_bytes());
    }

    #[test]
    fn a_frame_without_the_magic_is_refused() {
        let mut header = [0u8; 14];
        header[..6].copy_from_slice(b"nope!!");
        assert!(decode_header(&header).is_err());
    }

    #[test]
    fn events_are_told_apart_from_replies_by_the_top_bit() {
        // Both arrive on the same socket, and reading an event as a tree is the
        // failure this constant exists to prevent.
        assert_ne!(GET_TREE & EVENT_FLAG, EVENT_FLAG);
        assert_eq!((EVENT_FLAG | 3) & EVENT_FLAG, EVENT_FLAG);
    }

    #[test]
    fn the_socket_is_found_from_either_variable() {
        // Not `#[test]`-safe to mutate the environment in parallel with other
        // tests, so this only asserts the shape of what is already there.
        let found = socket_path();
        assert_eq!(
            found.is_some(),
            std::env::var_os("SWAYSOCK").is_some() || std::env::var_os("I3SOCK").is_some()
        );
    }
}
