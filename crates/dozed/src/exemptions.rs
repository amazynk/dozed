//! The exemption sources: the reasons an app must be left alone right now
//! (`DESIGN.md` §10).
//!
//! Until these existed the shipped `audio-exempt`, `inhibitor-exempt` and
//! `dbus-owner-exempt` rules matched on state nobody set, so they could never
//! fire. The safety net was drawn but not connected, and live mode would have
//! frozen a hidden music player.
//!
//! Every source publishes a **complete snapshot of PIDs**, never a delta, for
//! the same reason the compositor helper does: a source that restarts, or a
//! signal that goes missing, resynchronises on the next publish instead of
//! leaving the daemon holding a wrong belief forever. Turning PIDs into
//! [`AppKey`]s is [`ExemptionTracker`]'s job, because only the daemon knows how
//! processes map to apps — and an app counts as exempt when *any* process in its
//! tree is, since a browser's audio comes out of a child.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use futures_util::StreamExt as _;
use tokio::sync::mpsc;
use zbus::fdo::{DBusProxy, MonitoringProxy};
use zbus::names::BusName;
use zbus::{Connection, MatchRule, MessageStream};

use dozed_core::ipc::AudioStream;
use dozed_core::{AppKey, Event, Exemption};

use crate::sources::Incoming;

/// Folds PID-level exemption snapshots into per-app [`Event`]s.
///
/// Pure and clock-free: it holds the last snapshot from each source and the last
/// app-level view it published, and emits only the differences. Repeats are
/// dropped here rather than in the planner so that a source republishing an
/// unchanged snapshot costs nothing downstream.
#[derive(Debug, Default)]
pub struct ExemptionTracker {
    /// The latest PID snapshot per source.
    pids: BTreeMap<Exemption, BTreeSet<u32>>,
    /// The app-level view we last published, so we emit changes, not state.
    published: BTreeMap<Exemption, BTreeSet<AppKey>>,
    /// What each source has said it cannot see (`Incoming::SourceHealth`).
    health: BTreeMap<Exemption, (Option<String>, Option<String>)>,
    /// The audio streams each process holds open, and what was heard on them.
    stream_titles: BTreeMap<u32, Vec<AudioStream>>,
}

