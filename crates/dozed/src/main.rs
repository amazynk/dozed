//! The dozed daemon.
//!
//! Dry-run is the default and live mode must be asked for explicitly on the
//! command line — there is no way to enter it by accident (`DESIGN.md` §3).

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::mpsc;

mod activation;
mod config;
mod daemon;
mod exemptions;
mod icon;
mod identity;
mod ipcserver;
mod journalfile;
mod notify;
mod paths;
mod sampler;
mod session;
mod sources;
mod sway;
mod tracefile;
mod tray;
mod x11;

#[derive(Debug, Parser)]
#[command(
    name = "dozed",
    version,
    about = "Freeze backgrounded apps so they stop waking the CPU",
    long_about = "Freeze backgrounded desktop applications so they stop waking the CPU, and \
        thaw them the instant you switch back.\n\n\
        Unprivileged and same-UID only: it can act on your own processes and nothing else. \
        Dry-run is the default and live mode must be asked for explicitly on the command \
        line, so there is no way to enter it by accident.\n\n\
        This is the daemon; it is normally started by its systemd user unit rather than by \
        hand. `dozectl` is how you ask it what it is doing."
)]
struct Args {
    /// Record what would happen and touch nothing. The default.
    #[arg(long, conflicts_with = "live")]
    dry_run: bool,

    /// Actually freeze and throttle applications.
    #[arg(long)]
    live: bool,

    /// Config file to use instead of ~/.config/dozed/config.toml.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Write a starter config file and exit. Overwrites the file at that path.
    #[arg(long)]
    write_default_config: bool,

    /// Rewrite an existing config with the current explanations, and exit.
    ///
    /// Keeps every value it already has — this changes the comments and nothing
    /// else, which is what makes it safe to run after an upgrade that documents
    /// a new field. The original is kept alongside as `.bak-preannotate`.
    #[arg(long, conflicts_with = "write_default_config")]
    annotate_config: bool,

    /// Check the config, report what it would do, and exit.
    #[arg(long)]
    check: bool,

    /// Do not record this session.
    ///
    /// Recording is on by default: without a trace, a surprising decision can
    /// only be reasoned about, never reproduced (`DESIGN.md` §14).
    #[arg(long)]
    no_trace: bool,

    /// Where to record, instead of ~/.local/state/dozed/traces.
    #[arg(long, value_name = "DIR", conflicts_with = "no_trace")]
    trace_dir: Option<PathBuf>,

    /// Which focus source to run, instead of detecting one.
    ///
    /// Detection reads `XDG_SESSION_TYPE`, `WAYLAND_DISPLAY`, `DISPLAY` and
    /// `XDG_CURRENT_DESKTOP`. The override is for testing, and for a session
    /// those variables describe badly.
    #[arg(long, value_enum, default_value_t = session::FocusSourceKind::Auto)]
    focus_source: session::FocusSourceKind,

    /// Print this manual as roff, for `man`, and exit.
    ///
    /// Generated from the parser rather than kept beside it, so it cannot
    /// describe a flag this binary does not have. `packaging/install.sh` writes
    /// the output where `man` will find it.
    #[arg(long, hide = true)]
    man: bool,
}

/// Nothing this daemon writes is anybody else's business.
///
/// Set once, before the first file is created, because *everything* under
/// `paths::state_dir` is a record of which applications this user runs and when:
/// the journal is a line per decision, a trace is the same at full resolution, and
/// `frozen.json` and `trial.json` name apps too. They were being created at
/// whatever the umask allowed — `0664` under the `0002` this machine ships, so
/// world-readable in a world-traversable `$HOME`, for the life of the file and not
/// just the life of a connection.
///
/// A umask rather than a mode at each of the dozen `fs::write` calls that produce
/// them: `fs::write` takes no mode, so per-site control means `OpenOptions`
/// everywhere, and one site added later without it would be a quiet regression.
/// This cannot be forgotten by a new writer. The two largest records get an
/// explicit mode as well — see `journalfile` and `tracefile` — so that guarantee
/// does not rest on a process-wide setting alone.
///
/// `0o077` clears group and other and leaves the owner alone, which is what the
/// daemon wants for every path it touches, config included.
fn own_files_only() {
    rustix::process::umask(rustix::fs::Mode::from_bits_truncate(0o077));
}