impl ExemptionTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace one source's snapshot and emit whatever changed.
    pub fn snapshot(
        &mut self,
        exemption: Exemption,
        pids: BTreeSet<u32>,
        app_pids: &BTreeMap<AppKey, Vec<u32>>,
    ) -> Vec<Event> {
        self.pids.insert(exemption, pids);
        self.diff(exemption, app_pids)
    }

    /// Re-derive every source after the PID map changes.
    ///
    /// Apps come and go, and an app discovered *after* the audio snapshot that
    /// named its PID would otherwise never be marked exempt — a real hazard,
    /// because a media player is usually already playing by the time the
    /// compositor reports its window.
    pub fn reresolve(&mut self, app_pids: &BTreeMap<AppKey, Vec<u32>>) -> Vec<Event> {
        let kinds: Vec<Exemption> = self.pids.keys().copied().collect();
        kinds.into_iter().flat_map(|e| self.diff(e, app_pids)).collect()
    }

    /// Record what a source cannot see, for `dozectl status` and the dashboard.
    ///
    /// A source may report this more than once — a utility that fails, recovers
    /// and fails again — so the latest word wins rather than accumulating.
    pub fn set_health(
        &mut self,
        exemption: Exemption,
        unavailable: Option<String>,
        degraded: Option<String>,
    ) {
        if unavailable.is_none() && degraded.is_none() {
            self.health.remove(&exemption);
        } else {
            self.health.insert(exemption, (unavailable, degraded));
        }
    }

    /// Replace what we know about the streams each process holds open.
    pub fn set_stream_titles(&mut self, streams: BTreeMap<u32, Vec<AudioStream>>) {
        self.stream_titles = streams;
    }

    /// The names of the audio streams one app holds open, for the window that
    /// has to explain why it is dozing something with a stream.
    ///
    /// Attributed through [`owners`], so it lands on the same app the exemption
    /// did — the browser rather than the terminal it was launched from. An app
    /// holding nothing gets an empty list, which is the common case and costs a
    /// lookup.
    ///
    /// Sorted, because this reaches a screen and a list that reorders itself
    /// between refreshes is a list nobody can read.
    #[must_use]
    pub fn titles_for(
        &self,
        key: &AppKey,
        app_pids: &BTreeMap<AppKey, Vec<u32>>,
    ) -> Vec<AudioStream> {
        if self.stream_titles.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<AudioStream> = Vec::new();
        for (pid, streams) in &self.stream_titles {
            if !owners(&BTreeSet::from([*pid]), app_pids).contains(key) {
                continue;
            }
            for s in streams {
                if !out.iter().any(|e| e.name == s.name) {
                    out.push(s.clone());
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Which processes currently hold `exemption`, or `None` if the source has
    /// never reported.
    #[must_use]
    pub fn pids_for(&self, exemption: Exemption) -> Option<&BTreeSet<u32>> {
        self.pids.get(&exemption)
    }

    /// Per-source health for `dozectl status`.
    ///
    /// A source that has never reported is a safety net that is drawn but not
    /// connected — the exact failure these sources were written to fix, and
    /// one that was previously invisible. Two of the eight are reported as
    /// permanently unavailable rather than merely quiet, and any of them may
    /// report itself *degraded* (a missing utility, a sound server that is not
    /// answering), because pretending otherwise is how a user ends up trusting
    /// an exemption that cannot fire.
    #[must_use]
    pub fn status(&self, x11: bool) -> Vec<dozed_core::ipc::ExemptionSource> {
        Exemption::ALL
            .iter()
            .map(|e| {
                let held = self.pids_for(*e);
                let (reported_unavailable, degraded) =
                    self.health.get(e).cloned().unwrap_or((None, None));
                dozed_core::ipc::ExemptionSource {
                    name: e.as_str().to_owned(),
                    reported: held.is_some() || matches!(e, Exemption::Fullscreen),
                    pids: held.map_or(0, BTreeSet::len),
                    // A structural reason wins over a reported one: "not
                    // observable on Wayland" is a fact about the platform, and a
                    // source that also happens to be misconfigured there does not
                    // make it less true.
                    unavailable: unavailable_reason(*e, x11).or(reported_unavailable),
                    degraded,
                }
            })
            .collect()
    }

    fn diff(&mut self, exemption: Exemption, app_pids: &BTreeMap<AppKey, Vec<u32>>) -> Vec<Event> {
        let Some(pids) = self.pids.get(&exemption) else { return Vec::new() };
        let now = owners(pids, app_pids);

        let was = self.published.entry(exemption).or_default();
        let mut events = Vec::new();
        for key in now.difference(was) {
            events.push(Event::ExemptionChanged { app: key.clone(), exemption, active: true });
        }
        for key in was.difference(&now) {
            events.push(Event::ExemptionChanged { app: key.clone(), exemption, active: false });
        }
        *was = now;
        events
    }
}

/// Which apps hold an exemption, given the PIDs that hold it.
///
/// "Any process in the app's tree" is the right rule for a browser, whose audio
/// comes out of a child — but a process tree is not exclusive to one app. An app
/// launched from a terminal is *inside the terminal's tree*, so the plain rule
/// hands the terminal every exemption belonging to everything started from it.
/// Seen in a real session: opening a LibreOffice presentation marked
/// `org.wezfurlong.wezterm` fullscreen for the same 1.05s, because Impress was
/// launched from it and its PID is in the terminal's tree.
///
/// So each exempt PID is attributed to **one** owner: the app with the smallest
/// tree containing it, which is the most specific claim available. Nesting makes
/// that the right answer — an ancestor's tree is a superset of its descendant's,
/// so the innermost app always wins, and a browser still gets the exemption its
/// own child process holds.
///
/// The safety property, and the reason this is not a narrowing of the safety net:
/// every exempt PID that *any* app contains still yields exactly one exempt app.
/// An exemption is never dropped, only attributed — including when the process
/// holding it belongs to an app with no window of its own, where the containing
/// terminal remains the only candidate and therefore keeps it.
///
/// Ties are broken by `Ord` on [`AppKey`] so a replay cannot pick differently.
#[must_use]
fn owners(pids: &BTreeSet<u32>, app_pids: &BTreeMap<AppKey, Vec<u32>>) -> BTreeSet<AppKey> {
    let mut out = BTreeSet::new();
    for pid in pids {
        let owner = app_pids
            .iter()
            .filter(|(_, tree)| tree.contains(pid))
            .min_by_key(|(key, tree)| (tree.len(), (*key).clone()));
        if let Some((key, _)) = owner {
            out.insert(key.clone());
        }
    }
    out
}

/// Why a source cannot report here, when it structurally cannot.
///
/// * **Clipboard.** Solvable on X11 with XFixes `SelectSelectionInput`, and not
///   solvable on Wayland at all — a client cannot observe selection ownership
///   there. So this depends on which focus source is actually running, *not* on
///   whether `WAYLAND_DISPLAY` happens to be set: a daemon started with
///   `--focus-source=x11` from inside a Wayland login shell inherits that
///   variable and would otherwise claim an exemption it is in fact providing.
/// * **Long-running job** is a classification, not a watched signal — it comes
///   from `identity::resolve` and the config's `class` override, so there is no
///   source for it to be connected to.
fn unavailable_reason(exemption: Exemption, x11: bool) -> Option<String> {
    match exemption {
        Exemption::ClipboardOwner if !x11 => {
            Some("needs the X11 source; not observable on Wayland (DESIGN.md §10.7)".to_owned())
        }
        Exemption::LongRunningJob => Some("classified at identity time, not watched".to_owned()),
        _ => None,
    }
}

// ---- audio (MPRIS) -----------------------------------------------------

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";

/// One record from `pactl list sink-inputs` or `pactl list source-outputs`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Stream {
    /// The record number: `Sink Input #52394`. What identifies a stream to the
    /// sound server, and what a stream loses the moment it is recreated.
    index: u32,
    /// The sink it feeds (`Sink: 82826`), which is the thing that can be
    /// listened to. Only present in the playback direction.
    sink: Option<u32>,
    pid: Option<u32>,
    corked: bool,
    muted: bool,
    /// Every channel at zero.
    volume_zero: bool,
    /// `media.name`: what the app calls this stream.
    ///
    /// For a browser this is the tab title, which is the whole reason it is
    /// carried: "firefox is holding a silent audio stream" is a fact about a
    /// process, and the question it prompts is *which tab*. For everything else
    /// it is whatever the app chose — a track name, or something as unhelpful as
    /// "playback" — so it is shown where it helps and never relied on.
    title: Option<String>,
}

impl Stream {
    /// Silent for a reason the sound server already told us, so no measurement
    /// is needed or possible: nothing comes out of a muted stream, and nothing
    /// comes out of one turned all the way down.
    const fn silent_by_declaration(&self) -> bool {
        self.muted || self.volume_zero
    }
}

/// Every stream in one direction, from `pactl list sink-inputs` or
/// `pactl list source-outputs`.
///
/// MPRIS alone is not enough for playback, and a real session proved it: MPlayer
/// played video twice while the audio exemption fired **zero** times all day,
/// because MPlayer implements no MPRIS interface and there were no MPRIS names on
/// the bus at all. Fullscreen happened to save it; a windowed player would have
/// been frozen mid-playback, which is exactly what these sources exist to prevent.
///
/// The server side rather than the app side, so it sees every client whatever it
/// speaks — mplayer, ffplay, aplay, mpv without a script. ALSA's procfs cannot
/// substitute: under PipeWire the device owner is PipeWire, so
/// `/proc/asound/.../status` says who is *mixing*, never who is playing.
///
/// `Corked` is the difference between "has a stream" and "is using it"; a paused
/// player is corked and is a perfectly good freeze candidate. It means the same
/// thing in the capture direction, where a corked source-output is a client that
/// has opened the microphone and stopped reading from it. What it is *not* is
/// the difference between "is using it" and "can be heard" — see [`SilenceMeter`].
///
/// The record header is the only difference between the two directions, so it is
/// a parameter rather than a second parser.
#[must_use]
fn parse_stream_list(text: &str, header: &str) -> Vec<Stream> {
    let mut out = Vec::new();
    let mut cur: Option<Stream> = None;
    // Fields only count once a matching header has been seen. Without this the
    // parser reads any `Corked:`/`application.process.id` pair it passes, so
    // handing it the wrong list still yields that list's last record — the two
    // directions would contaminate each other on a single mistaken argument.
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix(header) {
            out.extend(cur.take());
            cur = Some(Stream {
                index: rest.trim().trim_start_matches('#').parse().unwrap_or(u32::MAX),
                ..Stream::default()
            });
            continue;
        }
        let Some(s) = cur.as_mut() else { continue };
        if let Some(v) = t.strip_prefix("Corked:") {
            s.corked = v.trim() == "yes";
        } else if let Some(v) = t.strip_prefix("Mute:") {
            s.muted = v.trim() == "yes";
        } else if let Some(v) = t.strip_prefix("Sink:") {
            s.sink = v.trim().parse().ok();
        } else if let Some(v) = t.strip_prefix("Volume:") {
            s.volume_zero = all_channels_silent(v);
        } else if let Some(v) = t.strip_prefix("application.process.id = ") {
            s.pid = v.trim().trim_matches('"').parse().ok();
        } else if let Some(v) = t.strip_prefix("media.name = ") {
            let name = v.trim().trim_matches('"').trim();
            if !name.is_empty() {
                s.title = Some(name.to_owned());
            }
        }
    }
    out.extend(cur);
    out
}

/// Whether a `Volume:` line has every channel at zero.
///
/// The line reads `front-left: 65536 / 100% / 0.00 dB,   front-right: …`, so the
/// percentages are the only field worth reading — the raw value is scale-
/// dependent and the dB figure is `-inf` at zero, which is not a number.
/// A line with no percentage at all (the `balance 0.00` continuation, or a
/// format we do not recognise) claims nothing.
#[must_use]
fn all_channels_silent(line: &str) -> bool {
    let mut any = false;
    for token in line.split_whitespace() {
        let Some(pct) = token.trim_end_matches(',').strip_suffix('%') else { continue };
        let Ok(v) = pct.parse::<u32>() else { continue };
        any = true;
        if v != 0 {
            return false;
        }
    }
    any
}

/// The PIDs holding an uncorked stream, whatever it is carrying.
#[must_use]
fn active_pids(streams: &[Stream]) -> BTreeSet<u32> {
    streams.iter().filter(|s| !s.corked).filter_map(|s| s.pid).collect()
}

/// Ask the sound server for one direction's streams right now.
///
/// `None` on any failure, which every caller treats as "no information" rather
/// than "nothing is happening": clearing the belief on a transient error would
/// make a playing player, or a live microphone, freezable.
async fn streams(what: &str, header: &str) -> Option<Vec<Stream>> {
    let out = tokio::process::Command::new("pactl")
        .arg("list")
        .arg(what)
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    out.status.success().then(|| parse_stream_list(&String::from_utf8_lossy(&out.stdout), header))
}

/// Everything currently feeding a sink, corked or not.
async fn playing_streams() -> Option<Vec<Stream>> {
    streams("sink-inputs", "Sink Input #").await
}

/// PIDs holding the microphone.
async fn capturing_streams() -> Option<BTreeSet<u32>> {
    streams("source-outputs", "Source Output #").await.map(|s| active_pids(&s))
}

/// Sink index to monitor-source name, from `pactl list sinks short`.
///
/// The monitor of a sink is named after it, which is a documented PulseAudio
/// convention rather than a guess — and the name is what has to be passed to
/// `parec`, because an index cannot be.
async fn sink_monitors() -> Option<BTreeMap<u32, String>> {
    let out = tokio::process::Command::new("pactl")
        .arg("list")
        .arg("sinks")
        .arg("short")
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    out.status.success().then(|| parse_sinks(&String::from_utf8_lossy(&out.stdout)))
}

#[must_use]
fn parse_sinks(text: &str) -> BTreeMap<u32, String> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            let index: u32 = f.next()?.trim().parse().ok()?;
            let name = f.next()?.trim();
            (!name.is_empty()).then(|| (index, format!("{name}.monitor")))
        })
        .collect()
}

// ---- audio that cannot be heard ----------------------------------------

/// Whether the apps holding an output stream are making any sound with it.
///
/// The gap this closes, measured on this machine: a Firefox with a call tab open
/// held a sink-input reporting `Corked: no` at 100% volume whose peak was
/// **exactly zero** across four seconds. `playing_audio` was true, `audio-exempt`
/// matched, and the browser — the app most worth dozing — was exempt for as long
/// as the tab stayed open. Uncorked says a stream is open. It does not say
/// anything is coming out of it.
///
/// ## Why the sink and not the stream
///
/// PulseAudio can monitor one sink-input directly (`parec --monitor-stream=N`),
/// which would attribute silence per app, and it is not usable here. Measured
/// against PipeWire's Pulse server: an index that no longer exists does not fail,
/// it **falls back to the default source** — verified by watching
/// `pactl list source-outputs` during a probe, where a stale index attached our
/// `parec` to `alsa_input…Mic1`. Passing `-d` alongside it does not help; the
/// fallback wins there too. So a daemon that monitored streams by index would
/// open the user's microphone every time a stream ended a moment before it was
/// asked about, and would then read the room's silence as the app's.
///
/// Sinks are addressed by name, which has no such failure mode, so this listens
/// to a sink's whole mixed output instead. That costs attribution and buys
/// safety, and the direction it loses in is the harmless one:
///
/// * The mix is silent ⇒ **every** stream feeding it is silent. Certain, because
///   a mix is a sum: nothing audible can hide inside a silent one.
/// * The mix is audible ⇒ *something* is playing and we cannot say what, so
///   nothing on that sink is called silent. A silent app sharing a sink with a
///   playing one therefore stays exempt, which costs battery and nothing else.
///
/// ## What it does not hear
///
/// Sound falling entirely between two probe windows. The measurement is a
/// sample, not a recording — `probe_window` out of every `probe_every` — so an
/// app that beeps once a minute and is silent in between reads as silent, and
/// `silence_after` is what keeps the ordinary gaps (between tracks, mid-buffer,
/// a call with nobody talking) from ever getting near it.
///
/// ## One thing it pays for
///
/// A probe is itself a recording client, so `pactl subscribe` reports a
/// source-output appearing and the next pass re-reads the capture list. That
/// costs one `pactl` per probe and nothing else: our own `parec` is long gone by
/// the time the list is read, so the published set is unchanged and the planner
/// is never woken. Worth knowing before wondering why the capture source stirs
/// whenever something is playing.
struct SilenceMeter {
    cfg: dozed_core::config::AudioConfig,
    /// Per uncorked sink-input, how long it has been silent and when that was
    /// last actually confirmed. Absent means audible, or never measured — the
    /// two are the same answer here, and it is the safe one.
    silent_since: BTreeMap<u32, Silence>,
    /// Cleared the first time `parec` cannot be run, so we stop trying and say
    /// so once instead of spawning a process every interval that cannot work.
    usable: bool,
    /// What we last told the daemon about our own health, so a recovery is
    /// reported exactly once and a fault is not repeated every probe.
    degraded: Option<String>,
    /// When the next measurement is due, as a deadline rather than a delay.
    ///
    /// A delay would be starved: this loop is also woken by `pactl subscribe`,
    /// and a busy audio graph restarts every relative timer in the `select!`
    /// each time it fires. A stream that changes twice a second would then be
    /// measured never, which is exactly the stream worth measuring.
    next_probe: Option<tokio::time::Instant>,
}

/// How long one stream has been silent, and how fresh that claim is.
///
/// Two timestamps rather than one, because on a real desktop the sound server
/// re-enumerates its devices under you. Observed on the machine this was written
/// against: the sink carrying a browser's stream was index 82826, then 52418,
/// then 83175, then 83893 over a few minutes of HDMI churn — so a pass whose
/// stream list was read either side of a re-enumeration finds a sink index that
/// is not in the sink list any more and can measure nothing.
///
/// Treating that as *audible* threw away accumulated silence every few minutes
/// and pushed the crossing out well past `silence_after`. Treating it as
/// *silent* would be worse: it would let a stream cross on one measurement
/// followed by a minute of failures. So a pass that cannot measure changes
/// nothing, and `confirmed` is what stops the resulting gap being open-ended —
/// evidence has to be recent as well as old enough.
#[derive(Debug, Clone, Copy)]
struct Silence {
    /// When the run of silence started.
    since: tokio::time::Instant,
    /// The last pass that actually heard it.
    confirmed: tokio::time::Instant,
}

impl SilenceMeter {
    /// How many probe intervals a confirmation stays good for.
    ///
    /// Three, so a single missed pass — a re-enumeration, a sink that did not
    /// deliver — costs nothing, and a source that has genuinely stopped
    /// answering stops holding apps dozed within a minute of settings.
    const STALE_AFTER: u32 = 3;

    fn new(cfg: dozed_core::config::AudioConfig) -> Self {
        Self { cfg, silent_since: BTreeMap::new(), usable: true, degraded: None, next_probe: None }
    }

    /// How long until the next measurement is due.
    ///
    /// Long when there is nothing to measure, which is most of the day: this is
    /// not a timer the daemon runs, it is a timer that runs while something is
    /// playing. With no uncorked stream anywhere, nothing here wakes up at all
    /// (`DESIGN.md` §13).
    fn due_in(&mut self, anything_playing: bool) -> Duration {
        if !self.enabled() || !anything_playing {
            // Nothing open: the next stream to appear starts a fresh interval
            // rather than inheriting a deadline set while it did not exist.
            self.next_probe = None;
            return Duration::from_secs(3600);
        }
        let now = tokio::time::Instant::now();
        let due = *self.next_probe.get_or_insert(now + self.cfg.probe_every.get());
        due.saturating_duration_since(now)
    }

    const fn enabled(&self) -> bool {
        self.cfg.detect_silence && self.usable
    }

    /// Take a measurement and fold it into what we already believe.
    ///
    /// `probe` is false on the passes that were woken by something else — a
    /// player pausing, a stream appearing — where the streams are worth
    /// re-reading and the sinks are not worth listening to again.
    async fn update(&mut self, streams: &[Stream], probe: bool, tx: &mpsc::Sender<Incoming>) {
        self.forget_dead(streams);
        if !self.enabled() || !probe || streams.iter().all(|s| s.corked) {
            return;
        }
        // Booked before the work rather than after it, and unconditionally: a
        // pass that gives up half way — no sink list, no `parec` — must still
        // have moved the deadline, or the next loop asks again immediately and
        // keeps asking.
        self.next_probe = Some(tokio::time::Instant::now() + self.cfg.probe_every.get());

        // Only sinks with something uncorked on them: a suspended sink has
        // nothing to say, and opening its monitor would wake the hardware up to
        // hear it say so.
        let wanted: BTreeSet<u32> =
            streams.iter().filter(|s| !s.corked).filter_map(|s| s.sink).collect();
        let Some(monitors) = sink_monitors().await else {
            self.report(tx, Some("could not list sinks; every stream counts as audible")).await;
            return;
        };

        let mut quiet: BTreeSet<u32> = BTreeSet::new();
        // Which sinks we got an answer about at all. A sink missing from here
        // was not heard to be audible, it was not heard — and the difference is
        // what keeps device churn from resetting an honest run of silence.
        let mut heard: BTreeSet<u32> = BTreeSet::new();
        for sink in wanted {
            let Some(monitor) = monitors.get(&sink) else { continue };
            match sink_peak(monitor, self.cfg.probe_window.get()).await {
                Measured::Peak(peak) if peak <= self.threshold() => {
                    heard.insert(sink);
                    quiet.insert(sink);
                }
                Measured::Peak(_) => {
                    heard.insert(sink);
                }
                // No samples in time. Not silence — a failed read says nothing
                // about the sink, and treating it as quiet would freeze whatever
                // is playing — but not a broken utility either, so it costs this
                // sink this pass and nothing more.
                Measured::Nothing => {
                    tracing::debug!(sink, "no samples from the sink monitor; assuming audible");
                }
                // This one *is* permanent: a binary that cannot be started will
                // not start on the next interval either, and spawning it every
                // fifteen seconds to find that out again is the kind of thing
                // §13 exists to keep out.
                Measured::Unrunnable => {
                    self.usable = false;
                    tracing::warn!(
                        "{}: an app holding a silent audio stream stays exempt, as it did \
                         before this existed",
                        Self::MISSING
                    );
                    self.report(tx, Some(Self::MISSING)).await;
                    return;
                }
            }
        }

        // The one race left by addressing sinks by name: a sink that vanished
        // between the listing and the probe puts `parec` back on the default
        // source, so what we just called silence may be a quiet room. Ruled out
        // by one re-read, and only on the answer that can freeze something.
        if !quiet.is_empty() {
            let claimed = quiet.clone();
            match sink_monitors().await {
                Some(now) => quiet.retain(|sink| now.contains_key(sink)),
                None => quiet.clear(),
            }
            // A sink ruled out here was not heard either — what came back was
            // the default source, which says nothing about this stream. It has
            // to leave `heard` too, or the fold would read it as audible and
            // reset a run of silence on the strength of a reading we just
            // discarded.
            heard.retain(|sink| quiet.contains(sink) || !claimed.contains(sink));
        }

        self.report(tx, None).await;
        self.fold(streams, &quiet, &heard);
    }

    /// Streams that have gone away take their history with them.
    ///
    /// Indices are reused by the sound server, so a dead stream's timer left
    /// behind would hand its accumulated silence to whatever is created next —
    /// and the stream created next, on a machine where one just ended, is very
    /// often the same app about to play something.
    fn forget_dead(&mut self, streams: &[Stream]) {
        let live: BTreeSet<u32> = streams.iter().filter(|s| !s.corked).map(|s| s.index).collect();
        self.silent_since.retain(|index, _| live.contains(index));
    }

    /// Fold one measurement into the per-stream clocks.
    ///
    /// `quiet` is the sinks heard to be silent and `heard` the sinks heard at
    /// all. A stream whose sink is in neither was not measured this pass, and
    /// nothing happens to it — see [`Silence`] for why that is not the same as
    /// hearing something.
    fn fold(&mut self, streams: &[Stream], quiet: &BTreeSet<u32>, heard: &BTreeSet<u32>) {
        self.forget_dead(streams);
        let now = tokio::time::Instant::now();
        for s in streams.iter().filter(|s| !s.corked) {
            // Mute and zero volume come from the listing rather than from a
            // measurement, so they count as heard wherever the stream points.
            let declared = s.silent_by_declaration();
            let silent = declared || s.sink.is_some_and(|sink| quiet.contains(&sink));
            let measured = declared || s.sink.is_some_and(|sink| heard.contains(&sink));
            if silent {
                self.silent_since
                    .entry(s.index)
                    .and_modify(|e| e.confirmed = now)
                    .or_insert(Silence { since: now, confirmed: now });
            } else if measured {
                self.silent_since.remove(&s.index);
            }
        }
    }