#[tokio::main]
async fn main() -> Result<()> {
    own_files_only();
    // Which `/proc` file the wakeup counter is read out of, decided once against
    // this kernel. Half the sweep cost where `schedstat` is maintained, and this
    // is the only place that can tell — see `sampler::WakeupCounter`. Probed
    // before anything reads a counter, and after enough has happened that our own
    // task has certainly been scheduled.
    let wakeup_counter = sampler::WakeupCounter::probe();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dozed=info,warn".into()),
        )
        .with_target(false)
        .init();

    let args = Args::parse();

    if args.man {
        clap_mangen::Man::new(<Args as clap::CommandFactory>::command())
            .render(&mut std::io::stdout())
            .context("writing the manual to stdout")?;
        return Ok(());
    }

    if args.write_default_config {
        let path = args.config.unwrap_or_else(paths::config_file);
        config::write_default(&path)?;
        println!("wrote {}", path.display());
        return Ok(());
    }

    if args.annotate_config {
        let path = args.config.clone().unwrap_or_else(paths::config_file);
        if !path.exists() {
            anyhow::bail!(
                "no config at {} to annotate; `--write-default-config` writes a fresh one",
                path.display()
            );
        }
        let backup = config::annotate_in_place(&path)?;
        println!("annotated {} (values unchanged)", path.display());
        println!("original at {}", backup.display());
        return Ok(());
    }

    let (cfg, cfg_path) = config::load(args.config.as_ref())?;
    if args.check {
        println!(
            "config ok: {} rule{}, mode {}",
            cfg.rules.len(),
            if cfg.rules.len() == 1 { "" } else { "s" },
            cfg.mode.as_str()
        );
        for rule in &cfg.rules {
            println!("  {:>4}  {:<20} {:?}", rule.priority, rule.name, rule.action);
        }
        return Ok(());
    }

    // The command line wins over the file, and only `--live` grants actuation.
    let live = args.live || (!args.dry_run && cfg.mode.acts());
    if live {
        tracing::warn!("LIVE mode: applications will actually be frozen");
    } else {
        tracing::info!("dry-run: recording what would happen, touching nothing");
    }

    // Said out loud, because the fallback is twice the cost of the fast path and
    // the fast path is the one that can be unavailable. A machine paying double
    // should be able to find out why without reading this file.
    match wakeup_counter {
        sampler::WakeupCounter::Schedstat => {
            tracing::info!("wakeups from /proc/<tid>/schedstat");
        }
        sampler::WakeupCounter::Status => tracing::info!(
            "wakeups from /proc/<tid>/status; this kernel does not maintain schedstat's \
             pcount, so the per-thread sweep costs about twice what it would otherwise"
        ),
    }

    let (tx, rx) = mpsc::channel(256);
    let (cmd_tx, cmd_rx) = mpsc::channel(64);

    // Before anything is opened for writing, and after the early-exit commands
    // above: `--check` and `--man` have no business re-permissioning anybody's
    // files. `own_files_only` covers what this run creates; this covers what
    // earlier runs left behind, which is where the record actually is.
    paths::tighten_state();

    let journal = journalfile::JournalFile::open(paths::journal_file());
    tracing::info!(path = %journal.path().display(), "journal");

    // A token unique to this run, so the compositor helper can detect a restart
    // it would otherwise sleep through. The trace carries it too, so a recording
    // can be lined up against the helper's own view of the same session.
    let instance = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis())
    );
    let wall_start = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());

    let trace = if args.no_trace {
        tracing::info!("not recording this session (--no-trace)");
        tracefile::TraceFile::disabled()
    } else {
        let dir = args.trace_dir.clone().unwrap_or_else(paths::trace_dir);
        let header = dozed_core::TraceHeader {
            version: dozed_core::TRACE_VERSION,
            dozed_version: env!("CARGO_PKG_VERSION").to_owned(),
            instance: instance.clone(),
            wall_start,
            live,
            config: Box::new(cfg.clone()),
        };
        let t = tracefile::TraceFile::create(&dir, &header);
        if t.is_recording() {
            tracing::info!(path = %t.path().display(), "recording this session");
        }
        t
    };

    let mut daemon = daemon::Daemon::new(cfg, live, journal, trace, cfg_path);

    // Claim the bus name and serve the focus sink before anything else: the
    // compositor helper polls for us and republishes as soon as we appear.
    //
    // This also brings up the watchers that read the session bus, and keeps the
    // whole group alive: they share one connection, and when that connection went
    // deaf they all stopped together while the daemon carried on believing the
    // compositor had abandoned it (`sources::serve_focus_sink`).
    // Seeded from the config rather than from `Default`, so the audio watcher
    // never starts under settings the user replaced; the daemon keeps the sending
    // half so a reload reaches it (`daemon::Daemon::publish_audio_config`).
    let (audio_tx, audio_rx) = tokio::sync::watch::channel(daemon.world().config.audio);
    let bus_probe = sources::serve_focus_sink(tx.clone(), instance, audio_rx)
        .await
        .context("claiming org.dozed.Daemon on the session bus")?;
    daemon.set_bus_probe(bus_probe);
    daemon.set_audio_config(audio_tx);
    tracing::info!("serving org.dozed.FocusSink1; waiting for the compositor helper");

    // UPower and logind are system-bus services; the focus sink is not.
    match sources::system_bus().await {
        Ok(system) => {
            daemon.set_battery_wh(sources::battery_wh(&system).await);
            // Shared with the daemon so `dozectl status` can report what the
            // system-bus watcher costs — on an idle desktop it is most of what
            // dozed does (§13).
            let bus_calls = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            daemon.set_bus_counter(bus_calls.clone());
            // logind is also where idle inhibitors are enumerable.
            tokio::spawn(exemptions::watch_inhibitors(system.clone(), tx.clone()));
            tokio::spawn(sources::watch_system(system, tx.clone(), bus_calls));
        }
        Err(e) => {
            // Without power state we cannot know if we are on battery, and the
            // default policy is to do nothing on AC — so this degrades to inert
            // rather than to dangerous.
            tracing::error!(error = %e, "no system bus: power and idle state unavailable");
        }
    }

    // The session-bus watchers — audio, bus names, screencast and activation —
    // are spawned by `serve_focus_sink` above, with the connection they ride, so
    // that a rebuild brings all of them back rather than only the focus sink
    // (`sources::spawn_consumers`).

    // Which of the four session types this is, and therefore where window
    // reports will come from. A wrong guess here is expensive — the X11 source
    // under Wayland sees only XWayland clients, and "no windows" reads as
    // "everything is hidden" — so the reasoning is logged and reportable.
    let session = session::resolve(&session::Env::from_process(), args.focus_source);
    tracing::info!(
        source = %session.kind,
        desktop = %session.desktop,
        "{}",
        session.startup_message()
    );
    daemon.set_session(session.clone());
    match session.kind {
        session::FocusSourceKind::X11 => x11::spawn(tx.clone()),
        session::FocusSourceKind::Sway => sway::spawn(tx.clone()),
        // The helper calls in on its own; the sink is already serving.
        session::FocusSourceKind::Helper | session::FocusSourceKind::Auto => {}
        session::FocusSourceKind::None => {
            tracing::warn!("no focus source: staying passive, nothing will be held");
        }
    }

    // The indicator, and the one notification dozed sends on its own. Both are
    // best-effort: a machine with no system tray and no notification daemon is a
    // normal machine, and neither is worth a warning.
    // Seeded from the config rather than from `Default`, so `ui.tray = false`
    // never briefly shows an icon on the way to hiding it.
    let (tray_tx, tray_rx) = tokio::sync::watch::channel(tray::TrayState {
        enabled: daemon.world().config.ui.tray,
        ..tray::TrayState::default()
    });
    daemon.set_tray(tray_tx);
    tokio::spawn(tray::run(tray_rx, cmd_tx.clone()));
    daemon.set_digest(notify::Digest::new(paths::digest_file()));

    let socket = dozed_core::ipc::socket_path();
    let listener = ipcserver::bind(&socket)?;
    tracing::info!(path = %socket.display(), "ipc socket");
    tokio::spawn(ipcserver::serve(listener, cmd_tx, daemon.journal_sender()));

    let result = daemon.run(rx, cmd_rx).await;
    let _ = std::fs::remove_file(&socket);
    result
}