    /// How long one stream has been continuously silent, on evidence recent
    /// enough to still mean something.
    fn silent_for(&self, index: u32) -> Option<Duration> {
        let now = tokio::time::Instant::now();
        let e = self.silent_since.get(&index)?;
        let stale = self.cfg.probe_every.get().saturating_mul(Self::STALE_AFTER);
        (now.saturating_duration_since(e.confirmed) <= stale)
            .then(|| now.saturating_duration_since(e.since))
    }

    /// Peak amplitude, as a fraction of full scale, at or below which a window
    /// counts as silent.
    fn threshold(&self) -> f32 {
        10f32.powf(self.cfg.silence_dbfs / 20.0)
    }

    /// The PIDs whose every uncorked stream has been silent for long enough.
    ///
    /// "Every", not "any": an app with two streams, one of them audible, is
    /// playing something. And a PID with no stream at all is not in here, which
    /// is the distinction that keeps `audio-silent` from meaning "quiet" — it
    /// means "holding a stream and not using it".
    fn silent_pids(&self, streams: &[Stream]) -> BTreeSet<u32> {
        let after = self.cfg.silence_after.get();
        let mut holding: BTreeMap<u32, bool> = BTreeMap::new();
        for s in streams.iter().filter(|s| !s.corked) {
            let Some(pid) = s.pid else { continue };
            let quiet = self.silent_for(s.index).is_some_and(|d| d >= after);
            *holding.entry(pid).or_insert(true) &= quiet;
        }
        holding.into_iter().filter(|(_, quiet)| *quiet).map(|(pid, _)| pid).collect()
    }

    fn reconfigure(&mut self, cfg: dozed_core::config::AudioConfig) {
        if cfg == self.cfg {
            return;
        }
        // A knob that was turned back on deserves another go at the utility that
        // failed under the old settings.
        self.usable = true;
        self.silent_since.clear();
        self.next_probe = None;
        self.cfg = cfg;
    }

    const MISSING: &'static str = "could not run `parec` (install pulseaudio-utils, or \
                                   pipewire-pulse); a silent stream still counts as audio";

    /// Say what we cannot see, once, and stop saying it once we can.
    async fn report(&mut self, tx: &mpsc::Sender<Incoming>, why: Option<&str>) {
        let next = why.map(str::to_owned);
        if next == self.degraded {
            return;
        }
        self.degraded.clone_from(&next);
        let _ = tx
            .send(Incoming::SourceHealth {
                exemption: Exemption::AudioSilent,
                unavailable: None,
                degraded: next,
            })
            .await;
    }

    /// The state to publish when measurement is off, so the source is honest
    /// about being switched off rather than looking like one that never fires.
    async fn announce(&mut self, tx: &mpsc::Sender<Incoming>) {
        let why = (!self.cfg.detect_silence)
            .then_some("`audio.detect_silence = false`: an open stream counts as audio");
        let _ = tx
            .send(Incoming::SourceHealth {
                exemption: Exemption::AudioSilent,
                unavailable: why.map(str::to_owned),
                degraded: None,
            })
            .await;
    }
}

/// What one attempt to listen to a sink came back with.
///
/// The two failures are separated because they call for different actions, and
/// conflating them once meant one busy sink switched silence detection off for
/// the rest of the run. Neither is ever silence: reading a failure as zero is the
/// one mistake here that freezes something mid-playback.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Measured {
    /// A peak, 0.0 to 1.0.
    Peak(f32),
    /// `parec` ran and delivered nothing in time. No information about this
    /// sink, this pass, and quite possibly fine on the next one.
    Nothing,
    /// `parec` could not be started at all, which will still be true in fifteen
    /// seconds.
    Unrunnable,
}

/// Listen to one sink's mixed output and return its peak.
///
/// 16 kHz mono is a deliberate downmix rather than the sink's own format: peak
/// detection needs no fidelity, resampling zeros yields zeros, and the format
/// costs 32 KB/s to read instead of nearly 400.
///
/// `--latency-msec` is not a tuning knob here, it is what makes the measurement
/// possible at all. Measured against PipeWire's Pulse server: without it the
/// first sample arrives **more than two seconds** after `parec` starts, in one
/// burst, so every window shorter than that read nothing and every sink looked
/// unmeasurable. Asking for 50 ms fragments brings the first sample back to
/// about 50 ms. The startup allowance below is the belt to that braces — a
/// server that ignores the request still gets answered, just slowly.
async fn sink_peak(monitor: &str, window: Duration) -> Measured {
    use tokio::io::AsyncReadExt as _;

    /// How long to wait for the *first* sample before giving up on a sink.
    const STARTUP: Duration = Duration::from_secs(2);

    let spawned = tokio::process::Command::new("parec")
        .arg(format!("--device={monitor}"))
        .args(["--latency-msec=50", "--format=s16le", "--rate=16000", "--channels=1", "--raw"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let Ok(mut child) = spawned else { return Measured::Unrunnable };
    let Some(mut out) = child.stdout.take() else { return Measured::Unrunnable };

    let mut buf = [0u8; 8192];
    let mut peak = 0i32;
    let mut got = false;
    // A byte left over from a read that split a sample. Dropping it would shift
    // every following sample by one byte and turn silence into noise, which
    // fails safe but would make the whole measurement useless.
    let mut carry: Option<u8> = None;
    let deadline = tokio::time::sleep(window + STARTUP);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => break,
            read = out.read(&mut buf) => match read {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // The window is `window` of *audio*, not of waiting: it
                    // starts when the first sample lands, so a slow connection
                    // shortens nothing.
                    if !got {
                        got = true;
                        deadline.as_mut().reset(tokio::time::Instant::now() + window);
                    }
                    let mut chunk = &buf[..n];
                    if let Some(lo) = carry.take()
                        && let Some((hi, rest)) = chunk.split_first()
                    {
                        peak = peak.max(magnitude(lo, *hi));
                        chunk = rest;
                    }
                    let (pairs, rest) = chunk.as_chunks::<2>();
                    for p in pairs {
                        peak = peak.max(magnitude(p[0], p[1]));
                    }
                    carry = rest.first().copied();
                }
            },
        }
    }
    if got { Measured::Peak(peak as f32 / f32::from(i16::MAX)) } else { Measured::Nothing }
}

/// One little-endian 16-bit sample's distance from zero.
fn magnitude(lo: u8, hi: u8) -> i32 {
    i32::from(i16::from_le_bytes([lo, hi]).saturating_abs())
}

#[zbus::proxy(interface = "org.mpris.MediaPlayer2.Player", assume_defaults = false)]
trait MprisPlayer {
    #[zbus(property)]
    fn playback_status(&self) -> zbus::Result<String>;
}

/// Watch what is playing **and** what is being captured.
///
/// Two exemptions from one task, because they come from one place. `Audio` is
/// MPRIS plus the sound server's playback streams; `Capture` is the sound
/// server's *record* streams plus the camera. Splitting them into two tasks
/// would mean two `pactl subscribe` children, and that subscription is woken by
/// every event in the audio graph whether or not this daemon cares — so the
/// second one would be a permanent cost bought for tidiness. §13 is the reason
/// they share.
///
/// Signal-driven rather than polled: `NameOwnerChanged` catches players starting
/// and stopping, `PropertiesChanged` on the player interface catches play/pause,
/// `pactl subscribe` catches both stream directions, and inotify catches the
/// camera. A slow re-enumeration backs the MPRIS half up, but **only while a
/// player exists** — with no media player on the bus this task is a pure signal
/// wait and costs nothing, which is the common case and the one §13 cares about.
pub async fn watch_audio(
    conn: Connection,
    tx: mpsc::Sender<Incoming>,
    mut cfg: tokio::sync::watch::Receiver<dozed_core::config::AudioConfig>,
) {
    let Ok(dbus) = DBusProxy::new(&conn).await else {
        tracing::error!("no session bus: the audio exemption is not available");
        return;
    };
    let mut owner_changes = match dbus.receive_name_owner_changed().await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "cannot watch MPRIS players");
            return;
        }
    };

    // One match rule for every player's property changes, rather than a
    // subscription per player: players come and go, and per-name subscriptions
    // are how the KWin helper once leaked itself into 1035 wakeups/s.
    let props_rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.DBus.Properties")
        .and_then(|b| b.member("PropertiesChanged"))
        .and_then(|b| b.path(MPRIS_PATH))
        .map(zbus::match_rule::Builder::build);
    let mut props = match props_rule {
        Ok(rule) => zbus::MessageStream::for_match_rule(rule, &conn, Some(16)).await.ok(),
        Err(e) => {
            tracing::warn!(error = %e, "could not build the MPRIS match rule");
            None
        }
    };

    // A second wake source, for players that speak no MPRIS and for the
    // microphone, neither of which the bus knows anything about. `pactl
    // subscribe` is event-driven, so an idle audio graph costs nothing — polling
    // for playback in a battery tool would be the problem it exists to solve.
    let mut streams = StreamWatch::spawn();
    let mut cameras = CameraWatch::spawn();

    // Say what is missing, once, where a user will see it. A source that cannot
    // report is the failure §10 was written about, and the only thing worse than
    // an exemption that cannot fire is one that cannot fire silently.
    let mut sound = SoundServer::new(streams.is_some());
    sound.announce(&tx).await;
    report_camera_health(&tx, cameras.is_some()).await;

    let mut meter = SilenceMeter::new(*cfg.borrow_and_update());
    meter.announce(&tx).await;

    let mut playing = publish_audio(&conn, &dbus, &tx, None, &mut sound, &mut meter, false).await;
    let mut captured = publish_capture(&tx, None, &mut sound, cameras.is_some()).await;

    loop {
        let backstop = if playing.any_players {
            Duration::from_secs(30)
        } else {
            // No players on the bus: nothing to poll for, so wait on signals
            // alone rather than waking up to confirm an empty list.
            Duration::from_secs(3600)
        };
        let probe_in = meter.due_in(!playing.streams.is_empty());

        // Which half to re-read. A camera event says nothing about playback and
        // a sink-input event says nothing about the microphone, so a nudge that
        // republished both would spawn two `pactl` processes where one was
        // asked for.
        let (mut audio, mut capture, mut probe) = (false, false, false);

        tokio::select! {
            Some(sig) = owner_changes.next() => {
                // Only re-enumerate for MPRIS names; the session bus is busy.
                let relevant = sig
                    .args()
                    .map(|a| a.name().as_str().starts_with(MPRIS_PREFIX))
                    .unwrap_or(false);
                if !relevant {
                    continue;
                }
                audio = true;
            }
            Some(_) = async { match &mut props { Some(s) => s.next().await, None => None } } => {
                audio = true;
            }
            Some(nudge) = async {
                match &mut streams {
                    Some(w) => w.next_change().await,
                    None => None,
                }
            } => match nudge {
                Nudge::Audio => audio = true,
                Nudge::Capture => capture = true,
            },
            Some(()) = async {
                match &mut cameras {
                    Some(w) => w.next_change().await,
                    None => None,
                }
            } => capture = true,
            () = tokio::time::sleep(probe_in) => {
                // Time to listen to what is actually coming out. Nothing schedules
                // this when no stream is open, so an idle desktop never reaches it.
                probe = true;
                audio = true;
            }
            Ok(()) = cfg.changed() => {
                meter.reconfigure(*cfg.borrow_and_update());
                meter.announce(&tx).await;
                audio = true;
            }
            () = tokio::time::sleep(backstop) => {
                // The backstop is for the MPRIS half, which is the one with a
                // signal that can go missing. Capture is covered by `pactl
                // subscribe` and inotify, both of which deliver or die.
                audio = true;
            }
            else => return,
        }

        if audio {
            playing =
                publish_audio(&conn, &dbus, &tx, Some(playing), &mut sound, &mut meter, probe)
                    .await;
        }
        if capture {
            captured = publish_capture(&tx, Some(captured), &mut sound, cameras.is_some()).await;
        }
    }
}

/// What woke us, and therefore which exemption needs re-reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Nudge {
    Audio,
    Capture,
}

/// Whether the sound server is answering, and who to tell when it is not.
///
/// The daemon runs two external utilities — this one and `parec`, which
/// [`SilenceMeter`] reports on separately — and this is the whole of what
/// happens when it cannot: the failure travels to `dozectl status` and the
/// dashboard rather than to a log line. Both failure modes are covered, because
/// they are different bugs with the same symptom — `pactl` not installed is a
/// missing package, and `pactl` installed but not answering is a sound server
/// that is not running — and a user who sees neither is left with an exemption
/// that silently cannot fire.
///
/// The two exemptions lose different amounts, so they are told different things.
/// Playback keeps MPRIS, which is most of what a user would notice; the
/// microphone has no second detector at all.
struct SoundServer {
    /// Whether `pactl subscribe` started. False means the binary is missing or
    /// unrunnable, and no read will ever succeed.
    runnable: bool,
    /// Whether the last read succeeded. Starts optimistic so the first failure
    /// is a transition and gets reported.
    answering: bool,
}

impl SoundServer {
    const MISSING: &'static str = "could not run `pactl` (install pulseaudio-utils, or \
                                   pipewire-pulse)";
    const SILENT: &'static str = "`pactl` runs but the sound server is not answering";

    fn new(runnable: bool) -> Self {
        Self { runnable, answering: true }
    }

    /// Whether it is worth asking at all.
    const fn usable(&self) -> bool {
        self.runnable
    }

    /// Report the state we start in, before anything has been read.
    async fn announce(&self, tx: &mpsc::Sender<Incoming>) {
        if self.runnable {
            return;
        }
        tracing::warn!(
            "no `pactl`: playback detection falls back to MPRIS alone, so a player that \
             does not implement it (mplayer, ffplay, aplay) cannot be seen as playing, and \
             the microphone cannot be seen at all"
        );
        Self::publish(tx, Self::MISSING).await;
    }

    async fn failed(&mut self, tx: &mpsc::Sender<Incoming>) {
        if !std::mem::replace(&mut self.answering, false) {
            return;
        }
        tracing::warn!("{}", Self::SILENT);
        Self::publish(tx, Self::SILENT).await;
    }

    /// Stop complaining once it works again. A tool that reports a fault it has
    /// recovered from teaches people to ignore the field it reports it in.
    async fn answered(&mut self, tx: &mpsc::Sender<Incoming>) {
        if std::mem::replace(&mut self.answering, true) {
            return;
        }
        tracing::info!("the sound server is answering again");
        for exemption in [Exemption::Audio, Exemption::Capture] {
            let _ = tx
                .send(Incoming::SourceHealth { exemption, unavailable: None, degraded: None })
                .await;
        }
    }

    async fn publish(tx: &mpsc::Sender<Incoming>, why: &str) {
        for (exemption, lost) in [
            (Exemption::Audio, "MPRIS players only"),
            (Exemption::Capture, "the microphone cannot be seen"),
        ] {
            let _ = tx
                .send(Incoming::SourceHealth {
                    exemption,
                    unavailable: None,
                    degraded: Some(format!("{why}; {lost}")),
                })
                .await;
        }
    }
}

/// And what the camera cannot tell us, which on most desktops is "there is no
/// camera", not "this is broken".
async fn report_camera_health(tx: &mpsc::Sender<Incoming>, watching: bool) {
    if watching {
        return;
    }
    let why = if video_devices().is_empty() {
        "no /dev/video* device on this machine".to_owned()
    } else {
        "could not watch /dev/video*".to_owned()
    };
    tracing::info!(%why, "camera detection is off");
    let _ = tx
        .send(Incoming::SourceHealth {
            exemption: Exemption::Capture,
            unavailable: None,
            degraded: Some(format!("{why}; the camera cannot be seen")),
        })
        .await;
}

/// Publish the union of "holding the microphone" and "holding the camera".
///
/// One snapshot per exemption kind is the tracker's contract, so these two
/// detectors have to be unioned here rather than published separately — the
/// second publisher would otherwise clobber the first, which is the same trap
/// MPRIS and the sink-input reader are already unioned to avoid.
///
/// Either detector failing contributes nothing rather than clearing the set,
/// and that direction is deliberate: a false exemption costs battery, a missed
/// one costs somebody else's meeting.
async fn publish_capture(
    tx: &mpsc::Sender<Incoming>,
    previous: Option<BTreeSet<u32>>,
    sound: &mut SoundServer,
    have_cameras: bool,
) -> BTreeSet<u32> {
    let mut pids = BTreeSet::new();
    if sound.usable() {
        match capturing_streams().await {
            Some(mic) => {
                sound.answered(tx).await;
                pids.extend(mic);
            }
            None => sound.failed(tx).await,
        }
    }
    if have_cameras && let Some(cam) = camera_holders().await {
        pids.extend(cam);
    }

    if previous.as_ref() != Some(&pids) {
        tracing::debug!(pids = pids.len(), "capture exemption changed");
        let _ = tx
            .send(Incoming::Exemptions { exemption: Exemption::Capture, pids: pids.clone() })
            .await;
    }
    pids
}

/// A long-lived `pactl subscribe`, read for playback-stream changes.
struct StreamWatch {
    /// Held so the child is reaped with us rather than outliving the daemon.
    _child: tokio::process::Child,
    lines: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
}

impl StreamWatch {
    fn spawn() -> Option<Self> {
        use std::process::Stdio;
        let mut child = tokio::process::Command::new("pactl")
            .arg("subscribe")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .ok()?;
        let stdout = child.stdout.take()?;
        Some(Self {
            _child: child,
            lines: tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(stdout)),
        })
    }

    /// Resolves when a stream appears, changes or goes away, saying which
    /// direction it was.
    ///
    /// `None` once the stream ends, which parks this branch of the `select!`
    /// rather than spinning on a dead child.
    async fn next_change(&mut self) -> Option<Nudge> {
        loop {
            let line = self.lines.next_line().await.ok().flatten()?;
            // `Event 'new' on sink-input #123`. Devices — plain `sink` and
            // `source` — change for reasons that have nothing to do with an app,
            // and are matched by neither arm below: `source-output` is a client
            // reading a source, `source` on its own is the microphone's volume
            // being nudged.
            if line.contains("sink-input") {
                return Some(Nudge::Audio);
            }
            if line.contains("source-output") {
                return Some(Nudge::Capture);
            }
        }
    }
}

// ---- the camera --------------------------------------------------------

/// Every V4L2 device node, which is what an app opens to use a camera.
///
/// Includes the metadata nodes some drivers expose beside the capture node
/// (a UVC webcam is commonly `video0` and `video1`): opening either is still
/// evidence, and telling them apart would need ioctls this daemon has no
/// business making.
#[must_use]
fn video_devices() -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir("/dev") else { return Vec::new() };
    let mut out: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with("video") && n[5..].chars().all(|c| c.is_ascii_digit())
            })
        })
        .collect();
    // Stable order so a trace replays identically.
    out.sort();
    out
}

/// Which PIDs hold a `/dev/video*` open, by walking `/proc/*/fd`.
///
/// There is no kernel interface that answers "who has this device open" —
/// `fuser` does this same walk — and no signal to subscribe to either, so the
/// scan is the whole mechanism. What keeps it inside §13's budget is *when* it
/// runs: never on a timer, only when inotify says a video device was opened or
/// closed, which on an ordinary day is a handful of times. Measured on a
/// 707-process desktop, one scan is ~24ms of a single core in the slowest
/// plausible implementation.
///
/// Unreadable processes are skipped rather than failing the scan: most of
/// `/proc` belongs to other users and to root, and none of those are apps this
/// daemon may touch anyway.
async fn camera_holders() -> Option<BTreeSet<u32>> {
    tokio::task::spawn_blocking(|| {
        let mut out = BTreeSet::new();
        let entries = std::fs::read_dir("/proc").ok()?;
        for proc in entries.flatten() {
            let Some(pid) = proc.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            let Ok(fds) = std::fs::read_dir(proc.path().join("fd")) else { continue };
            for fd in fds.flatten() {
                if std::fs::read_link(fd.path())
                    .is_ok_and(|t| t.to_string_lossy().starts_with("/dev/video"))
                {
                    out.insert(pid);
                    break;
                }
            }
        }
        Some(out)
    })
    .await
    .ok()
    .flatten()
}

/// inotify on the camera device nodes, so the scan above is never a poll.
///
/// `IN_OPEN` and `IN_CLOSE_*` fire on a device node exactly as they do on a
/// regular file — verified against `/dev/video0` on this machine, which reports
/// `0x20` then `0x10` around an open/close pair. That is what makes camera
/// detection free at idle: no timer, no enumeration, and nothing at all until
/// something actually reaches for the camera.
///
/// `/dev` itself is watched too, so a webcam plugged in after the daemon started
/// is picked up rather than ignored until the next restart.
struct CameraWatch {
    rx: mpsc::Receiver<()>,
}

impl CameraWatch {
    /// `None` when there is no camera to watch, or when inotify is unavailable.
    fn spawn() -> Option<Self> {
        use rustix::fs::inotify;

        let devices = video_devices();
        if devices.is_empty() {
            return None;
        }
        let fd = inotify::init(inotify::CreateFlags::empty()).ok()?;
        let flags = inotify::WatchFlags::OPEN
            | inotify::WatchFlags::CLOSE_WRITE
            | inotify::WatchFlags::CLOSE_NOWRITE;
        let mut watched = 0;
        for dev in &devices {
            if inotify::add_watch(&fd, dev, flags).is_ok() {
                watched += 1;
            }
        }
        if watched == 0 {
            return None;
        }
        // A device appearing later is a webcam being plugged in. Nothing else in
        // /dev churns often enough for this to be a cost.
        let _ = inotify::add_watch(&fd, "/dev", inotify::WatchFlags::CREATE);

        // A dedicated thread parked in `read` rather than a tokio task: the fd is
        // blocking, and a thread asleep in a syscall is the cheapest waiting
        // there is. `spawn_blocking` would hold a pool thread for the life of the
        // daemon, which is what that pool is explicitly not for.
        let (tx, rx) = mpsc::channel(8);
        std::thread::Builder::new()
            .name("dozed-camera".into())
            .spawn(move || {
                let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 4096];
                let mut reader = inotify::Reader::new(&fd, &mut buf);
                let mut known: BTreeSet<std::path::PathBuf> = devices.into_iter().collect();
                loop {
                    let Ok(event) = reader.next() else { return };
                    // A new node in /dev needs a watch of its own before it can
                    // report anything; the event that created it is not one.
                    if let Some(name) = event.file_name() {
                        let name = name.to_string_lossy().into_owned();
                        if !name.starts_with("video") {
                            continue;
                        }
                        let path = std::path::Path::new("/dev").join(&name);
                        if known.insert(path.clone()) {
                            let _ = inotify::add_watch(&fd, &path, flags);
                        }
                        continue;
                    }
                    // The receiver is bounded and every event means the same
                    // thing — "go and look" — so a full channel is already
                    // carrying the message and dropping this one loses nothing.
                    if tx.try_send(()).is_err() && tx.is_closed() {
                        return;
                    }
                }
            })
            .ok()?;
        Some(Self { rx })
    }

    async fn next_change(&mut self) -> Option<()> {
        self.rx.recv().await
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
struct MprisState {
    /// PIDs reporting `Playing` over MPRIS.
    playing: BTreeSet<u32>,
    /// PIDs with an uncorked stream at the sound server, which catches the
    /// players that speak no MPRIS at all.
    streams: BTreeSet<u32>,
    /// PIDs whose every uncorked stream has been measurably silent for long
    /// enough to stop counting ([`SilenceMeter`]).
    silent: BTreeSet<u32>,
    any_players: bool,
    /// Kept so a pass woken by a signal rather than by the probe timer can
    /// re-derive both sets without asking the sound server twice.
    open: Vec<Stream>,
}

/// How long one MPRIS player gets to say whether it is playing.
///
/// This is not defensive tidiness; without it the whole task wedges, and the
/// thing that wedges it is dozed. Observed on a live desktop: dozed froze Brave,
/// Brave owns two `org.mpris.MediaPlayer2.brave.instance*` names, and a frozen
/// process answers no method call — so each `PlaybackStatus` read blocked for
/// the **25-second D-Bus timeout**, twice per pass. Every other job in this task
/// lives in the same `select!` loop, so for as long as a frozen browser owned an
/// MPRIS name there were no silence measurements at all, and `dozectl status`
/// showed `audio-silent` sitting at zero while the sink was measurably silent.
/// Self-reinforcing, too: dozing a browser is what breaks the detector that
/// decides whether to doze a browser.
///
/// A second is generous for a local process that is not frozen. Anything slower
/// is not treated as playing, which is the same direction the rest of this
/// function already fails in — it costs battery, never correctness.
const MPRIS_TIMEOUT: Duration = Duration::from_secs(1);

/// The PID behind one MPRIS name, if that player says it is `Playing`.
///
/// Anything we cannot read is not treated as playing: this exemption widens what
/// we leave alone, so a failure here costs battery, never correctness. The
/// reverse — guessing "playing" — would exempt every broken player forever.
async fn playing_pid(conn: &Connection, dbus: &DBusProxy<'_>, bus: BusName<'_>) -> Option<u32> {
    let pid = dbus.get_connection_unix_process_id(bus.clone()).await.ok()?;
    let player = MprisPlayerProxy::builder(conn)
        .destination(bus)
        .and_then(|b| b.path(MPRIS_PATH))
        .map(zbus::proxy::Builder::build)
        .ok()?
        .await
        .ok()?;
    (player.playback_status().await.as_deref() == Ok("Playing")).then_some(pid)
}

/// Enumerate players, measure what is coming out, and publish both answers.
async fn publish_audio(
    conn: &Connection,
    dbus: &DBusProxy<'_>,
    tx: &mpsc::Sender<Incoming>,
    previous: Option<MprisState>,
    sound: &mut SoundServer,
    meter: &mut SilenceMeter,
    probe: bool,
) -> MprisState {
    let Ok(names) = dbus.list_names().await else {
        // Hold the previous belief rather than clearing it: publishing an empty
        // set on a transient bus error would make a playing player freezable.
        tracing::warn!("could not list bus names; keeping the last audio snapshot");
        return previous.unwrap_or_default();
    };

    let mut state = MprisState::default();
    let players: Vec<&zbus::names::OwnedBusName> =
        names.iter().filter(|n| n.as_str().starts_with(MPRIS_PREFIX)).collect();
    state.any_players = !players.is_empty();
    // Concurrently, and each on a leash. Serial reads here wedged this whole
    // task — see `MPRIS_TIMEOUT`.
    for pid in futures_util::future::join_all(players.into_iter().map(|name| {
        let bus: BusName<'_> = name.as_ref();
        async move {
            tokio::time::timeout(MPRIS_TIMEOUT, playing_pid(conn, dbus, bus)).await.ok().flatten()
        }
    }))
    .await
    .into_iter()
    .flatten()
    {
        state.playing.insert(pid);
    }

    // The union of both detectors, because `ExemptionTracker` holds one snapshot
    // per exemption kind: two sources publishing `Audio` separately would clobber
    // each other, and the one that published second would win.
    //
    // An unreadable sound server contributes nothing rather than clearing the
    // set — same reasoning as the bus error above, and the same direction.
    if sound.usable() {
        match playing_streams().await {
            Some(open) => {
                sound.answered(tx).await;
                state.streams = active_pids(&open);
                state.open = open;
            }
            None => {
                sound.failed(tx).await;
                if let Some(prev) = &previous {
                    state.streams.clone_from(&prev.streams);
                    state.open.clone_from(&prev.open);
                }
            }
        }
    }

    meter.update(&state.open, probe, tx).await;
    state.silent = meter.silent_pids(&state.open);

    // Silence outranks both detectors, MPRIS included. A player reporting
    // `Playing` for a minute while nothing measurable comes out of the sink it
    // feeds is reporting, not playing — and the case this exists for, a browser
    // holding a call tab, is one that will happily claim both.
    let all: BTreeSet<u32> = state
        .playing
        .union(&state.streams)
        .filter(|pid| !state.silent.contains(pid))
        .copied()
        .collect();

    if previous.as_ref() != Some(&state) {
        tracing::debug!(
            mpris = state.playing.len(),
            streams = state.streams.len(),
            silent = state.silent.len(),
            "audio exemption changed"
        );
        let _ = tx.send(Incoming::Exemptions { exemption: Exemption::Audio, pids: all }).await;
        let _ = tx
            .send(Incoming::Exemptions {
                exemption: Exemption::AudioSilent,
                pids: state.silent.clone(),
            })
            .await;
        let _ =
            tx.send(Incoming::AudioStreams { streams: stream_report(&state.open, meter) }).await;
    }
    state
}

/// What each process is holding open, and what has been heard on it.
///
/// Uncorked only, and deduplicated per process: a paused tab is not what anyone
/// is asking about, and a browser that opens two streams under one name should
/// say the name once. Where two share a name the *least* silent of them wins —
/// `None` beats any duration — because an app counts as silent only when every
/// one of its streams is, and a row claiming a minute of silence beside a stream
/// that is playing would be the one misleading line in the panel.
#[must_use]
fn stream_report(open: &[Stream], meter: &SilenceMeter) -> BTreeMap<u32, Vec<AudioStream>> {
    let mut out: BTreeMap<u32, Vec<AudioStream>> = BTreeMap::new();
    for s in open.iter().filter(|s| !s.corked) {
        let (Some(pid), Some(name)) = (s.pid, s.title.as_ref()) else { continue };
        let silent_for = meter.silent_for(s.index);
        let streams = out.entry(pid).or_default();
        match streams.iter_mut().find(|e| &e.name == name) {
            Some(existing) => {
                if silent_for < existing.silent_for {
                    existing.silent_for = silent_for;
                }
            }
            None => streams.push(AudioStream { name: name.clone(), silent_for }),
        }
    }
    out
}

// ---- screen sharing (the desktop portal) -------------------------------

/// The portal interfaces a screencast passes through.
const SCREENCAST_IFACE: &str = "org.freedesktop.portal.ScreenCast";
const SESSION_IFACE: &str = "org.freedesktop.portal.Session";
/// Where the portal exports session objects. The path encodes the client's
/// unique name, which is the whole reason a `Closed` signal can be attributed.
const SESSION_PREFIX: &str = "/org/freedesktop/portal/desktop/session/";

/// Watch the portal for apps that are sharing or recording the screen.
///
/// The hazard is specific and common: you share one window and then work in
/// another, so the shared app is hidden, idle-looking and — to every rule in the
/// config — an excellent freeze candidate. Freezing it does not stop the share,
/// it freezes the *picture*, and the person watching sees a still frame with no
/// way to tell you why.
///
/// ## Why sessions rather than `Start`
///
/// The obvious signal is `ScreenCast.Start`, and it is the wrong one: if the
/// user cancels the picker, `Start` has already been called and there is no
/// second call to say it came to nothing, so an app would stay exempt for the
/// rest of its life after one cancelled share. The portal, by contrast, *closes
/// the session* whether the flow succeeded or was cancelled. So the thing to
/// track is how many screencast sessions a client has open, which is exactly the
/// question "is it sharing".
///
/// ## What this costs
///
/// Nothing at idle, for the reason `crate::activation` measured: `BecomeMonitor`
/// takes match rules and the broker applies them server-side, so the daemon is
/// woken only by messages that are already about screen sharing. Three rules,
/// none of which an ordinary desktop emits at all.
///
/// ## What is not verified
///
/// Unlike most of what this daemon believes, this has not been checked against a
/// live portal — there was none on the machine it was written on. The failure
/// direction is the safe one: a session miscounted as open leaves an app alone
/// that could have been frozen, which costs battery rather than somebody's
/// meeting. `dozectl status` reports the source, so a run where it never fires
/// during a real share is visible rather than assumed.
pub async fn watch_screencast(resolver: Connection, tx: mpsc::Sender<Incoming>) {
    let unavailable = |why: &str| {
        let tx = tx.clone();
        let why = why.to_owned();
        async move {
            tracing::warn!(%why, "screencast exemption unavailable");
            let _ = tx
                .send(Incoming::SourceHealth {
                    exemption: Exemption::ScreenCast,
                    unavailable: Some(why),
                    degraded: None,
                })
                .await;
        }
    };

    let monitor = match Connection::session().await {
        Ok(c) => c,
        Err(e) => return unavailable(&format!("no monitor connection: {e}")).await,
    };

    // Method calls into the ScreenCast interface open sessions; calls into the
    // Session interface and signals out of it close them.
    let mut rules = Vec::new();
    for (kind, iface) in [
        (zbus::message::Type::MethodCall, SCREENCAST_IFACE),
        (zbus::message::Type::MethodCall, SESSION_IFACE),
        (zbus::message::Type::Signal, SESSION_IFACE),
    ] {
        match MatchRule::builder().msg_type(kind).interface(iface) {
            Ok(b) => rules.push(b.build()),
            Err(e) => return unavailable(&format!("could not build a match rule: {e}")).await,
        }
    }

    let proxy = match MonitoringProxy::builder(&monitor)
        .destination("org.freedesktop.DBus")
        .and_then(|b| b.path("/org/freedesktop/DBus"))
    {
        Ok(b) => match b.build().await {
            Ok(p) => p,
            Err(e) => return unavailable(&format!("monitoring interface unavailable: {e}")).await,
        },
        Err(e) => return unavailable(&format!("monitoring interface unavailable: {e}")).await,
    };
    if let Err(e) = proxy.become_monitor(&rules, 0).await {
        return unavailable(&format!("the bus refused BecomeMonitor: {e}")).await;
    }

    let Ok(dbus) = DBusProxy::new(&resolver).await else {
        return unavailable("cannot resolve bus names to PIDs").await;
    };

    tracing::info!("watching the portal for screen sharing");
    let mut sessions = ScreenCastSessions::default();
    // Publish once up front so the source counts as *reported*: "nobody is
    // sharing" is an answer, and a source that has never spoken is indis-
    // tinguishable from one that is broken.
    sessions.publish(&tx, true).await;

    let mut stream = MessageStream::from(monitor);
    while let Some(Ok(msg)) = stream.next().await {
        let header = msg.header();
        let member = header.member().map(|m| m.as_str().to_owned());
        let iface = header.interface().map(|i| i.as_str().to_owned());

        match (header.message_type(), iface.as_deref(), member.as_deref()) {
            // A client asking for a screencast session. `CreateSession` is the
            // only member that opens one; `SelectSources` and `Start` operate on
            // a session that already exists.
            (zbus::message::Type::MethodCall, Some(SCREENCAST_IFACE), Some("CreateSession")) => {
                let Some(sender) = header.sender().map(|s| s.as_str().to_owned()) else { continue };
                let Ok(bus) = BusName::try_from(sender.clone()) else { continue };
                let Ok(pid) = dbus.get_connection_unix_process_id(bus).await else { continue };
                sessions.opened(&sender, pid);
                // Info, not debug: a screencast starting is rare, and this is the
                // one line that answers "did dozed see my recorder at all"
                // without attaching a debugger to somebody's desktop.
                tracing::info!(%sender, pid, "screencast session opened");
                sessions.publish(&tx, false).await;
            }
            // The client hanging up.
            (zbus::message::Type::MethodCall, Some(SESSION_IFACE), Some("Close")) => {
                let Some(sender) = header.sender().map(|s| s.as_str().to_owned()) else { continue };
                if sessions.closed(&sender) {
                    tracing::info!(%sender, "screencast session closed by the client");
                    sessions.publish(&tx, false).await;
                }
            }
            // The portal hanging up, which is also what a cancelled picker looks
            // like. The signal carries no sender we can use — it comes from the
            // portal — so the client is recovered from the object path.
            (zbus::message::Type::Signal, Some(SESSION_IFACE), Some("Closed")) => {
                let Some(owner) = header.path().and_then(|p| session_owner(p.as_str())) else {
                    continue;
                };
                if sessions.closed(&owner) {
                    tracing::info!(sender = %owner, "screencast session closed by the portal");
                    sessions.publish(&tx, false).await;
                }
            }
            _ => {}
        }
    }
    tracing::warn!("the screencast monitor stopped");
}

/// Recover a client's unique bus name from a portal session path.
///
/// The portal builds session paths out of the client's own unique name, with the
/// leading colon dropped and dots turned into underscores — `:1.42` becomes
/// `1_42`. That convention is part of the portal's API rather than an
/// implementation detail: clients rely on it to subscribe to a request before
/// the call that creates it has returned.
#[must_use]
fn session_owner(path: &str) -> Option<String> {
    let rest = path.strip_prefix(SESSION_PREFIX)?;
    let token = rest.split('/').next()?;
    if token.is_empty() {
        return None;
    }
    Some(format!(":{}", token.replace('_', ".")))
}

/// How many screencast sessions each client has open.
#[derive(Debug, Default)]
struct ScreenCastSessions {
    /// Unique bus name → (pid, sessions open).
    open: BTreeMap<String, (u32, u32)>,
    published: Option<BTreeSet<u32>>,
}

impl ScreenCastSessions {
    fn opened(&mut self, sender: &str, pid: u32) {
        let entry = self.open.entry(sender.to_owned()).or_insert((pid, 0));
        entry.0 = pid;
        // Saturating, so a stream of opens can never wrap the counter back to
        // "not sharing" — the one arithmetic error here that would be unsafe.
        entry.1 = entry.1.saturating_add(1);
    }

    /// Returns whether anything changed.
    fn closed(&mut self, sender: &str) -> bool {
        let Some(entry) = self.open.get_mut(sender) else { return false };
        entry.1 = entry.1.saturating_sub(1);
        if entry.1 == 0 {
            self.open.remove(sender);
        }
        true
    }

    /// The PIDs still sharing, dropping any whose process has exited.
    ///
    /// The reason this filter exists rather than a `NameOwnerChanged`
    /// subscription: that signal fires for every name change on a busy session
    /// bus, and subscribing to it to catch the rare case of a screencasting app
    /// exiting without closing its session would cost more wakeups than the
    /// whole source. A `/proc` lookup per sharing client — normally none, at
    /// most one — costs nothing by comparison.
    fn live_pids(&mut self) -> BTreeSet<u32> {
        self.open.retain(|_, (pid, _)| std::path::Path::new(&format!("/proc/{pid}")).exists());
        self.open.values().map(|(pid, _)| *pid).collect()
    }

    async fn publish(&mut self, tx: &mpsc::Sender<Incoming>, force: bool) {
        let pids = self.live_pids();
        if !force && self.published.as_ref() == Some(&pids) {
            return;
        }
        self.published = Some(pids.clone());
        let _ = tx.send(Incoming::Exemptions { exemption: Exemption::ScreenCast, pids }).await;
    }
}

// ---- idle inhibitors (logind) -----------------------------------------

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Login1Inhibitors {
    /// `(what, who, why, mode, uid, pid)` per inhibitor.
    fn list_inhibitors(&self) -> zbus::Result<Vec<(String, String, String, String, u32, u32)>>;
}

/// Whether one logind inhibitor means "keep me awake".
///
/// Measured on this machine, the inhibitors held on an ordinary KDE desktop are
/// almost all the wrong kind:
///
/// ```text
///   what                     who                mode   uid
///   shutdown                 Unattended Upgrades delay     0
///   idle:handle-lid-switch   repowerd            block     0
///   sleep                    NetworkManager      delay     0
///   sleep                    Screen Locker       delay  1000
///   sleep                    signal-desktop      delay  1000   <-- a real app
/// ```
///
/// `signal-desktop` is the case that matters: a `sleep`/`delay` inhibitor means
/// "let me finish before the machine suspends", not "do not freeze me". Treating
/// any inhibitor as an exemption would permanently exempt Signal — precisely the
/// chat app §7 wants throttled. So the filter is `idle` **and** `block`, which is
/// the only combination that actually says "the user is watching something".
///
/// The uid check is `DESIGN.md` §1's same-UID constraint: root's inhibitors are
/// not ours to reason about, and we could not touch those processes anyway.
fn is_idle_inhibitor(what: &str, mode: &str, uid: u32, our_uid: u32) -> bool {
    uid == our_uid && mode == "block" && what.split(':').any(|w| w == "idle")
}

/// Poll logind for idle inhibitors.
///
/// logind emits no signal when the inhibitor list changes — there is no
/// `InhibitorsChanged` — so this is the one exemption source that has to poll.
/// 15s is a compromise: an inhibitor appearing late costs at most one sampling
/// interval of exposure, and one system-bus round trip every 15s is well inside
/// the budget the daemon already spends on UPower.
pub async fn watch_inhibitors(conn: Connection, tx: mpsc::Sender<Incoming>) {
    let Ok(mgr) = Login1InhibitorsProxy::new(&conn).await else {
        tracing::error!("no logind: the inhibitor exemption is not available");
        return;
    };
    let our_uid = rustix::process::getuid().as_raw();
    let mut ticker = tokio::time::interval(Duration::from_secs(15));
    let mut last: Option<BTreeSet<u32>> = None;

    loop {
        ticker.tick().await;
        let Ok(list) = mgr.list_inhibitors().await else {
            // Keep the last belief: clearing on a transient error would make an
            // app that asked to stay awake freezable.
            tracing::warn!("could not list inhibitors; keeping the last snapshot");
            continue;
        };
        let pids: BTreeSet<u32> = list
            .into_iter()
            .filter(|(what, _, _, mode, uid, _)| is_idle_inhibitor(what, mode, *uid, our_uid))
            .map(|(_, _, _, _, _, pid)| pid)
            .collect();

        if last.as_ref() != Some(&pids) {
            last = Some(pids.clone());
            let _ = tx.send(Incoming::Exemptions { exemption: Exemption::Inhibitor, pids }).await;
        }
    }
}

// ---- D-Bus name owners -------------------------------------------------

/// Watch which processes own a well-known name that others can call.
///
/// §10 wants name owners exempt because freezing one blocks every caller for the
/// 25s D-Bus timeout. Measured on this desktop, the naive reading of that —
/// "owns any well-known name" — is far too wide: it catches Firefox, which owns
/// `org.mozilla.firefox.<profile>` once per profile, and Remmina. Exempting the
/// browser would give away most of what the tool is for.
///
/// The distinction that matters is **activatable**. Of 93 owned names, 34 are
/// also in `ListActivatableNames`, meaning a third party can call them and
/// expect something to answer. Every one of those 34 is held by session
/// infrastructure — `gnome-keyring-daemon` (the password manager §10 names),
/// `xdg-desktop-portal`, `gvfs`, `kded6`, `dconf`. Firefox's name is owned but
/// **not** activatable: nobody else calls it, it is Firefox's own
/// single-instance channel.
///
/// So filtering to activatable names protects exactly the population that would
/// block a caller, and costs nothing in apps worth freezing.
///
/// Note this leaves the single-instance launcher hang (§12, unresolved) exactly
/// where it was: clicking Firefox's launcher while Firefox is frozen still goes
/// through its own non-activatable name. That is the activation problem, not
/// this one, and conflating them would have hidden it behind a blanket
/// exemption.
pub async fn watch_bus_names(conn: Connection, tx: mpsc::Sender<Incoming>) {
    let Ok(dbus) = DBusProxy::new(&conn).await else {
        tracing::error!("no session bus: the D-Bus owner exemption is not available");
        return;
    };
    let mut owner_changes = match dbus.receive_name_owner_changed().await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "cannot watch bus name ownership");
            return;
        }
    };

    // Activatable names change only when a .service file is installed, so this
    // is read once rather than maintained.
    let activatable: BTreeSet<String> = match dbus.list_activatable_names().await {
        Ok(names) => names.into_iter().map(|n| n.as_str().to_owned()).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "cannot list activatable names; owner exemption disabled");
            return;
        }
    };

    // name -> pid, maintained incrementally. A full enumeration is ~90 round
    // trips, which is fine once at startup and would not be fine per change.
    let mut owners: BTreeMap<String, u32> = BTreeMap::new();
    if let Ok(names) = dbus.list_names().await {
        for name in names {
            let name = name.as_str().to_owned();
            if !activatable.contains(&name) {
                continue;
            }
            let Ok(bus) = BusName::try_from(name.clone()) else { continue };
            if let Ok(pid) = dbus.get_connection_unix_process_id(bus).await {
                owners.insert(name, pid);
            }
        }
    }
    let mut last = publish_owners(&owners, &tx, None).await;

    while let Some(sig) = owner_changes.next().await {
        let Ok(args) = sig.args() else { continue };
        let name = args.name().as_str().to_owned();
        if !activatable.contains(&name) {
            continue;
        }
        if args.new_owner().is_some() {
            let Ok(bus) = BusName::try_from(name.clone()) else { continue };
            match dbus.get_connection_unix_process_id(bus).await {
                Ok(pid) => {
                    owners.insert(name, pid);
                }
                // It went away again between the signal and the question.
                Err(_) => {
                    owners.remove(&name);
                }
            }
        } else {
            owners.remove(&name);
        }
        last = publish_owners(&owners, &tx, Some(last)).await;
    }
}

async fn publish_owners(
    owners: &BTreeMap<String, u32>,
    tx: &mpsc::Sender<Incoming>,
    previous: Option<BTreeSet<u32>>,
) -> BTreeSet<u32> {
    let pids: BTreeSet<u32> = owners.values().copied().collect();
    if previous.as_ref() != Some(&pids) {
        let _ = tx
            .send(Incoming::Exemptions { exemption: Exemption::DbusNameOwner, pids: pids.clone() })
            .await;
    }
    pids
}

#[cfg(test)]
#[path = "exemptions_tests.rs"]
mod tests;
