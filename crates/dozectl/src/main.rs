//! `dozectl` — ask the daemon what it is doing and why.
//!
//! The design goal for this binary is that `dozectl why slack` always gives a
//! straight answer (`DESIGN.md` §1). Everything it prints comes from the
//! daemon's own decision, not from a reconstruction, so the two cannot disagree.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use dozed_core::coverage::Coverage;
use dozed_core::ipc::{self, Request, Response};
use dozed_core::{Action, Coarse, HumanDuration, IdentityKind, Mode};

mod metrics;
mod paint;
mod traces;

/// Connect to the daemon, and establish that the daemon is *ours*.
///
/// The daemon has always checked its callers — `ipcserver::same_user` refuses a
/// connection from another UID before it reads a byte, on the grounds that this
/// socket can freeze and thaw applications and lists everything the user runs.
/// This is the same check pointed the other way, and it was missing.
///
/// It matters for the reason the daemon's own comment gives: `ipc::socket_path`
/// falls back to `/tmp` when `$XDG_RUNTIME_DIR` is unset — `su`, `cron`, `ssh`
/// without `pam_systemd`, some containers — and in `/tmp` there is nothing to stop
/// another account creating `dozed.sock` first. A client that does not look would
/// then send its requests to whoever got there first and print their answers:
/// every figure in `dozectl status`, `report` and `why` would be theirs to choose.
/// Nothing in a response is executed, so this is misinformation rather than
/// compromise — but a tool whose entire pitch is "it shows you what it did" has no
/// business reporting numbers it cannot attribute.
///
/// `SO_PEERCRED` and not the socket's mode or owner: the kernel fills it in at
/// `connect(2)` from the peer's real credentials, so there is no window between
/// checking and trusting, which is exactly the property a `stat` of the path would
/// not have.
///
/// Root is refused along with everyone else, mirroring the daemon: a root-owned
/// daemon is not one this user started, and quietly accepting it would make the
/// same-UID rule a thing that holds in one direction only.
async fn connect(path: &std::path::Path) -> Result<UnixStream> {
    let stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("no daemon at {} — is dozed running?", path.display()))?;
    let ours = rustix::process::getuid().as_raw();
    match stream.peer_cred() {
        Ok(cred) if cred.uid() == ours => Ok(stream),
        Ok(cred) => bail!(
            "the socket at {} is owned by uid {}, not by you (uid {ours}). That is not this \
             user's dozed, so nothing it says about your applications can be trusted. If \
             $XDG_RUNTIME_DIR is unset this path falls back to /tmp, where any account can \
             claim the name first.",
            path.display(),
            cred.uid(),
        ),
        Err(e) => bail!(
            "could not establish who is serving {}: {e}. Refusing to report figures that \
             cannot be attributed.",
            path.display(),
        ),
    }
}

/// Print to stdout, exiting quietly when the reader has gone away.
///
/// `dozectl watch | head` is an ordinary thing to type, and the `print!` macros
/// panic on it. Rust installs `SIG_IGN` for `SIGPIPE`, so a write to a closed
/// pipe returns `EPIPE` where a C program would have died silently — and the
/// macros unwrap that. What the user saw was a Rust panic and a note about
/// `RUST_BACKTRACE` where every other unix tool prints nothing at all.
///
/// Exiting zero is `head`'s own contract: closing the pipe early is how it says
/// "enough", not a failure either side should report. This is also why the
/// streaming commands must route every write through here — one stray
/// `println!` in a loop brings the panic back.
fn emit(s: &str) {
    use std::io::Write as _;
    if std::io::stdout().write_all(s.as_bytes()).is_err() {
        std::process::exit(0);
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "dozectl",
    version,
    about = "Control and inspect the dozed daemon",
    long_about = "Control and inspect the dozed daemon.\n\n\
        dozed freezes backgrounded desktop applications so they stop waking the CPU, and \
        thaws them the instant you switch back. It starts in dry-run, where it records \
        what it would have done and touches nothing.\n\n\
        The design goal for this program is that `dozectl why <app>` always gives a \
        straight answer, including when the answer is \"nothing happened, and here is \
        which rule was counting down\". Everything it prints comes from the daemon\u{2019}s \
        own decision rather than a reconstruction, so the two cannot disagree.\n\n\
        Start with `dozectl status`, leave it for a day, then read `dozectl report`."
)]
struct Args {
    /// Print raw protocol JSON instead of formatted output.
    #[arg(long, global = true)]
    json: bool,

    /// Never colour the output.
    ///
    /// Colour is already off when stdout is not a terminal, when `NO_COLOR` is
    /// set, and with `--json`, so this is for the case none of those catch: a
    /// terminal whose palette makes the colours worse than none.
    #[arg(long, global = true)]
    no_color: bool,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Daemon health, mode, and what it is holding.
    Status,
    /// Every app the daemon knows about.
    #[command(alias = "ls")]
    Apps,
    /// Why is this app in the state it is in?
    Why {
        app: String,
        /// Also show every rule that did *not* match, and the predicate that
        /// rejected it. This is the flag for "why did my rule not fire?".
        #[arg(long)]
        rules: bool,
    },
    /// Recent decisions, from the current run of the daemon.
    Journal {
        /// How far back to look, e.g. `30m`, `24h`.
        #[arg(long)]
        since: Option<String>,
        /// Keep streaming new entries.
        #[arg(long, short)]
        follow: bool,
        /// Also show entries from previous runs of the daemon.
        ///
        /// The journal survives restarts, so this reaches back further — but
        /// `ELAPSED` counts from the start of whichever run wrote the entry, so
        /// it runs backwards at each boundary. Those are marked.
        #[arg(long)]
        all: bool,
    },
    /// Follow the journal live. Shorthand for `journal --follow`.
    Watch {
        /// Also show entries from previous runs of the daemon.
        #[arg(long)]
        all: bool,
    },
    /// What dozing has saved, or would have saved.
    Report {
        #[arg(long)]
        since: Option<String>,
    },
    /// Switch between dry-run and live.
    Mode { mode: ModeArg },
    /// Stop acting for a while, then carry on as before.
    ///
    /// For the hour you are presenting, running a build, or on a call you would
    /// rather not have dozed interrupt. It thaws everything on the way in and
    /// puts the mode back on its own when the timer runs out — which is the
    /// difference between this and `dozectl mode dry-run`, where putting it back
    /// is your job and nobody ever remembers.
    Snooze {
        /// How long, e.g. `30m`, `2h`. Omit with `--off` to end one early.
        duration: Option<String>,
        /// End the current snooze now and put the mode back.
        #[arg(long, conflicts_with = "duration")]
        off: bool,
    },
    /// Freeze an app now, ignoring the rules.
    Freeze { app: String },
    /// Throttle an app to a percentage of one core.
    Throttle { app: String, pct: u8 },
    /// Restore one app.
    Restore { app: String },
    /// Leave an app alone for a while — dozed will not act on it at all.
    ///
    /// Temporary on purpose. For a permanent exemption write a `never` rule or
    /// an `[apps]` override, where it is written down and can be found again.
    Ease {
        app: String,
        /// How long, e.g. `30m`, `2h`. Omit for "until cleared".
        #[arg(long = "for", value_name = "DURATION")]
        for_: Option<String>,
    },
    /// Cap an app instead of freezing it, for a while.
    ///
    /// The gentle rung of the ladder (`DESIGN.md` §7): it keeps running and
    /// keeps its connections, it is just slower. Without `--pct` the rules pick
    /// the number, which is usually better than a guess — theirs came from a
    /// measurement.
    Cap {
        app: String,
        /// Percentage of one core. Omit to let the rules choose.
        #[arg(long)]
        pct: Option<u8>,
        /// How long, e.g. `30m`, `2h`. Omit for "until cleared".
        #[arg(long = "for", value_name = "DURATION")]
        for_: Option<String>,
    },
    /// Drop a limit set by `ease` or `cap`, and give the app back to the rules.
    Unhold { app: String },
    /// Panic thaw: restore everything, ignore rules, ignore mode.
    Thaw {
        /// Required, so a bare `dozectl thaw` cannot be a typo for something else.
        #[arg(long)]
        all: bool,
    },
    /// Re-read the config file.
    Reload,

    /// Offer rules this version ships that your config does not have.
    ///
    /// The shipped rules are a starter kit: the installer writes them once and
    /// never touches the file again, so a safety rule added after you installed
    /// reaches new installs only — and the people it was written for are the
    /// ones already running dozed.
    ///
    /// This only ever adds, matches by rule name, and never reorders or
    /// renumbers anything. It prints what it would do and changes nothing
    /// without `--write`; with it, the original is kept alongside as a backup
    /// and the result is validated before anything is written.
    ///
    /// A rule you deleted on purpose looks exactly like one that predates the
    /// version which added it — nothing on disk records the difference — which
    /// is why this asks rather than migrating on upgrade.
    UpgradePresets {
        /// Config to read. Defaults to `~/.config/dozed/config.toml`.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Append the missing rules, rather than only listing them.
        #[arg(long)]
        write: bool,
    },

    /// Print a shell completion script for `dozectl`.
    ///
    /// Generated from the same `clap` definition the binary parses with, so a
    /// subcommand cannot exist without its completion or the other way round —
    /// which is the whole reason nothing is checked in. `packaging/install.sh`
    /// writes the output where your shell will find it.
    ///
    ///   dozectl completions bash > ~/.local/share/bash-completion/completions/dozectl
    #[command(hide = true)]
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },

    /// Print this manual as roff, for `man`.
    ///
    /// Same argument as `completions`: generated from the parser, so it cannot
    /// describe a flag the binary does not have.
    ///
    ///   dozectl man > ~/.local/share/man/man1/dozectl.1
    #[command(hide = true)]
    Man {
        /// Write one page per subcommand into this directory instead.
        ///
        /// The top-level page cross-references `dozectl-status(1)` and the rest,
        /// so shipping it alone leaves every one of those references pointing at
        /// nothing. This writes them all.
        #[arg(long, value_name = "DIR")]
        to: Option<PathBuf>,
    },

    /// Read a recorded session: every event, derivation and intent, in order.
    ///
    /// This is the log to read when a decision needs explaining. The journal
    /// records what `dozed` did; a trace records what it saw and what it
    /// concluded, including the conclusions that led to no action at all.
    Trace {
        /// Trace file. Defaults to the most recent.
        trace: Option<PathBuf>,
        /// Only records concerning this app.
        #[arg(long)]
        app: Option<String>,
        /// Drop raw events and show only derivations and intents.
        #[arg(long)]
        decisions: bool,
        /// List the recorded sessions and exit.
        #[arg(long)]
        list: bool,
    },

    /// Re-run a recorded session against a different ruleset (`DESIGN.md` §14).
    ///
    /// Tune against your own recorded day instead of guessing: this diffs the
    /// intents a new config would have produced against the ones that were
    /// actually recorded.
    Replay {
        /// Trace file. Defaults to the most recent.
        trace: Option<PathBuf>,
        /// Ruleset to test. Defaults to the one the session ran under, which
        /// must reproduce the recording exactly.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Print the whole resulting intent stream, not only what changed.
        #[arg(long)]
        verbose: bool,
    },

    /// Search a recorded session for a better config than the one it ran under.
    ///
    /// `replay` answers "what would this config have done to my day". This answers
    /// "what config should I be running", by trying a few dozen variations of your
    /// own and scoring each against the recording.
    ///
    /// Candidates are scored on two numbers: wakeups avoided, and the times you
    /// switched to an app that was held and had to wait for it. Nothing is
    /// recommended that interrupts you more than your current config already does.
    ///
    /// Needs a recording in which nothing was actually held — a daemon started
    /// `--dry-run`. An app that really was frozen reads as silent for exactly the
    /// stretches it was held, leaving nothing to measure a saving against.
    Suggest {
        /// Trace file. Defaults to the most recent.
        trace: Option<PathBuf>,
        /// Config to tune. Defaults to the one the session ran under.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Write the recommended config here.
        #[arg(long, value_name = "PATH")]
        write: Option<PathBuf>,
        /// Recommend from a session that really froze things anyway.
        ///
        /// An app that was held reads as silent for exactly the stretches it was
        /// held, so a candidate can only be credited for acting sooner than this
        /// session did. The ranking that comes out rewards aggression and cannot
        /// score patience at all. Record a day with a daemon started `--dry-run`
        /// instead; this flag is for when you know that and want the numbers
        /// regardless.
        #[arg(long)]
        anyway: bool,
    },

    /// Write the current state as Prometheus metrics.
    ///
    /// For the week of dry-run §14 asks for: a report read once is an anecdote,
    /// and a graph of the same numbers over a week is where a pattern — a rule
    /// that fires every evening, an app that never settles — is actually visible.
    ///
    /// Made for `node_exporter`'s textfile collector, which reads a directory of
    /// `.prom` files on its own schedule:
    ///
    ///   */5 * * * * dozectl export > /var/lib/node_exporter/dozed.prom.tmp \
    ///               && mv /var/lib/node_exporter/dozed.prom{.tmp,}
    ///
    /// The rename matters: the collector may read at any moment, and a
    /// half-written file parses as garbage.
    Export,

    /// Measure whether dozing actually saves anything, by experiment.
    ///
    /// `report` gives a modelled saving: CPU-seconds times a guess at watts. This
    /// measures the battery instead, by alternating live and dry-run blocks in
    /// randomised pairs and comparing the discharge — and it is allowed to come
    /// back with "no measurable difference".
    ///
    /// Runs only while the desktop is idle and on battery, so it needs an evening
    /// rather than a coffee break. Requires a daemon started with --live.
    Trial {
        #[command(subcommand)]
        action: Option<TrialCmd>,
    },
}

#[derive(Debug, clap::Subcommand)]
enum TrialCmd {
    /// Start measuring. Prints nothing more until there is something to say.
    Start {
        /// Live/dry-run pairs to collect.
        ///
        /// Six is the floor, not a preference: the smallest p-value a sign-flip
        /// test can return from n pairs is 2/2^n, which first clears 0.05 at
        /// n = 6. Fewer than that cannot conclude anything however clean the data.
        #[arg(long)]
        pairs: Option<u32>,
        /// How long each block runs, e.g. `20m`.
        #[arg(long)]
        block: Option<String>,
        /// Fix the randomisation, to reproduce a previous trial exactly.
        #[arg(long)]
        seed: Option<u64>,
        /// Measure on mains too, judging on CPU package energy and deep-idle
        /// residency instead of on battery discharge.
        ///
        /// For a desktop or a docked laptop, which is the machine dozed gains
        /// most on over a week and the one that could never prove it: `power_now`
        /// reads nothing while the wall is supplying the power.
        ///
        /// Where the CPU's own energy counter is readable — see `README.md` on
        /// RAPL, which is `0400 root` on a stock kernel — this measures the
        /// processor's draw instead. That is a narrower claim than discharge and
        /// is reported as one: it is watts off the CPU, where dozing acts, and
        /// says nothing about the display, the radios or the drives.
        ///
        /// It refuses outright on a package pinned to its firmware power limit,
        /// where freezing an app hands the freed budget to whatever else is
        /// runnable rather than lowering the draw.
        #[arg(long)]
        on_mains: bool,
    },
    /// Stop early, keeping the blocks already finished.
    Stop,
    /// What the trial has measured so far. The default.
    Status,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum ModeArg {
    DryRun,
    Live,
}

impl From<ModeArg> for Mode {
    fn from(m: ModeArg) -> Self {
        match m {
            ModeArg::DryRun => Self::DryRun,
            ModeArg::Live => Self::Live,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    // Before anything prints, and once: every renderer below reads the answer
    // rather than the flags (`crate::paint`).
    paint::configure(args.no_color, args.json);

    // Trace reading and replay work on files, so they answer without a running
    // daemon — which matters, because the session you most want to inspect is
    // usually the one that has already ended.
    match &args.command {
        Cmd::Trace { trace, app, decisions, list } => {
            return traces::show(trace.as_deref(), app.as_deref(), *decisions, *list, args.json);
        }
        Cmd::Replay { trace, config, verbose } => {
            return traces::replay(trace.as_deref(), config.as_deref(), *verbose, args.json);
        }
        Cmd::Export => return metrics::export().await,
        // Both write to stdout and want no daemon: they are how the packaging
        // gets its completions and its manual, and they have to work on a
        // machine where dozed has never run.
        Cmd::Completions { shell } => {
            let mut cmd = <Args as clap::CommandFactory>::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(*shell, &mut cmd, name, &mut std::io::stdout());
            return Ok(());
        }
        Cmd::Man { to } => {
            let cmd = <Args as clap::CommandFactory>::command();
            match to {
                Some(dir) => {
                    std::fs::create_dir_all(dir)
                        .with_context(|| format!("creating {}", dir.display()))?;
                    clap_mangen::generate_to(cmd, dir)
                        .with_context(|| format!("writing manual pages to {}", dir.display()))?;
                }
                None => clap_mangen::Man::new(cmd)
                    .render(&mut std::io::stdout())
                    .context("writing the manual to stdout")?,
            }
            return Ok(());
        }
        Cmd::Suggest { trace, config, write, anyway } => {
            return traces::suggest(
                trace.as_deref(),
                config.as_deref(),
                write.as_deref(),
                args.json,
                *anyway,
            );
        }
        // Edits a file, and the daemon is only told about it afterwards. Doing
        // it through the config file rather than through an IPC command is the
        // same choice §12 records for the dashboard's editor: the file is the
        // source of truth, and a second way to change settings is a second way
        // for them to be true.
        Cmd::UpgradePresets { config, write } => {
            return upgrade_presets(config.as_deref(), *write, args.json);
        }
        _ => {}
    }

    let (request, follow) = match &args.command {
        Cmd::Status => (Request::Status, false),
        // Not `watching`: this prints once and exits, so a faster sampling tick
        // would arrive after the table it was for.
        Cmd::Apps => (Request::ListApps { watching: false }, false),
        Cmd::Why { app, rules } => (Request::Why { app: app.clone(), rules: *rules }, false),
        Cmd::Journal { since, follow, all } => (
            Request::Journal { since: parse_since(since.as_deref())?, follow: *follow, all: *all },
            *follow,
        ),
        Cmd::Watch { all } => (Request::Journal { since: None, follow: true, all: *all }, true),
        Cmd::Report { since } => (Request::Report { since: parse_since(since.as_deref())? }, false),
        Cmd::Mode { mode } => (Request::SetMode { mode: (*mode).into() }, false),
        Cmd::Snooze { duration, off } => {
            let duration = match (duration.as_deref(), off) {
                (Some(d), _) => Some(
                    d.parse::<HumanDuration>()
                        .map_err(|e| anyhow::anyhow!("bad snooze duration: {e}"))?
                        .get(),
                ),
                (None, true) => None,
                // A bare `dozectl snooze` is ambiguous between "for a while" and
                // "stop snoozing", and guessing either way is a mode change
                // nobody asked for.
                (None, false) => {
                    bail!("say how long, e.g. `dozectl snooze 1h`, or `dozectl snooze --off`")
                }
            };
            (Request::Snooze { duration }, false)
        }
        Cmd::Freeze { app } => {
            (Request::Act { app: app.clone(), action: Action::Freeze { reclaim: false } }, false)
        }
        Cmd::Throttle { app, pct } => {
            (Request::Act { app: app.clone(), action: Action::Throttle { pct: *pct } }, false)
        }
        Cmd::Restore { app } => (Request::Act { app: app.clone(), action: Action::Restore }, false),
        Cmd::Ease { app, for_ } => (
            Request::SetHold {
                app: app.clone(),
                ceiling: Some(dozed_core::Ceiling::LeaveAlone),
                duration: parse_for(for_.as_deref())?,
            },
            false,
        ),
        Cmd::Cap { app, pct, for_ } => {
            if let Some(pct) = pct
                && !(1..=100).contains(pct)
            {
                bail!("`--pct {pct}` is not a percentage of one core; use 1-100");
            }
            (
                Request::SetHold {
                    app: app.clone(),
                    ceiling: Some(dozed_core::Ceiling::CapOnly { pct: *pct }),
                    duration: parse_for(for_.as_deref())?,
                },
                false,
            )
        }
        Cmd::Unhold { app } => {
            (Request::SetHold { app: app.clone(), ceiling: None, duration: None }, false)
        }
        Cmd::Thaw { all } => {
            if !*all {
                bail!("use `dozectl thaw --all`; there is no partial thaw");
            }
            (Request::ThawAll, false)
        }
        Cmd::Reload => (Request::ReloadConfig, false),
        Cmd::Trial { action } => (
            match action {
                Some(TrialCmd::Start { pairs, block, seed, on_mains }) => Request::StartTrial {
                    pairs: *pairs,
                    block: parse_since(block.as_deref())?,
                    seed: *seed,
                    on_battery_only: Some(!on_mains),
                },
                Some(TrialCmd::Stop) => Request::StopTrial,
                // A bare `dozectl trial` asks rather than acts. Starting a
                // four-hour experiment that moves the machine into live mode is
                // not something a command should do by default.
                Some(TrialCmd::Status) | None => Request::TrialStatus,
            },
            false,
        ),
        Cmd::Trace { .. }
        | Cmd::Replay { .. }
        | Cmd::Suggest { .. }
        | Cmd::Export
        | Cmd::Completions { .. }
        | Cmd::UpgradePresets { .. }
        | Cmd::Man { .. } => {
            unreachable!("handled above, without a daemon")
        }
    };

    let path = ipc::socket_path();
    let stream = connect(&path).await?;
    let (read, mut write) = stream.into_split();
    write.write_all(ipc::encode(&request)?.as_bytes()).await?;

    let mut lines = BufReader::new(read).lines();
    let mut exit_error = false;
    // The journal is the one response that arrives as a stream of rows, so it
    // is the one that gets a table. Printed before the first row rather than
    // lazily, so `watch` on an idle daemon still explains what it is waiting
    // to show.
    let tabular = matches!(request, Request::Journal { .. }) && !args.json;
    if tabular {
        emit(&format!("{JOURNAL_LEGEND}\n\n{JOURNAL_HEADER}\n"));
    }
    let mut last_at: Option<dozed_core::MonoTime> = None;
    while let Some(line) = lines.next_line().await? {
        if args.json {
            emit(&format!("{line}\n"));
        }
        let response: Response =
            serde_json::from_str(&line).with_context(|| format!("unparseable response: {line}"))?;
        match &response {
            Response::EndOfStream if !follow => break,
            Response::EndOfStream => continue,
            _ => {}
        }
        if let (true, Response::Entry(e)) = (tabular, &response) {
            if last_at.is_some_and(|prev| e.at < prev) {
                emit(&restart_banner());
            }
            last_at = Some(e.at);
            emit(&journal_row(e));
        } else if !args.json {
            emit(&render(&response));
        }
        if matches!(response, Response::Error { .. }) {
            exit_error = true;
        }
        // Single-shot requests get exactly one substantive reply.
        if !follow && !matches!(response, Response::Entry(_)) {
            break;
        }
    }

    if exit_error {
        std::process::exit(1);
    }
    Ok(())
}

/// What the report's columns mean, in the order they appear on a row.
///
/// Added because `CPU-s` did not survive contact with a reader — twice. It is
/// the same unit as the summary line's `CPU-seconds`, abbreviated to fit the
/// column, and abbreviating a unit is exactly the kind of saving that costs
/// more than it saves.
///
/// Printed by `dozectl` rather than by `Report::render`, so the GUI — which has
/// room for real labels and tooltips — is not made to carry a text legend it
/// does not need.
const REPORT_LEGEND: &str = "  APP        the application
  DOZED      how long it was held, in total — frozen, or capped by a throttle
  CPU-s      CPU-seconds — one second of one core at full tilt
  WAKEUPS    times it woke the CPU over that period
  NOT HELD   this app went on running while dozed had it held: the freeze is not taking,\n             and every saving on the row above it is overstated by that much";

/// The journal's two clocks, and why both are here.
///
/// `ELAPSED` is the daemon's monotonic time, and on its own it misleads: it
/// counts from the start of *that run* of `dozed`, but the journal is persisted
/// and reloaded across restarts. Read in a column by itself it runs backwards
/// at every restart boundary, with nothing to say why. Observed in a live
/// journal: `+3900.142s` immediately followed by `+244.589s`.
///
/// `TIME` fixes that and is what a person actually wants while watching. It
/// costs nothing to show — every entry already carries `wall` next to `at`.
///
/// `ELAPSED` stays because it is the key that ties a journal line to a
/// `dozectl trace` record, and §14's replay orders on it precisely because it
/// cannot be perturbed by an NTP step or a suspend.
const JOURNAL_LEGEND: &str = "\
TIME     wall clock                        APP      the application
ELAPSED  since this dozed run began        ACTION   what dozed did; \"would …\" is dry-run
         (resets to zero on restart)       WHY      which rule fired, and whether it landed
WAKES/s  the app's rate when the rule fired; \"-\" on a restore, where it only measures the hold";

/// `ACTION` is sized for `would throttle`, the longest verb. Dry-run is a
/// first-class mode, not an edge case — sized for the live verbs instead, every
/// row of a dry-run journal would sit out of alignment.
const JOURNAL_HEADER: &str =
    "TIME      ELAPSED       APP                ACTION         WAKES/s  WHY";

/// One journal entry as table columns.
///
/// Widths are fixed rather than fitted to the data: `watch` streams, so there
/// is no last row to measure against. `APP` is truncated at its width like the
/// `apps` table's is — `org.wezfurlong.wezterm` is 22 characters and not
/// unusual, and one such row must not knock the whole column out of true.
///
/// `WAKES/s` is the reason the tool exists — §13's primary metric is wakeups
/// per second, not CPU% — and without it every row reads as an equal win.
/// Measured over one real journal: of 89 freeze and throttle decisions, the
/// median app was doing 280 wakeups/s and the busiest 3466, but **18 of them
/// were already under 10/s** and saved essentially nothing. Those eighteen
/// rows were indistinguishable from the rest.
///
/// The cell therefore distinguishes three things, and the distinction is the
/// point:
///
/// * a number — measured, and the justification for the row;
/// * `-` — a restore reading zero, which measured the hold rather than the app;
/// * blank — no sample at all. The global restores (shutdown, suspend, panic
///   thaw, focus-source loss) record no `DecisionInputs`, because there the
///   kind is the whole explanation.
///
/// Collapsing any of those into `0` would be worse than showing nothing:
/// eighteen doze rows in one real journal legitimately measured at or near
/// zero, and that is exactly the finding the column exists to surface, so `0`
/// has to keep meaning zero.
fn journal_row(e: &dozed_core::JournalEntry) -> String {
    let clock = chrono::DateTime::from_timestamp(e.wall as i64, 0)
        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "--:--:--".to_owned());
    // Whole wakeups per second, matching how `dozectl trace` prints the same
    // number. The decimals are noise at the scale the judgement is made on.
    let wakes = e.reason.inputs.as_ref().map_or_else(String::new, |i| {
        let rate = format!("{:.0}", i.sample.wakeups_per_sec);
        // A restore that reads zero is not measuring a quiet app, it is
        // measuring the freeze it is about to undo: the sample was taken while
        // the app was held, so zero was the only answer available. On a doze
        // that same zero is real and load-bearing, which is why the
        // substitution is narrow — restores that read non-zero (32 of 54 in one
        // real journal, up to 948/s) are genuine and stay as numbers.
        if rate == "0" && matches!(e.action, Action::Restore) { "-".to_owned() } else { rate }
    });
    // Padded then coloured, and only the verb: a stream of rows wants one
    // thing the eye can land on, not five.
    let verb = format!("{:<14}", e.verb());
    let verb = match e.action {
        Action::Restore => verb,
        Action::Throttle { .. } | Action::Freeze { .. } => paint::held(&verb),
    };
    format!(
        "{clock}  {:<12}  {:<18} {verb} {wakes:>7}  {}{}\n",
        e.at.to_string(),
        truncate(e.app.label(), 18),
        e.reason.detail,
        e.qualifier()
    )
}

/// A restart is only visible as `at` going backwards, so say so outright rather
/// than leaving it to be inferred from a number that just got smaller.
fn restart_banner() -> String {
    format!("{:─^78}\n", " dozed restarted ")
}

/// Offer the rules this build ships that a config file has not got.
///
/// `DESIGN.md` §9 states the gap and leaves it standing: the shipped rules are
/// written once on first install and never touched again, so curation "is not a
/// fix that reaches the people already running it". For the app name lists that
/// is a shame; for `shared-window-exempt`, which exists because a shared window
/// was being frozen in somebody's meeting, it means the people it was written
/// for are precisely the ones who never get it.
///
/// Four properties, and each of them is a way this could have been worse than
/// the gap it closes:
///
/// * **Nothing happens without `--write`.** A rule deleted on purpose is not
///   distinguishable from one that predates the version adding it — nothing on
///   disk records which — so this asks rather than migrating on upgrade.
/// * **It appends.** The file is not regenerated, so comments the user wrote
///   survive. `dozed --annotate-config` is the command that rewrites, and it
///   says so.
/// * **The result is parsed before it is kept**, by `Config::parse` — the
///   daemon's own function — so a file this produces cannot be one that fails
///   at the next restart.
/// * **The original is kept.** Same backup dance as `annotate_in_place`, and an
///   existing backup is never clobbered.
fn upgrade_presets(config: Option<&std::path::Path>, write: bool, json: bool) -> Result<()> {
    let path = config.map_or_else(dozed_core::paths::config_file, std::path::Path::to_path_buf);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let cfg = dozed_core::Config::parse(&text)
        .with_context(|| format!("in {}", path.display()))
        // A config that does not parse is not something to append to. The daemon
        // is running on the last one that did, and adding rules to a broken file
        // would produce a differently broken file.
        .context("a config that does not parse is not one to add rules to; fix it first")?;

    let missing = dozed_core::presets::missing_from(&cfg);

    if json {
        let rows: Vec<serde_json::Value> = missing
            .iter()
            .map(|m| {
                serde_json::json!({
                    "name": m.rule.name,
                    "priority": m.rule.priority,
                    "priority_clash": m.priority_clash,
                })
            })
            .collect();
        emit(&format!(
            "{}\n",
            serde_json::to_string_pretty(&serde_json::json!({
                "config": path.display().to_string(),
                "missing": rows,
                "written": write && !missing.is_empty(),
            }))?
        ));
        if !write || missing.is_empty() {
            return Ok(());
        }
    }

    if missing.is_empty() {
        if !json {
            emit(&format!("{} has every rule this version ships.\n", path.display()));
        }
        return Ok(());
    }

    if !json {
        emit(&format!(
            "{} is missing {} rule{} this version ships:\n\n",
            path.display(),
            missing.len(),
            if missing.len() == 1 { "" } else { "s" }
        ));
        for m in &missing {
            emit(&dozed_core::starter::render_rule(&m.rule));
            if let Some(other) = &m.priority_clash {
                // Legal — first-match-wins resolves a tie by position — but
                // which of the two wins is then decided by something that is not
                // visible in the file.
                emit(&format!(
                    "{}\n",
                    paint::warn(&format!(
                        "# note: priority {} is already used by `{other}`; whichever comes \
                         first in the file wins",
                        m.rule.priority
                    ))
                ));
            }
            emit("\n");
        }
    }

    if !write {
        if !json {
            emit(&paint::faint(
                "Nothing has been changed. Re-run with --write to append these, or copy the \
                 ones you want by hand.\n",
            ));
            emit("\n");
        }
        return Ok(());
    }

    // Validated as a whole before anything is written: appending each rule's
    // text and hoping is how a config that parses in pieces fails to parse as a
    // file.
    let mut appended = text.clone();
    if !appended.ends_with('\n') {
        appended.push('\n');
    }
    for m in &missing {
        appended.push('\n');
        appended.push_str(&dozed_core::starter::render_rule(&m.rule));
    }
    let round_tripped = dozed_core::Config::parse(&appended)
        .context("the upgraded config did not parse; nothing has been written")?;
    // The rules that were already there must come through untouched. This is
    // the assertion that makes "it only ever adds" a fact rather than an
    // intention.
    for existing in &cfg.rules {
        anyhow::ensure!(
            round_tripped.rules.contains(existing),
            "appending would have changed the existing rule `{}`; nothing has been written",
            existing.name
        );
    }

    let backup = path.with_file_name(format!(
        "{}.bak-preupgrade",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    if !backup.exists() {
        std::fs::copy(&path, &backup)
            .with_context(|| format!("backing up to {}", backup.display()))?;
    }
    std::fs::write(&path, &appended).with_context(|| format!("writing {}", path.display()))?;

    if !json {
        emit(&format!(
            "Appended {} rule{} to {}. The previous file is at {}.\n",
            missing.len(),
            if missing.len() == 1 { "" } else { "s" },
            path.display(),
            backup.display()
        ));
        emit("Run `dozectl reload` to apply them without restarting the daemon.\n");
    }
    Ok(())
}

fn parse_since(s: Option<&str>) -> Result<Option<Duration>> {
    match s {
        None => Ok(None),
        Some(s) => {
            let d: HumanDuration =
                s.parse().map_err(|e| anyhow::anyhow!("bad --since value: {e}"))?;
            Ok(Some(d.get()))
        }
    }
}

fn render(response: &Response) -> String {
    match response {
        Response::Status(s) => {
            let mut out = String::new();
            // The mode *in force*, and the configured one when they differ.
            // `mode live` above `0 held` on a daemon started `--dry-run` was the
            // status line contradicting itself.
            // Live is the state where this program can stop your applications,
            // so it gets the colour a fault would. Dry-run is deliberately quiet.
            let mode = match s.mode {
                Mode::Live => paint::warn(s.mode.as_str()),
                _ => paint::faint(s.mode.as_str()),
            };
            match s.configured_mode {
                Some(configured) => out.push_str(&format!(
                    "{}          {mode} (config says {}; daemon started without --live)\n",
                    paint::head("mode"),
                    configured.as_str()
                )),
                None => out.push_str(&format!("{}          {mode}\n", paint::head("mode"))),
            }
            // Directly under the mode, for the same reason the snooze line is:
            // it is why the mode reads the way it does, and `configured_mode`
            // structurally cannot say so (`ipc::StatusReport::mode_driven_by_trial`).
            if s.mode_driven_by_trial {
                out.push_str(&format!(
                    "              {}\n",
                    paint::note("set by the running trial block, not by your config")
                ));
            } else if let Some(restores) = s.trial_restores {
                // The other three quarters of a trial's life. Blocks are twenty
                // minutes and the waits between them are hours, so most of the
                // time no arm is current and `mode_driven_by_trial` is false —
                // which used to leave a bare `mode dry-run` with nothing on the
                // line to connect it to the trial further down the page. Being
                // the correct mode did not make it legible.
                let same = restores == s.mode;
                out.push_str(&format!(
                    "              {}\n",
                    paint::note(&if same {
                        format!(
                            "between trial blocks, so this is the `{}` the trial found when it \
                             started and puts back each time a block ends",
                            restores.as_str()
                        )
                    } else {
                        format!(
                            "a trial is running; it will put the mode back to `{}` when its \
                             next block ends",
                            restores.as_str()
                        )
                    })
                ));
            }
            // Directly under the mode, because it is the reason the mode reads
            // the way it does. A snoozed daemon showing only `dry-run` is
            // indistinguishable from one somebody switched off and forgot.
            if let Some(left) = s.snooze_remaining {
                out.push_str(&format!(
                    "snooze        {} left, then back to what it was doing\n",
                    Coarse(left)
                ));
            }
            out.push_str(&format!("uptime        {}\n", Coarse(s.uptime)));
            out.push_str(&format!(
                "power         {}{}\n",
                if s.power.on_ac { "AC" } else { "battery" },
                s.power.percentage.map(|p| format!(" ({p:.0}%)")).unwrap_or_default()
            ));
            out.push_str(&format!("desktop       {}\n", s.desktop.as_str()));
            // Two failures used to print as one. A lost focus source is either a
            // helper that went away or a daemon that cannot hear it any more, and
            // saying "LOST" for the second sent the reader after the compositor
            // while the fault was on this side of the socket.
            out.push_str(&format!(
                "focus source  {}{}\n",
                match (s.focus_source_healthy, s.session_bus_healthy) {
                    (true, _) => paint::good("ok"),
                    (false, true) => paint::bad("LOST — holding nothing"),
                    (false, false) => paint::bad("LOST — our own session bus is not delivering"),
                },
                s.compositor.as_ref().map(|c| format!(" ({c})")).unwrap_or_default()
            ));
            if let Some(src) = &s.focus_source {
                out.push_str(&format!("source        {src}\n"));
            }
            out.push_str(&format!("apps          {} known, {} held\n", s.apps_known, s.apps_held));
            // A battery tool that cannot show its own cost is asking to be
            // taken on faith (`DESIGN.md` §13).
            out.push_str(&format!("own wakeups   {:.1}/s\n", s.self_wakeups_per_sec));
            // The other half of the daemon's own cost, and the larger half
            // while idle: the sampler backs off, the bus watcher does not.
            if let Some(calls) = s.bus_calls_per_sec {
                out.push_str(&format!("own bus calls {calls:.2}/s\n"));
            }
            // Only worth a line when it is the expensive one. `schedstat` is the
            // ordinary case and costs nobody an explanation; `status` is twice
            // the sweep, and somebody comparing this machine's `own wakeups`
            // against another's deserves to know which of the two they have.
            if s.wakeup_counter.as_deref() == Some("status") {
                out.push_str(&format!(
                    "              {}\n",
                    paint::faint(
                        "wakeups read from /proc/<tid>/status: this kernel does not maintain \
                         schedstat, so the per-thread sweep costs about twice what it would"
                    )
                ));
            }
            // A safety net that broke says so where the health is read, not only
            // in a journal line that has already scrolled past. Without the
            // record of what is held, a crash leaves applications frozen until
            // something sweeps them up (§10.3).
            if let Some(why) = &s.frozen_file_error {
                out.push_str(&format!(
                    "held record   {}\n",
                    paint::bad(&format!(
                        "cannot be written: {why} — a crash now would leave held apps \
                         frozen until the next startup sweep"
                    ))
                ));
            }
            out.push_str(&format!(
                "config        {}\n",
                s.config_path.as_deref().unwrap_or("(built-in defaults)")
            ));
            // Same principle as the line above: a recorder whose cost is
            // invisible is asking to be taken on faith.
            out.push_str(&match &s.trace {
                // The size is the evidence that recording is affordable, so a
                // recorder that stopped has to say so on the same line: the
                // number it stopped at is smaller than a healthy one, and
                // reading it as good news is the whole failure mode.
                Some(t) => format!(
                    "trace         {} ({}{} records, {}){}{}\n",
                    t.path,
                    // Said where it is true, which is after a rotation. Without
                    // it the size on this line drops from 64 MB to nothing at a
                    // file boundary and reads as a recorder that just broke —
                    // the same misreading as the small file, from the other end.
                    if t.part > 1 { format!("part {}, ", t.part) } else { String::new() },
                    t.records,
                    traces::human_bytes(t.bytes),
                    match &t.stopped {
                        // Three facts, each said when it is true, rather than
                        // four hand-written arms that disagreed about which of
                        // them mattered.
                        Some(why) => {
                            let mut line = format!(" — STOPPED: {why}");
                            // Whether it is coming back is the daemon's to say,
                            // not this program's to guess from the reason text:
                            // guessing is what put "hit the 67108864 byte size
                            // cap; retrying every 60s" on a real machine for a
                            // day. Either answer makes it a condition rather
                            // than a moment, which "STOPPED" alone does not.
                            line.push_str(if t.retrying {
                                "; retrying every 60s"
                            } else {
                                "; not retried before the next restart"
                            });
                            // And what it is costing, wherever that is known.
                            // This was printed only for a stop that was *not*
                            // being retried, so it vanished in the one case
                            // where the number is still climbing — the machine
                            // this was written for hit the cap on day four of a
                            // seven-day run and went on losing records for
                            // three more.
                            if t.lost > 0 {
                                line.push_str(&format!(
                                    "; {} record{} unrecorded since",
                                    t.lost,
                                    if t.lost == 1 { "" } else { "s" }
                                ));
                            }
                            paint::bad(&line)
                        }
                        None => String::new(),
                    },
                    // Outlives the outage, because the hole does. A recorder
                    // that recovered is healthy and its file is still missing a
                    // stretch, and those are two different facts — reporting
                    // only the first would let a whole recording be assumed from
                    // a working one.
                    match t.gaps {
                        0 => String::new(),
                        n => paint::warn(&format!(
                            " — {n} gap{} in this recording",
                            if n == 1 { "" } else { "s" }
                        )),
                    }
                ),
                None => "trace         not recording\n".to_owned(),
            });
            // The instrument behind the *measured* report, shown next to the
            // daemon's own cost for the same reason: a tool asking to be believed
            // about power should say what it is reading.
            if let Some(e) = &s.energy {
                let mut parts = Vec::new();
                if let Some(w) = e.discharge_w {
                    parts.push(format!("{w:.2} W discharge"));
                }
                // Named for what it measures. "W" alone next to a discharge
                // figure would read as a second opinion about the same quantity,
                // and the package is a part of the machine rather than all of it.
                if let Some(w) = e.package_w {
                    parts.push(format!("{w:.2} W cpu package"));
                }
                // Under its own label, never merged with the package figure and
                // never added to it. On an integrated GPU this is the *same
                // silicon* measured by a second driver — 24.07 W of RAPL package
                // against 24.00 W of amdgpu PPT on the machine this was written
                // against — and on a discrete card it is a separate chip. No
                // attribute in `/sys` tells the two apart, so the sensor's own
                // word is printed and the sum is left unmade.
                if let Some(w) = e.gpu_w {
                    let label = e.gpu_label.as_deref().unwrap_or("gpu");
                    parts.push(format!("{w:.2} W amdgpu {label}"));
                }
                if let Some(f) = e.deepest_idle {
                    parts.push(format!("{:.1}% deep idle over {} cpus", f * 100.0, e.cpus));
                }
                // The other side of deep idle, and the denominator every per-app
                // watt figure is divided by (`dozectl apps`). Shown here so the
                // estimate's divisor is visible somewhere that is not a footnote
                // under the thing it divided.
                if let Some(c) = e.machine_cores_busy {
                    parts.push(format!("{c:.2} of {} cores busy", e.cpus));
                }
                // Beside the GPU's wattage rather than instead of it: on an
                // integrated chip the watts are mostly a floor that barely moves,
                // and this is the half that does. It is also the denominator the
                // per-app GPU column is read against.
                if let Some(b) = e.gpu_busy {
                    parts.push(format!("{:.0}% gpu busy", b * 100.0));
                }
                if let Some(w) = e.system_wakeups_per_sec {
                    parts.push(format!("{w:.0}/s system-wide"));
                }
                if parts.is_empty() {
                    // On mains the battery reports nothing at all, which is
                    // different from reporting zero (`dozed_linux::energy`).
                    parts.push("no reading yet".to_owned());
                }
                // Appended after the readings, because it is not one: it is the
                // reason a reading this machine is capable of taking is missing.
                // Said only when the counter is there and shut, never when the
                // hardware simply has none — see `ipc::EnergyStatus`.
                if e.package_denied {
                    parts
                        .push("cpu package unreadable (needs a permission; see README)".to_owned());
                }
                out.push_str(&format!("power meter   {}\n", parts.join(", ")));
                // What this machine's watts actually respond to, fitted to its own
                // counters — and, just as often, why no fit is on offer. Printed
                // whichever it is: on the machine this was written for the refusal
                // *was* the finding, and a package pinned to a firmware limit is
                // the one state in which holding an app saves nothing at all.
                if let Some(v) = &e.power_model {
                    out.push_str(&format!("              {}\n", paint::faint(&v.summary())));
                }
                // The package broken down, on its own line and *indented under*
                // the total rather than listed beside it. `intel-rapl:0:0` is
                // `core`, a subset of its parent, so a reader who adds this row
                // to the one above gets a CPU drawing half again what it draws —
                // the trap §11.1 names once for the package total and which gets
                // worse the moment the parts are visible. The layout is the
                // defence: parts inside, remainder named, nothing to sum.
                if !e.package_parts.is_empty() {
                    let mut inside: Vec<String> =
                        e.package_parts.parts().map(|(n, w)| format!("{n} {w:.2} W")).collect();
                    // What the package spends that no sub-domain claimed. Named
                    // rather than left as the gap between two numbers, because a
                    // breakdown that does not account for its own total reads as
                    // a bug in the tool. Measured here: 2.2 W of core inside a
                    // 24.1 W package, so this is usually the larger half.
                    if let Some(rest) = e.package_w.and_then(|w| e.package_parts.rest_w(w)) {
                        inside.push(format!("{rest:.2} W not itemised"));
                    }
                    out.push_str(&format!(
                        "              {}\n",
                        paint::faint(&format!("of which: {}", inside.join(", ")))
                    ));
                }
                // Said only where both figures are present, because it is about
                // the relationship between them and there is no relationship to
                // warn about when one is missing.
                if e.gpu_w.is_some() && e.package_w.is_some() {
                    out.push_str(&format!(
                        "              {}\n",
                        paint::faint(
                            "the gpu figure may be the same silicon as the cpu package on an \
                             integrated gpu; nothing here adds them"
                        )
                    ));
                }
            }
            if let Some(t) = &s.thermal {
                let mut parts = vec![t.state.as_str().to_owned()];
                if let Some(c) = t.hottest_celsius {
                    parts.push(format!("{c:.0}C"));
                }
                match t.fastest_rpm {
                    // Measured: a temperature threshold alone calls a machine
                    // with a 4500 RPM fan "normal", because 70C is unremarkable.
                    // The fan is the thing making the noise, so it is shown.
                    Some(rpm) => parts.push(format!("fan {rpm} rpm")),
                    None => parts.push("no fan".to_owned()),
                }
                out.push_str(&format!("thermal       {}\n", parts.join(", ")));
                // Where the fan thresholds came from. Learned ones are the
                // default, and a threshold deciding things with nothing on
                // screen to explain it is the failure this whole section exists
                // to stop repeating.
                if let Some(f) = &t.fan {
                    let source = if f.pinned { "from config" } else { "learned" };
                    let line = match (f.thresholds, f.observed) {
                        (Some((hot, cool)), Some((lo, hi))) => format!(
                            "fan hot >={hot} rpm, cool <{cool} ({source}; seen {lo}-{hi} this run)"
                        ),
                        (Some((hot, cool)), None) => {
                            format!("fan hot >={hot} rpm, cool <{cool} ({source})")
                        }
                        (None, Some((lo, hi))) => format!(
                            "fan not deciding: only {lo}-{hi} rpm seen so far, \
                             too little range to tell loud from idle"
                        ),
                        (None, None) => "fan not deciding: nothing measured yet".to_owned(),
                    };
                    out.push_str(&format!("              {}\n", paint::faint(&line)));
                }
                // Which sensors are deliberately not in the maximum above. A
                // drive at 68C is doing nothing wrong and is not evidence about
                // this machine being hot, but a temperature quietly missing
                // from a maximum looks exactly like a sensor that is not there
                // — and one of those is this list being wrong about somebody's
                // hardware.
                if !t.ignored.is_empty() {
                    out.push_str(&format!(
                        "              {}\n",
                        paint::faint(&format!(
                            "not counted as this machine's temperature: {} \
                             (own thermal envelope, not the cpu or the fan)",
                            t.ignored.join(", ")
                        ))
                    ));
                }
                // A threshold that cannot be reached on this machine looks
                // exactly like a machine that is genuinely always hot, and the
                // difference decides whether every thermal rule in the file is
                // doing anything. Said in full, because the fix is a number in
                // the config and the user needs to know which one.
                if t.saturated {
                    out.push_str(&format!(
                        "              {}\n",
                        paint::bad(
                            "hot for 4h without coming near clearing, so `thermal` rules have \
                             been firing unconditionally that whole time. Either the machine \
                             really is that hot, or `hot_rpm`/`hot_celsius` are out of its \
                             range — the readings above say which."
                        )
                    ));
                }
            }
            if let Some(t) = &s.trial {
                let state = match (&t.arm, t.waiting) {
                    (Some(arm), _) => format!("measuring {arm}"),
                    (None, true) => "waiting for idle on battery".to_owned(),
                    (None, false) => "complete".to_owned(),
                };
                out.push_str(&format!(
                    "trial         {}/{} blocks, {state}\n",
                    t.blocks_done, t.blocks_total
                ));
            }
            if !s.exemption_sources.is_empty() {
                // A rule that matches on state nothing sets can never fire, and
                // that failure is otherwise completely silent.
                out.push_str(&format!("\n{}\n", paint::head("exemption sources")));
                for e in &s.exemption_sources {
                    let state = match (&e.unavailable, &e.degraded, e.reported) {
                        // Not a fault: the platform cannot do it, and no amount
                        // of colour will change that.
                        (Some(why), _, _) => paint::faint(&format!("unavailable — {why}")),
                        // Degraded outranks the process count: "ok, 0
                        // process(es)" beside a source that is half switched off
                        // is the reassuring half of the truth.
                        (None, Some(why), _) => paint::warn(&format!("DEGRADED — {why}")),
                        (None, None, false) => {
                            paint::bad("NOT REPORTING — this rule cannot fire").to_owned()
                        }
                        (None, None, true) => paint::good(&format!("ok, {} process(es)", e.pids)),
                    };
                    // The name is a label, not a heading: nine bold rows in a
                    // column is not emphasis, it is a wall. Only the state —
                    // the half that can be bad news — carries colour.
                    out.push_str(&format!("  {:<18} {state}\n", e.name));
                }
            }
            out
        }
        Response::Apps { apps, machine } => {
            if apps.is_empty() {
                return "no apps yet — is the compositor helper running?\n".to_owned();
            }
            // The estimate's column exists only where there is a measured total
            // to apportion, which on a stock kernel is nowhere: RAPL is `0400
            // root`. A permanent column of `-` would be a promise the machine
            // cannot keep, printed on every invocation — so the table is narrower
            // instead, and `power meter` in `dozectl status` already says why.
            let basis = machine.basis;
            let watts_head =
                if basis.is_some() { format!("{:>8}", "EST W") } else { String::new() };
            // Shown only where something in the list actually holds a DRM client.
            // A column of dashes on a desktop nobody is rendering on is width
            // spent to say "no".
            let any_gpu = apps.iter().any(|a| a.sample.gpu_pct.is_some());
            let gpu_head = if any_gpu { format!("{:>7}", "GPU%") } else { String::new() };
            let mut out = format!(
                "{}\n",
                paint::head(&format!(
                    "{:<26} {:<18} {:<12} {:>7} {:>9}{}{}  {}",
                    "APP",
                    "VISIBILITY",
                    "STATE",
                    "CPU%",
                    "WAKEUPS/s",
                    gpu_head,
                    watts_head,
                    "NOTES"
                ))
            );
            for a in apps {
                let mut notes: Vec<String> = Vec::new();
                if a.mode == Mode::Monitor {
                    notes.push("monitor".to_owned());
                }
                // An app known only by its executable or its comm is one the
                // compositor gave no app_id and no desktop file for, so the name
                // in the APP column is a basename we derived rather than
                // anything the app declared. Worth seeing, because it is exactly
                // when a rule written against the name you expect — `app =
                // "libreoffice"` for a window that resolved to
                // `exe:/usr/lib/libreoffice/program/oosplash` — silently fails
                // to match. `dozectl why <app>` prints every identifier it saw.
                if matches!(a.key.kind, IdentityKind::ExePath | IdentityKind::Comm) {
                    notes.push(format!("{}-only", a.key.kind.as_str()));
                }
                // Before the exemptions, because it is the one a reader is
                // looking for: it is the only note in this column that somebody
                // put there on purpose, and the only one with a clock on it.
                if let Some(held) = &a.held_off {
                    notes.push(held.clone());
                }
                notes.extend(a.exempt.iter().cloned());
                // What is about to happen to this app, from the planner's own
                // deadline. Last in the notes because it is the only one that
                // is about the future, and because it is the one worth reading
                // twice: a `dozectl apps` that says an app is running says
                // nothing about the freeze thirty seconds away from it.
                if let Some(pending) = &a.pending {
                    notes.push(pending.summary());
                }
                let notes = notes.join(",");
                // `-` for an app the sampler has not measured yet, exactly as
                // the journal's WAKES/s column does it. A rate needs two
                // readings a minute apart, so a fresh daemon has none — and
                // printing `0.0` for that says "this app is idle" about an app
                // nobody has looked at.
                let (cpu, wakes) = if a.sampled {
                    (format!("{:.1}", a.sample.cpu_pct), format!("{:.0}", a.sample.wakeups_per_sec))
                } else {
                    ("-".to_owned(), "-".to_owned())
                };
                // Each cell padded to its visible width, then coloured. The
                // state column is where the eye goes, so it carries the colour;
                // the notes say why, so they carry a quieter one.
                // `-` for the same two reasons the columns beside it use one,
                // and they are different reasons: an app with no sample has
                // nothing to apportion, and an app whose sampled CPU exceeds what
                // `/proc/stat` saw in the last tick cannot be given a share
                // without inventing one (`attribution::Basis::watts`). Both are
                // "not measured", and neither is 0.0 W.
                let watts = match basis {
                    None => String::new(),
                    Some(b) => {
                        let cell = a
                            .sampled
                            .then(|| b.watts(a.sample.cpu_pct, a.sample.wakeups_per_sec))
                            .flatten()
                            .map_or_else(|| "-".to_owned(), |w| format!("{w:.2}"));
                        format!("{cell:>8}")
                    }
                };
                // `-` for an app that holds no DRM client at all, which is
                // most of them: it has not done no GPU work, it cannot do any.
                let gpu = if any_gpu {
                    let cell = a
                        .sample
                        .gpu_pct
                        .filter(|_| a.sampled)
                        .map_or_else(|| "-".to_owned(), |p| format!("{p:.1}"));
                    format!("{cell:>7}")
                } else {
                    String::new()
                };
                out.push_str(&format!(
                    "{:<26} {:<18} {} {cpu:>7} {wakes:>9}{gpu}{watts}  {}\n",
                    truncate(&a.label, 26),
                    a.visibility.as_str(),
                    state_cell(a, 12),
                    if notes.is_empty() { notes } else { paint::note(&notes) }
                ));
            }
            // A footer after a blank line rather than a TOTAL row in the table:
            // inside it, the sum aligns as though it were an app called TOTAL,
            // and anything reading `dozectl apps` line by line would parse it
            // as one.
            let coverage = Coverage::of(apps, *machine);
            out.push_str(&format!("\n{}\n", coverage.summary()));
            if let Some(note) = coverage.note() {
                out.push_str(&format!("{note}\n"));
            }
            // The estimate's arithmetic, under the column it produced. §11.1's
            // rule: a modelled figure travels with what it was divided by, or it
            // is a number nobody can check. Both lines are printed — the first is
            // how the column was computed, the second is which way it leans — and
            // the second matters more, because the bias runs against exactly the
            // app this tool is built to catch.
            if let Some(b) = basis {
                out.push_str(&format!("{}\n", b.summary()));
                if let Some(rest) = b.unattributed_w(coverage.tracked_cores) {
                    out.push_str(&format!(
                        "{}\n",
                        paint::faint(&format!(
                            "{rest:.2} W of that goes to what has no window: daemons, \
                             kernel threads, and dozed itself"
                        ))
                    ));
                }
                out.push_str(&format!("{}\n", paint::faint(b.caveat())));
            }
            out
        }
        Response::Why(w) => {
            let mut out = format!("{} ({})\n", paint::head(&w.label), w.key);
            out.push_str(&format!("  state       {}\n", paint_state(w.applied, 0)));
            out.push_str(&format!("  visibility  {}\n", w.visibility.as_str()));
            out.push_str(&format!("  mode        {}\n", w.mode.as_str()));
            // The answer to the question that was asked, so it is the one line
            // here worth finding without reading the others.
            out.push_str(&format!("  because     {}\n", paint::note(&w.reason.detail)));
            if let Some(rule) = &w.reason.rule {
                out.push_str(&format!("  rule        {rule}\n"));
            }
            if let Some(grace) = &w.grace {
                out.push_str(&format!("  grace       {grace}\n"));
            }
            out.push_str(&format!("  for         {}\n", Coarse(w.elapsed)));
            // The state the decision was made from. Without it the line above is
            // a claim; with it, it is checkable.
            if let Some(inputs) = &w.reason.inputs {
                out.push_str(&format!("  observed    {}\n", inputs.summary()));
            }
            out.push_str(&format!("  identity    {}\n", w.evidence));
            if let Some(cg) = &w.cgroup {
                out.push_str(&format!("  cgroup      {cg}\n"));
            }
            // Which process is actually responsible. An app is a tree, and the
            // total on its row says nothing about where inside that tree the
            // wakeups come from — and for an Electron app `comm` is the same
            // string for all seventeen of them, so the role is what makes this
            // readable at all (`ProcContrib::role`).
            if !w.top_processes.is_empty() {
                out.push_str("\n  busiest processes\n");
                for p in &w.top_processes {
                    let role =
                        if p.role.is_empty() { String::new() } else { format!(" [{}]", p.role) };
                    out.push_str(&format!(
                        "    {:>9.0}/s  pid {:<8} {}{}\n",
                        p.wakeups_per_sec, p.pid, p.comm, role
                    ));
                }
            }
            if !w.rules.is_empty() {
                out.push_str("\n  rules (-> decided, * would match but outranked)\n");
                for r in &w.rules {
                    out.push_str(&format!("  {}\n", r.line()));
                }
            }
            out
        }
        Response::Entry(e) => format!("{}\n", e.stamped_line()),
        // The legend goes after the summary and before the per-app table,
        // which is where a reader first meets the abbreviations — and only when
        // there is a table, since the summary spells its units out in full.
        Response::Report(r) => {
            let body = r.render(r.battery_wh);
            if r.apps.is_empty() {
                return body;
            }
            match body.split_once("\n\n") {
                Some((summary, rows)) => {
                    format!("{summary}\n\n{REPORT_LEGEND}\n\n{rows}")
                }
                None => body,
            }
        }
        Response::Trial { report } => match report {
            None => "no trial has been run — `dozectl trial start` measures whether \
                     dozing actually saves anything on this machine\n"
                .to_owned(),
            Some(t) => {
                let mut out = t.verdict.render(t.battery_wh);
                if t.waiting {
                    out.push_str("  waiting for suitable conditions: idle desktop, on battery\n");
                } else if let Some(arm) = &t.arm {
                    out.push_str(&format!("  measuring the {arm} block now\n"));
                }
                // Per block, because "why is my trial not finishing" is answered
                // by the discard column and by nothing else.
                if !t.blocks.is_empty() {
                    out.push_str(&format!(
                        "\n{:<6} {:<8} {:>8} {:>9} {:>9}  {}\n",
                        "PAIR", "ARM", "SAMPLES", "WATTS", "DEEPIDLE", "NOTE"
                    ));
                    for b in &t.blocks {
                        out.push_str(&format!(
                            "{:<6} {:<8} {:>8} {:>9} {:>9}  {}\n",
                            b.pair,
                            b.arm.as_str(),
                            b.samples,
                            b.discharge_w.map_or_else(|| "-".to_owned(), |w| format!("{w:.2}")),
                            b.deepest_idle
                                .map_or_else(|| "-".to_owned(), |f| format!("{:.1}%", f * 100.0)),
                            b.discarded
                                .map(|d| format!("dropped: {}", d.as_str()))
                                .unwrap_or_default()
                        ));
                    }
                }
                out
            }
        },
        Response::Acted { app, outcome } => format!("{app}: {outcome:?}\n"),
        Response::Ok => String::new(),
        Response::Error { message } => format!("error: {message}\n"),
        Response::EndOfStream => String::new(),
    }
}

/// `--for 30m`, or `None` for "until cleared".
///
/// Rejects zero rather than accepting it: a ceiling that lapses on the tick it
/// was set is a command that appeared to work and did nothing, which is the
/// failure mode this whole feature exists to avoid at a larger scale.
fn parse_for(text: Option<&str>) -> Result<Option<std::time::Duration>> {
    let Some(text) = text else { return Ok(None) };
    let parsed: dozed_core::HumanDuration =
        text.parse().map_err(|e| anyhow::anyhow!("could not read `--for {text}`: {e}"))?;
    if parsed.get().is_zero() {
        bail!("`--for {text}` is no time at all; omit it to hold until cleared");
    }
    Ok(Some(parsed.get()))
}

fn state_of(action: Action) -> &'static str {
    match action {
        Action::Restore => "running",
        Action::Throttle { .. } => "throttled",
        Action::Freeze { .. } => "dozing",
    }
}

/// The state, padded to `width` and *then* coloured.
///
/// `width` rather than a constant because the same word appears in a table that
/// needs its column and in `why`, which is a list of labelled lines and would
/// otherwise carry twelve characters of coloured whitespace after it.
///
/// Two states rather than three carry colour: two of them are dozed doing
/// something, and the third is dozed leaving an app alone — which is the default
/// and does not need finding in a list of twenty.
fn paint_state(action: Action, width: usize) -> String {
    let cell = format!("{:<width$}", state_of(action));
    match action {
        Action::Restore => cell,
        Action::Throttle { .. } | Action::Freeze { .. } => paint::held(&cell),
    }
}

/// The state column for one row, which is not always the applied action.
///
/// An app dozed may never touch reports `applied = Restore` like any other
/// untouched app, so [`state_of`] called it **running** — true, and an answer to
/// a different question than the column is read for. Measured on a live session:
/// `plasmashell` drew 84 consecutive `safety` refusals and appeared in this table
/// as `running`, indistinguishable from a browser that simply had not been dozed
/// yet. Somebody had to go and read the source to find out whether their panel
/// was about to be frozen, which is §10's rule failing in the place it is most
/// expensive.
///
/// Quiet rather than loud. This is not a fault and not something to act on — it
/// is the daemon saying nothing will ever happen here, which is precisely the
/// row the eye should skip.
fn state_cell(app: &ipc::AppSummary, width: usize) -> String {
    if app.never_actionable {
        return paint::faint(&format!("{:<width$}", "protected"));
    }
    paint_state(app.applied, width)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_owned();
    }
    s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report_with_rows() -> dozed_core::journal::Report {
        let saving = |name: &str, dozed: u64, cpu: f64, wakeups: f64, leak: f64| {
            dozed_core::journal::AppSaving {
                app: dozed_core::AppKey::desktop(name),
                dozed: Duration::from_secs(dozed),
                throttled: Duration::ZERO,
                cpu_seconds: cpu,
                wakeups,
                episodes: 3,
                leaked_cpu_seconds: 0.0,
                leaked_wakeups: leak,
            }
        };
        dozed_core::Report {
            power_fit: None,
            window: Duration::from_secs(7 * 3600),
            apps: vec![
                saving("firefox", 5400, 143.0, 512_000.0, 0.0),
                saving("signal", 3200, 61.0, 240_000.0, 0.0),
                saving("brave-browser", 2100, 22.0, 90_000.0, 900_000.0),
            ],
            total_dozed: Duration::from_secs(10_700),
            covered: Duration::from_secs(6_000),
            cpu_seconds: 226.0,
            wakeups: 842_000.0,
            dry_run: false,
            battery_wh: Some(60.0),
            on_ac: false,
            measured: None,
        }
    }

    #[test]
    fn the_report_legend_appears_between_the_summary_and_the_rows() {
        let out = plain_render(&Response::Report(Box::new(report_with_rows())));
        let legend = out.find("CPU-seconds — one second").expect("legend");
        let summary = out.find("avoided").expect("summary");
        let first_row = out.find("firefox").expect("a row");
        assert!(summary < legend && legend < first_row, "wrong order:\n{out}");
    }

    #[test]
    fn a_report_with_nothing_in_it_gets_no_legend() {
        // The summary spells its units out in full, so a legend explaining
        // columns that are not on screen is noise.
        let mut empty = report_with_rows();
        empty.apps.clear();
        let out = plain_render(&Response::Report(Box::new(empty)));
        assert!(!out.contains("CPU-seconds — one second"), "{out}");
    }

    /// A trial with `n` clean pairs at `dry` and `live` watts, as the daemon
    /// would hand it over the socket.
    fn trial_report(n: u32, dry: f64, live: f64) -> dozed_core::ipc::TrialReport {
        use dozed_core::trial::{Arm, Block, Trial, TrialPlan};
        let plan = TrialPlan { pairs: n, ..TrialPlan::default() };
        let mut trial = Trial::new(plan, Mode::DryRun);
        let mut blocks = Vec::new();
        for pair in 0..n {
            for (arm, w) in [(Arm::DryRun, dry), (Arm::Live, live)] {
                blocks.push(Block {
                    pair,
                    arm,
                    samples: 18,
                    discharge_w: Some(w + f64::from(pair) * 0.02),
                    package_w: None,
                    deepest_idle: Some(if arm == Arm::Live { 0.88 } else { 0.71 }),
                    system_wakeups_per_sec: Some(900.0),
                    discarded: None,
                });
            }
        }
        // Rebuilt through the real type so the verdict is the real arithmetic
        // rather than a hand-written one that could disagree with it.
        trial.stop();
        let mut verdict = trial.verdict();
        let paired: Vec<(f64, f64)> = (0..n)
            .map(|p| {
                let bump = f64::from(p) * 0.02;
                (dry + bump, live + bump)
            })
            .collect();
        let stats = dozed_core::stats::Paired::from_pairs(&paired);
        verdict.discharge_pairs = paired.len();
        verdict.pairs_used = paired.len();
        verdict.live_w = Some(live + f64::from(n - 1) * 0.01);
        verdict.dry_run_w = Some(dry + f64::from(n - 1) * 0.01);
        verdict.saving_w = stats.mean();
        verdict.saving_interval_w = stats.bootstrap_interval(1, 0.95);
        verdict.saving_p = stats.sign_flip_p(1);
        verdict.blocks_done = blocks.len();
        dozed_core::ipc::TrialReport {
            verdict,
            blocks,
            waiting: false,
            arm: None,
            battery_wh: Some(60.0),
        }
    }

    #[test]
    fn why_names_the_process_that_is_actually_responsible() {
        // The reading that motivated the role: a real Signal runs seventeen
        // processes and every one reports `comm = signal-desktop`, so a breakdown
        // without roles is seventeen identical rows.
        let proc = |pid: u32, role: &str, rate: f32| dozed_core::ProcContrib {
            pid,
            comm: "signal-desktop".into(),
            wakeups_per_sec: rate,
            role: role.into(),
        };
        let why = dozed_core::ipc::WhyReport {
            key: dozed_core::AppKey::desktop("signal"),
            label: "signal".into(),
            visibility: dozed_core::Visibility::Hidden,
            applied: Action::Throttle { pct: 5 },
            mode: Mode::Live,
            reason: dozed_core::Reason::new(dozed_core::ReasonKind::RuleFired, "hidden for 20m"),
            grace: Some("30s".into()),
            elapsed: Duration::from_secs(1250),
            evidence: "desktop=signal".into(),
            cgroup: None,
            rules: Vec::new(),
            top_processes: vec![
                proc(101, "renderer", 2.5),
                proc(102, "utility", 2.3),
                proc(103, "gpu-process", 0.8),
                proc(104, "main", 0.3),
            ],
        };
        let out = plain_render(&Response::Why(Box::new(why)));
        assert!(out.contains("busiest processes"), "{out}");
        assert!(out.contains("[renderer]"), "{out}");
        assert!(out.contains("[utility]"), "{out}");
        // Busiest first, so the answer is the first row rather than a search.
        let renderer = out.find("[renderer]").unwrap();
        let main = out.find("[main]").unwrap();
        assert!(renderer < main, "busiest first: {out}");
    }

    #[test]
    fn an_app_that_is_not_a_process_zoo_gets_no_role_column() {
        // A single-process app has no roles to distinguish, and `[main]` on every
        // row of it would be noise.
        let why = dozed_core::ipc::WhyReport {
            key: dozed_core::AppKey::desktop("alacritty"),
            label: "alacritty".into(),
            visibility: dozed_core::Visibility::Hidden,
            applied: Action::Restore,
            mode: Mode::DryRun,
            reason: dozed_core::Reason::new(dozed_core::ReasonKind::NoRule, "no rule"),
            grace: None,
            elapsed: Duration::from_secs(5),
            evidence: "desktop=alacritty".into(),
            cgroup: None,
            rules: Vec::new(),
            top_processes: vec![dozed_core::ProcContrib {
                pid: 7,
                comm: "alacritty".into(),
                wakeups_per_sec: 12.0,
                role: String::new(),
            }],
        };
        let out = plain_render(&Response::Why(Box::new(why)));
        assert!(out.contains("alacritty"), "{out}");
        assert!(!out.contains('['), "no empty role brackets: {out}");
    }

    #[test]
    fn a_machine_that_has_never_run_a_trial_is_told_how_to() {
        let out = plain_render(&Response::Trial { report: None });
        assert!(out.contains("dozectl trial start"), "{out}");
    }

    #[test]
    fn the_trial_output_shows_the_verdict_and_every_block() {
        // The block table is not decoration: "why is my trial not finishing" is
        // answered by the discard column and by nothing else.
        let mut t = trial_report(6, 6.0, 5.0);
        t.blocks[3].discarded = Some(dozed_core::trial::Discard::UserActive);
        let out = plain_render(&Response::Trial { report: Some(Box::new(t)) });
        assert!(out.contains("measured saving"), "{out}");
        assert!(out.contains("PAIR"), "{out}");
        assert!(out.contains("dropped: user active"), "{out}");
        assert!(out.contains("of a charge per idle hour"), "{out}");
    }

    #[test]
    fn a_trial_that_measured_nothing_does_not_render_a_saving() {
        // Four pairs cannot clear p < 0.05 even with perfect data, so this also
        // pins that the CLI reports the arithmetic rather than the direction.
        let t = trial_report(4, 6.0, 5.0);
        let out = plain_render(&Response::Trial { report: Some(Box::new(t)) });
        assert!(out.contains("no measurable difference"), "{out}");
        assert!(!out.contains("measured saving"), "{out}");
    }

    #[test]
    fn status_says_when_the_configured_mode_is_not_the_one_in_force() {
        let mut s = status_report();
        s.mode = Mode::DryRun;
        s.configured_mode = Some(Mode::Live);
        let out = plain_render(&Response::Status(Box::new(s)));
        assert!(out.contains("mode          dry-run (config says live"), "{out}");
        assert!(out.contains("without --live"), "the reason, not just the discrepancy: {out}");

        // And no parenthetical when they agree.
        let mut s = status_report();
        s.configured_mode = None;
        let out = plain_render(&Response::Status(Box::new(s)));
        assert!(out.contains("mode          dry-run\n"), "{out}");
        assert!(!out.contains("config says"), "{out}");
    }

    #[test]
    fn status_only_promises_a_retry_when_there_is_going_to_be_one() {
        // Printed on a real machine for a day: "STOPPED: hit the 67108864 byte
        // size cap; retrying every 60s". This line was appending the retry text
        // to every stop reason it was given, because it was reading the reason
        // string instead of asking.
        let mut s = status_report();
        s.trace = Some(dozed_core::ipc::TraceStatus {
            path: "/tmp/t.jsonl".into(),
            bytes: 67_108_864,
            records: 341_955,
            part: 1,
            stopped: Some(
                "could not open the next trace file: No space left on device (os error 28)".into(),
            ),
            gaps: 0,
            lost: 0,
            retrying: false,
        });
        let out = plain_render(&Response::Status(Box::new(s.clone())));
        assert!(out.contains("STOPPED: could not open the next trace file"), "{out}");
        assert!(!out.contains("retrying"), "a promise nothing will keep: {out}");
        // Still a condition rather than a moment. A stop nothing is going to
        // lift has to say so, or "STOPPED" reads as something that happened once
        // and is over.
        assert!(out.contains("not retried before the next restart"), "{out}");
        // And the raw byte count is gone with it: everything else on that line is
        // human-formatted, and `67108864` was the one number that was not.
        assert!(!out.contains("67108864"), "{out}");

        // The recoverable case still says so, because there it is true.
        let t = s.trace.as_mut().expect("just set");
        t.stopped = Some("flush failed: No space left on device (os error 28)".into());
        t.retrying = true;
        // And what the stop is costing, wherever that is known. This was
        // attached only to the arm that was *not* being retried, so it vanished
        // in the one case where the number is still going up.
        t.lost = 266_947;
        let out = plain_render(&Response::Status(Box::new(s)));
        assert!(out.contains("retrying every 60s"), "{out}");
        assert!(out.contains("266947 records unrecorded since"), "{out}");
    }

    #[test]
    fn status_explains_a_mode_a_trial_left_behind() {
        // A trial's blocks are twenty minutes and the waits between them are
        // hours, so `mode_driven_by_trial` is false for most of its life. That
        // left the common case unexplained: a bare `mode dry-run` with nothing
        // connecting it to the `trial 4/12 blocks` line further down. On a real
        // machine that was 89% of an eight-hour stretch.
        let mut s = status_report();
        s.mode = Mode::DryRun;
        s.mode_driven_by_trial = false;
        s.trial_restores = Some(Mode::DryRun);
        let out = plain_render(&Response::Status(Box::new(s.clone())));
        assert!(out.contains("between trial blocks"), "{out}");
        assert!(out.contains("puts back each time a block ends"), "{out}");

        // While a block is running the existing line still wins: that one is
        // about the mode being taken, this one about it being given back.
        let mut running = s.clone();
        running.mode_driven_by_trial = true;
        let out = plain_render(&Response::Status(Box::new(running)));
        assert!(out.contains("set by the running trial block"), "{out}");
        assert!(!out.contains("between trial blocks"), "{out}");

        // A mode that is not the baseline says what is coming instead.
        let mut live = s.clone();
        live.mode = Mode::Live;
        let out = plain_render(&Response::Status(Box::new(live)));
        assert!(out.contains("will put the mode back to `dry-run`"), "{out}");

        // And with no trial at all, nothing is said.
        s.trial_restores = None;
        let out = plain_render(&Response::Status(Box::new(s)));
        assert!(!out.contains("trial block"), "silent when there is no trial: {out}");
    }

    #[test]
    fn status_says_when_the_crash_recovery_file_cannot_be_written() {
        // §10.3 makes that file mandatory: a freeze outlives the process that
        // applied it. A full disk broke it on a real machine and said so only in
        // a journal line that scrolled past.
        let mut s = status_report();
        s.frozen_file_error = Some("No space left on device (os error 28)".into());
        let out = plain_render(&Response::Status(Box::new(s.clone())));
        assert!(out.contains("held record"), "{out}");
        assert!(out.contains("No space left"), "{out}");
        assert!(out.contains("frozen until the next startup sweep"), "the consequence: {out}");

        s.frozen_file_error = None;
        let out = plain_render(&Response::Status(Box::new(s)));
        assert!(!out.contains("held record"), "silent while the net is intact: {out}");
    }

    #[test]
    fn status_shows_the_power_meter_and_the_trial() {
        let mut s = status_report();
        let out = plain_render(&Response::Status(Box::new(s.clone())));
        assert!(out.contains("power meter"), "{out}");
        assert!(out.contains("6.40 W discharge"), "{out}");
        // Both wattages can be present at once, and they are different
        // quantities: the package is part of what the battery is paying for.
        assert!(out.contains("3.10 W cpu package"), "{out}");
        assert!(out.contains("82.0% deep idle"), "{out}");
        assert!(out.contains("trial         3/12"), "{out}");

        // On mains the battery reports nothing at all, which is not zero watts,
        // and RAPL is unreadable on a stock kernel.
        s.energy = Some(dozed_core::ipc::EnergyStatus {
            discharge_w: None,
            package_w: None,
            package_denied: false,
            deepest_idle: None,
            system_wakeups_per_sec: None,
            cpus: 16,
            package_parts: dozed_core::energy::PackageBreakdown::default(),
            gpu_w: None,
            gpu_label: None,
            machine_cores_busy: None,
            power_model: None,
            gpu_busy: None,
        });
        let out = plain_render(&Response::Status(Box::new(s.clone())));
        assert!(out.contains("no reading yet"), "{out}");
        assert!(!out.contains("0.00 W"), "a missing reading must not render as zero: {out}");
        assert!(
            !out.contains("unreadable"),
            "and a machine with no RAPL at all is owed no explanation: {out}"
        );

        // But a counter that is *there* and shut is worth saying, because it is the
        // difference between "this machine cannot" and "this machine can, and one
        // decision away". Found by hitting exactly that silence: a correct udev
        // rule, a correct build, and no way to tell which of the two was wrong.
        s.energy = Some(dozed_core::ipc::EnergyStatus {
            discharge_w: None,
            package_w: None,
            package_denied: true,
            deepest_idle: Some(0.77),
            system_wakeups_per_sec: Some(16_348.0),
            cpus: 16,
            package_parts: dozed_core::energy::PackageBreakdown::default(),
            gpu_w: None,
            gpu_label: None,
            machine_cores_busy: None,
            power_model: None,
            gpu_busy: None,
        });
        let out = plain_render(&Response::Status(Box::new(s)));
        assert!(out.contains("cpu package unreadable"), "{out}");
        assert!(out.contains("77.0% deep idle"), "beside the readings, not instead of: {out}");
    }

    fn status_report() -> dozed_core::ipc::StatusReport {
        dozed_core::ipc::StatusReport {
            mode: Mode::DryRun,
            configured_mode: None,
            mode_driven_by_trial: false,
            trial_restores: None,
            uptime: Duration::from_secs(60),
            now: dozed_core::MonoTime::ZERO,
            power: dozed_core::PowerState::default(),
            desktop: dozed_core::DesktopState::IdlePresent,
            focus_source_healthy: true,
            session_bus_healthy: true,
            compositor: Some("kwin".into()),
            focus_source: None,
            apps_known: 3,
            apps_held: 1,
            snooze_remaining: None,
            self_wakeups_per_sec: 0.4,
            bus_calls_per_sec: Some(0.2),
            wakeup_counter: Some("schedstat".into()),
            frozen_file_error: None,
            config_path: None,
            trace: None,
            exemption_sources: Vec::new(),
            energy: Some(dozed_core::ipc::EnergyStatus {
                discharge_w: Some(6.4),
                package_w: Some(3.1),
                package_denied: false,
                deepest_idle: Some(0.82),
                system_wakeups_per_sec: Some(1_400.0),
                cpus: 16,
                package_parts: dozed_core::energy::PackageBreakdown::default(),
                gpu_w: None,
                gpu_label: None,
                machine_cores_busy: None,
                power_model: None,
                gpu_busy: None,
            }),
            trial: Some(dozed_core::ipc::TrialSummary {
                blocks_done: 3,
                blocks_total: 12,
                arm: Some("live".into()),
                waiting: false,
            }),
            thermal: Some(dozed_core::ipc::ThermalStatus {
                state: dozed_core::ThermalState::Hot,
                hottest_celsius: Some(99.4),
                fastest_rpm: Some(4473),
                saturated: false,
                fan: None,
                ignored: Vec::new(),
            }),
        }
    }

    #[test]
    fn since_accepts_human_durations() {
        assert_eq!(parse_since(Some("24h")).unwrap(), Some(Duration::from_secs(86_400)));
        assert_eq!(parse_since(Some("30m")).unwrap(), Some(Duration::from_secs(1800)));
        assert_eq!(parse_since(None).unwrap(), None);
        // A bare number is rejected upstream, so 5 does not silently mean 5ms.
        assert!(parse_since(Some("5")).is_err());
    }

    #[test]
    fn thaw_requires_all() {
        // `dozectl thaw` on its own must not be interpretable as anything.
        let err = Args::try_parse_from(["dozectl", "thaw"]).map(|_| ()).err();
        assert!(err.is_none(), "clap accepts it; the runtime check rejects it");
    }

    /// Where a column starts, counted in characters. Byte offsets would not do:
    /// `truncate` appends `…`, which is one column and three bytes.
    fn col_of(line: &str, needle: &str) -> usize {
        let line = strip_ansi(line);
        line[..line.find(needle).expect(needle)].chars().count()
    }

    /// Render with any colour taken back out.
    ///
    /// [`paint`]'s switch is a process-wide global and the suite runs on threads
    /// of one process, so the two tests that turn colour on are recolouring
    /// every table the others are asserting against at that moment. Every
    /// assertion about *layout or wording* therefore goes through here, and the
    /// two that are about colour itself compare a stripped render against a
    /// plain one instead.
    fn plain_render(response: &Response) -> String {
        strip_ansi(&render(response))
    }

    fn journal_entry(
        at_ms: u64,
        wall: u64,
        action: Action,
        outcome: dozed_core::Outcome,
    ) -> dozed_core::JournalEntry {
        dozed_core::JournalEntry {
            at: dozed_core::MonoTime::from_millis(at_ms),
            wall,
            app: dozed_core::AppKey::desktop("firefox"),
            action,
            reason: dozed_core::Reason::new(dozed_core::ReasonKind::RuleFired, "hidden for 4m"),
            outcome,
        }
    }

    #[test]
    fn a_row_carries_both_clocks() {
        // The wall clock is the point of the column: `at` alone cannot say when
        // this happened, because it restarts with the daemon.
        let row = journal_row(&journal_entry(
            801_454,
            1_786_004_968,
            Action::Restore,
            dozed_core::Outcome::Applied,
        ));
        assert!(row.contains("+801.454s"), "{row}");
        assert!(row.contains("firefox"), "{row}");
        assert!(row.contains("restored"), "{row}");
        assert!(row.contains("hidden for 4m"), "{row}");
        // Rendered in local time, so assert the shape rather than the value.
        let clock = &row[..8];
        assert!(
            clock.len() == 8 && clock.as_bytes()[2] == b':' && clock.as_bytes()[5] == b':',
            "expected HH:MM:SS, got {clock:?}"
        );
    }

    #[test]
    fn the_widest_verb_still_lines_up_with_the_header() {
        // `would throttle` is the longest ACTION, and in dry-run every row uses
        // a `would …` verb — so if the column is sized for the live verbs, a
        // whole dry-run journal sits out of alignment.
        let row = journal_row(&journal_entry(
            1000,
            1_786_004_968,
            Action::Throttle { pct: 5 },
            dozed_core::Outcome::WouldHave,
        ));
        assert!(row.contains("would throttle"), "{row}");
        assert_eq!(
            col_of(JOURNAL_HEADER, "WHY"),
            col_of(&row, "hidden for 4m"),
            "WHY column drifted from its header:\n{JOURNAL_HEADER}\n{row}"
        );
        assert_eq!(col_of(JOURNAL_HEADER, "ACTION"), col_of(&row, "would throttle"));
    }

    /// The `WAKES/s` cell of a rendered row, read at the offset its header
    /// claims. Checking the whole line would not do: the clock and `ELAPSED`
    /// are full of digits, so "is there a number here" has to mean *here*.
    fn wakes_cell(row: &str) -> String {
        let start = col_of(JOURNAL_HEADER, "WAKES/s");
        strip_ansi(row)
            .chars()
            .skip(start)
            .take("WAKES/s".len())
            .collect::<String>()
            .trim()
            .to_owned()
    }

    /// The rendered app table, for tests that are about the rows rather than
    /// the coverage footer. Default `MachineTotals` is what a daemon one tick
    /// old sends: no rate yet, so the footer prints the sums alone.
    fn apps_table(apps: Vec<ipc::AppSummary>) -> String {
        strip_ansi(&apps_table_raw(apps))
    }

    /// The same table with whatever colour is in force left on it, for the one
    /// test that is about the colour rather than about the table.
    fn apps_table_raw(apps: Vec<ipc::AppSummary>) -> String {
        render(&Response::Apps { apps, machine: dozed_core::coverage::MachineTotals::default() })
    }

    fn with_sample(
        mut e: dozed_core::JournalEntry,
        wakeups_per_sec: f32,
    ) -> dozed_core::JournalEntry {
        e.reason.inputs = Some(dozed_core::DecisionInputs {
            visibility: dozed_core::Visibility::Hidden,
            class: dozed_core::AppClass::Normal,
            protected: false,
            exemptions: Vec::new(),
            sample: dozed_core::ProcSample { cpu_pct: 1.0, wakeups_per_sec, gpu_pct: None },
            desktop: dozed_core::DesktopState::Active,
            power: dozed_core::PowerState::default(),
            in_cooldown: false,
            cooldown_bypassed: false,
            thermal: dozed_core::ThermalState::Normal,
            other_app_fullscreen: false,
        });
        e
    }

    #[test]
    fn an_unmeasured_rate_is_blank_and_a_measured_zero_is_zero() {
        // The distinction is the whole point of the column. A global restore
        // carries no `DecisionInputs`, and one doze in five is of an app that
        // really was at zero — printing `0` for both would merge "nothing to
        // measure" into "measured nothing", which is the judgement the column
        // exists to support.
        let doze = journal_entry(
            1000,
            1_786_004_968,
            Action::Freeze { reclaim: false },
            dozed_core::Outcome::Applied,
        );
        assert_eq!(wakes_cell(&journal_row(&doze)), "", "an absent sample must print nothing");
        assert_eq!(
            wakes_cell(&journal_row(&with_sample(doze, 0.0))),
            "0",
            "a doze of an already-quiet app measured zero, and must say so"
        );
    }

    #[test]
    fn a_restore_reading_zero_shows_a_dash_not_a_zero() {
        // Zero on a restore is the freeze being undone, not the app being
        // quiet: the sample was taken while the app was held, so zero was the
        // only answer it could give. Reporting it as a measurement invites the
        // reader to conclude the app was cheap all along.
        let base =
            journal_entry(1000, 1_786_004_968, Action::Restore, dozed_core::Outcome::Applied);
        assert_eq!(wakes_cell(&journal_row(&with_sample(base.clone(), 0.0))), "-");
        // Rounding must not smuggle a bare zero back in.
        assert_eq!(wakes_cell(&journal_row(&with_sample(base.clone(), 0.4))), "-");
        // A restore that really did measure something keeps its number: 32 of
        // 54 restores in one real journal were non-zero, the busiest at 948/s.
        assert_eq!(wakes_cell(&journal_row(&with_sample(base.clone(), 947.9))), "948");
        // And no sample at all is still blank, not a dash.
        assert_eq!(wakes_cell(&journal_row(&base)), "");
    }

    #[test]
    fn a_measured_rate_is_shown_whole_and_stays_in_column() {
        // Whole wakeups/s, matching how `dozectl trace` renders the same value.
        let row = journal_row(&with_sample(
            journal_entry(
                1000,
                1_786_004_968,
                Action::Freeze { reclaim: false },
                dozed_core::Outcome::Applied,
            ),
            3466.4,
        ));
        assert!(row.contains("3466"), "{row}");
        assert!(!row.contains("3466.4"), "decimals are noise at this scale: {row}");
        assert_eq!(
            col_of(JOURNAL_HEADER, "WHY"),
            col_of(&row, "hidden for 4m"),
            "a measured rate shifted WHY off its header:\n{JOURNAL_HEADER}\n{row}"
        );
    }

    #[test]
    fn an_overlong_app_name_does_not_shift_the_columns() {
        let mut e =
            journal_entry(1000, 1_786_004_968, Action::Restore, dozed_core::Outcome::Applied);
        e.app = dozed_core::AppKey::desktop("org.wezfurlong.wezterm.averylongsuffix");
        let row = journal_row(&e);
        assert_eq!(
            col_of(JOURNAL_HEADER, "WHY"),
            col_of(&row, "hidden for 4m"),
            "a long app name pushed WHY off its header:\n{JOURNAL_HEADER}\n{row}"
        );
    }

    #[test]
    fn a_qualifier_travels_with_the_reason() {
        // "throttled" and "throttled [skipped: …]" mean opposite things, so the
        // qualifier must never be dropped when the row is split into columns.
        let row = journal_row(&journal_entry(
            1000,
            1_786_004_968,
            Action::Throttle { pct: 5 },
            dozed_core::Outcome::Skipped { why: "no cgroup scope".into() },
        ));
        assert!(row.contains("[skipped: no cgroup scope]"), "{row}");
    }

    #[test]
    fn state_names_read_naturally() {
        assert_eq!(state_of(Action::Restore), "running");
        assert_eq!(state_of(Action::Freeze { reclaim: true }), "dozing");
    }

    #[test]
    fn long_app_names_are_truncated_not_wrapped() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("averyveryverylongname", 10).chars().count(), 10);
    }

    #[test]
    fn an_app_with_no_declared_identity_says_so_in_the_table() {
        // A window the compositor gave no app_id and no desktop file for is filed
        // under a name we derived, and the APP column shows only the basename —
        // so nothing on the row hints that `app = "libreoffice"` will not match
        // it. Observed: LibreOffice known only as
        // `exe:/usr/lib/libreoffice/program/oosplash`.
        let app = |key: dozed_core::AppKey| ipc::AppSummary {
            key,
            label: "oosplash".into(),
            visibility: dozed_core::Visibility::Hidden,
            applied: Action::Restore,
            mode: Mode::DryRun,
            sample: dozed_core::ProcSample::default(),
            sampled: true,
            pids: 3,
            exempt: Vec::new(),
            held_off: None,
            never_actionable: false,
            audio_streams: Vec::new(),
            top_processes: Vec::new(),
            pending: None,
        };

        let weak =
            apps_table(vec![app(dozed_core::AppKey::exe("/usr/lib/libreoffice/program/oosplash"))]);
        assert!(weak.contains("exe-only"), "{weak}");

        let comm = apps_table(vec![app(dozed_core::AppKey::comm("oosplash"))]);
        assert!(comm.contains("comm-only"), "{comm}");

        // A declared identity is the normal case and must stay unannotated, or
        // the column is noise on every row.
        let declared = apps_table(vec![app(dozed_core::AppKey::desktop("firefox"))]);
        assert!(!declared.contains("-only"), "{declared}");

        // An app_id is inferred but still a name a rule can be written against,
        // and most Wayland apps only ever report one.
        let app_id =
            apps_table(vec![app(dozed_core::AppKey::new(IdentityKind::AppId, "firefox-bin"))]);
        assert!(!app_id.contains("-only"), "{app_id}");
    }

    /// Colour is an overlay on the table, never a change to it.
    ///
    /// Both halves matter. Off has to be byte-for-byte what it always was,
    /// because `dozectl apps | grep dozing` and every script anybody has written
    /// against this depends on it; on has to leave the columns where they were,
    /// which is the failure mode of colouring before padding rather than after.
    #[test]
    fn colour_is_an_overlay_and_does_not_move_a_column() {
        let row = |applied| ipc::AppSummary {
            key: dozed_core::AppKey::desktop("firefox"),
            label: "firefox".into(),
            visibility: dozed_core::Visibility::Hidden,
            applied,
            mode: Mode::Live,
            sample: dozed_core::ProcSample::default(),
            sampled: true,
            pids: 40,
            exempt: vec!["audio-silent".to_owned()],
            held_off: None,
            never_actionable: false,
            audio_streams: Vec::new(),
            top_processes: Vec::new(),
            pending: None,
        };

        // Both renders take `paint`'s lock, and in separate scopes: it is not
        // reentrant, and holding it across the other render would deadlock.
        let plain = {
            let _off = paint::force(false);
            apps_table_raw(vec![row(Action::Freeze { reclaim: false })])
        };
        assert!(!plain.contains('\x1b'), "no terminal, no escapes: {plain:?}");

        let coloured = {
            let _on = paint::force(true);
            apps_table_raw(vec![row(Action::Freeze { reclaim: false })])
        };
        assert!(coloured.contains('\x1b'), "{coloured:?}");
        // Strip every escape back out and the two must be identical: same
        // columns, same widths, same words.
        let stripped = strip_ansi(&coloured);
        assert_eq!(stripped, plain, "colour changed the table, not just its colour");

        // And the state that is not held carries no colour of its own, so a
        // table of running apps is as quiet as it was before.
        let _on = paint::force(true);
        assert!(!paint_state(Action::Restore, 12).contains('\x1b'));
        assert!(paint_state(Action::Freeze { reclaim: false }, 12).contains('\x1b'));
    }

    /// Every ANSI SGR sequence removed, so a coloured render can be compared
    /// against a plain one.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            // `ESC [ … m` — the only form this crate emits.
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        }
        out
    }

    /// The bug this closes was reported by a person, not a test: `plasmashell`
    /// appeared in the dashboard and there was no way to tell it apart from an
    /// app about to be frozen.
    #[test]
    fn an_app_that_can_never_be_touched_does_not_say_running() {
        let mut a = ipc::AppSummary {
            key: dozed_core::AppKey::desktop("org.kde.plasmashell"),
            label: "org.kde.plasmashell".into(),
            visibility: dozed_core::Visibility::Hidden,
            // The applied action of a protected app is Restore, exactly like an
            // untouched ordinary app — which is why the action alone cannot
            // answer this and the flag has to travel.
            applied: Action::Restore,
            mode: Mode::Live,
            sample: dozed_core::ProcSample::default(),
            sampled: true,
            pids: 1,
            exempt: Vec::new(),
            held_off: None,
            never_actionable: true,
            audio_streams: Vec::new(),
            top_processes: Vec::new(),
            pending: None,
        };
        let out = apps_table(vec![a.clone()]);
        assert!(out.contains("protected"), "{out}");
        assert!(!out.contains("running"), "`running` is the answer to a different question: {out}");

        // And an ordinary app with the identical applied action still reads as
        // running, so the new word cannot leak onto rows it is not about.
        a.never_actionable = false;
        let ordinary = apps_table(vec![a]);
        assert!(ordinary.contains("running"), "{ordinary}");
        assert!(!ordinary.contains("protected"), "{ordinary}");
    }

    #[test]
    fn a_hand_set_limit_is_the_first_thing_in_the_notes() {
        let mut a = ipc::AppSummary {
            key: dozed_core::AppKey::desktop("firefox"),
            label: "firefox".into(),
            visibility: dozed_core::Visibility::Hidden,
            applied: Action::Throttle { pct: 5 },
            mode: Mode::Live,
            sample: dozed_core::ProcSample::default(),
            sampled: true,
            pids: 1,
            exempt: vec!["audio-silent".to_owned()],
            held_off: Some("capped only, 42m left".to_owned()),
            never_actionable: false,
            audio_streams: Vec::new(),
            top_processes: Vec::new(),
            pending: None,
        };
        let out = apps_table(vec![a.clone()]);
        assert!(out.contains("capped only, 42m left"), "{out}");
        // Ahead of the exemptions: it is the only note somebody put there on
        // purpose, and the only one with a clock on it.
        let held = out.find("capped only").expect("present");
        let exempt = out.find("audio-silent").expect("present");
        assert!(held < exempt, "the hand-set limit reads first: {out}");

        a.held_off = None;
        assert!(!apps_table(vec![a]).contains("capped only"));
    }

    #[test]
    fn a_duration_of_no_time_at_all_is_refused_rather_than_accepted() {
        // A ceiling that lapses on the tick it was set is a command that looked
        // like it worked and did nothing.
        assert!(parse_for(Some("0s")).is_err());
        assert!(parse_for(Some("nonsense")).is_err());
        assert_eq!(parse_for(None).expect("omitted means until cleared"), None);
        assert_eq!(
            parse_for(Some("30m")).expect("a real duration"),
            Some(std::time::Duration::from_secs(1800))
        );
    }

    #[test]
    fn notes_still_carry_the_mode_and_the_exemptions() {
        // The identity note is additive; it must not displace what the column
        // was already for.
        let mut a = ipc::AppSummary {
            key: dozed_core::AppKey::exe("/usr/bin/thing"),
            label: "thing".into(),
            visibility: dozed_core::Visibility::Hidden,
            applied: Action::Restore,
            mode: Mode::Monitor,
            sample: dozed_core::ProcSample::default(),
            sampled: true,
            pids: 1,
            exempt: vec!["audio".to_owned()],
            held_off: None,
            never_actionable: false,
            audio_streams: Vec::new(),
            top_processes: Vec::new(),
            pending: None,
        };
        let out = apps_table(vec![a.clone()]);
        for expected in ["monitor", "exe-only", "audio"] {
            assert!(out.contains(expected), "{expected} missing from {out}");
        }

        a.mode = Mode::DryRun;
        a.key = dozed_core::AppKey::desktop("thing");
        let plain = apps_table(vec![a]);
        assert!(plain.contains("audio"), "{plain}");
        assert!(!plain.contains("monitor"), "{plain}");
    }

    /// A row that says "running" says nothing about the freeze thirty seconds
    /// away from it, which is the question `dozectl apps` is usually being
    /// asked at the moment somebody runs it.
    #[test]
    fn a_row_that_is_about_to_change_says_so() {
        let mut a = ipc::AppSummary {
            key: dozed_core::AppKey::desktop("firefox"),
            label: "firefox".into(),
            visibility: dozed_core::Visibility::Hidden,
            applied: Action::Restore,
            mode: Mode::Live,
            sample: dozed_core::ProcSample::default(),
            sampled: true,
            pids: 40,
            exempt: Vec::new(),
            held_off: None,
            never_actionable: false,
            audio_streams: Vec::new(),
            top_processes: Vec::new(),
            pending: Some(ipc::PendingChange {
                within: std::time::Duration::from_secs(134),
                rule: "bg-idle".into(),
                action: dozed_core::RuleAction::Freeze,
            }),
        };
        let out = apps_table(vec![a.clone()]);
        assert!(out.contains("dozes in 2m14s"), "{out}");

        // Nothing pending is the ordinary case and must leave the column
        // exactly as it was: a note on every row is a note nobody reads.
        a.pending = None;
        let quiet = apps_table(vec![a]);
        assert!(!quiet.contains("dozes in"), "{quiet}");
    }

    /// The footer sits *below* a blank line, outside the table. Inside it, the
    /// sum would align as though it were an app called `these apps:`, and
    /// anything reading `dozectl apps` line by line would parse it as a row.
    #[test]
    fn the_coverage_footer_is_a_footer_and_not_a_table_row() {
        let busy = ipc::AppSummary {
            key: dozed_core::AppKey::desktop("firefox"),
            label: "firefox".into(),
            visibility: dozed_core::Visibility::Focused,
            applied: Action::Restore,
            mode: Mode::Live,
            sample: dozed_core::ProcSample {
                cpu_pct: 120.0,
                wakeups_per_sec: 900.0,
                gpu_pct: None,
            },
            sampled: true,
            pids: 40,
            exempt: Vec::new(),
            held_off: None,
            never_actionable: false,
            audio_streams: Vec::new(),
            top_processes: Vec::new(),
            pending: None,
        };
        let out = plain_render(&Response::Apps {
            apps: vec![busy],
            machine: dozed_core::coverage::MachineTotals {
                wakeups_per_sec: Some(3_600.0),
                cpus: 16,
                sample_window: Duration::from_secs(60),
                basis: None,
                gpu_busy: None,
            },
        });

        let (table, footer) =
            out.split_once("\n\n").expect("a blank line separates the table from the footer");
        assert!(table.contains("firefox"), "{table}");
        assert!(!table.contains("these apps"), "the footer is not part of the table:\n{table}");
        assert_eq!(
            footer,
            "these apps: 1.2 of 16 cores busy, 900/s of 3600/s system-wide wakeups (25%)\n\
             the rest has no window: daemons, kernel threads, and dozed itself\n"
        );
    }

    /// A fresh daemon has looked at an app without having measured it yet, and
    /// `0.0 / 0` for that is a lie in the most alarming direction: every row
    /// zero on a busy machine reads as a broken tool. Same `-` the journal's
    /// WAKES/s column uses, for the same reason.
    #[test]
    fn an_app_with_no_sample_yet_shows_a_dash_not_a_zero() {
        let mut app = ipc::AppSummary {
            key: dozed_core::AppKey::desktop("firefox"),
            label: "firefox".into(),
            visibility: dozed_core::Visibility::Hidden,
            applied: Action::Restore,
            mode: Mode::Live,
            sample: dozed_core::ProcSample::default(),
            sampled: false,
            pids: 40,
            exempt: Vec::new(),
            held_off: None,
            never_actionable: false,
            audio_streams: Vec::new(),
            top_processes: Vec::new(),
            pending: None,
        };
        let machine = dozed_core::coverage::MachineTotals {
            wakeups_per_sec: Some(40_000.0),
            cpus: 16,
            sample_window: Duration::from_secs(60),
            basis: None,
            gpu_busy: None,
        };

        let out = plain_render(&Response::Apps { apps: vec![app.clone()], machine });
        let row = out.lines().find(|l| l.starts_with("firefox")).unwrap();
        assert!(row.contains(" - "), "expected dashes, got: {row}");
        assert!(!row.contains("0.0"), "a measured zero is a different claim: {row}");
        // And the coverage line must not report 0% of a busy machine on the
        // strength of placeholders — that is dozed's own "attribution is
        // broken" signal, produced here by nothing but a young daemon.
        assert!(out.contains("(1 not measured yet)"), "{out}");

        // A measured zero still prints as zero: some apps really are idle, and
        // that is the finding the column exists to show.
        app.sampled = true;
        let measured = plain_render(&Response::Apps { apps: vec![app], machine });
        let row = measured.lines().find(|l| l.starts_with("firefox")).unwrap();
        assert!(row.contains("0.0"), "{row}");
        assert!(!measured.contains("not measured yet"), "{measured}");
    }

    /// The first tick after a daemon start has one `/proc/stat` reading, which
    /// is not a rate. The rows are still worth summing.
    #[test]
    fn no_machine_reading_yet_still_foots_the_table() {
        let out = apps_table(vec![ipc::AppSummary {
            key: dozed_core::AppKey::desktop("firefox"),
            label: "firefox".into(),
            visibility: dozed_core::Visibility::Focused,
            applied: Action::Restore,
            mode: Mode::Live,
            sample: dozed_core::ProcSample { cpu_pct: 50.0, wakeups_per_sec: 300.0, gpu_pct: None },
            sampled: true,
            pids: 40,
            exempt: Vec::new(),
            held_off: None,
            never_actionable: false,
            audio_streams: Vec::new(),
            top_processes: Vec::new(),
            pending: None,
        }]);
        assert!(out.contains("these apps: 0.5 cores busy, 300/s wakeups"), "{out}");
        assert!(!out.contains("system-wide"), "nothing to compare against yet:\n{out}");
    }

    #[test]
    fn errors_render_visibly() {
        let r = Response::Error { message: "no such app".into() };
        assert!(render(&r).starts_with("error:"));
    }

    /// An app row, sampled at `cpu_pct`, for the attribution tests below.
    fn sampled_app(label: &str, cpu_pct: f32) -> ipc::AppSummary {
        ipc::AppSummary {
            key: dozed_core::AppKey::desktop(label),
            label: label.into(),
            visibility: dozed_core::Visibility::Hidden,
            applied: Action::Restore,
            mode: Mode::Live,
            sample: dozed_core::ProcSample { cpu_pct, wakeups_per_sec: 900.0, gpu_pct: None },
            sampled: true,
            pids: 4,
            exempt: Vec::new(),
            held_off: None,
            never_actionable: false,
            audio_streams: Vec::new(),
            top_processes: Vec::new(),
            pending: None,
        }
    }

    fn machine_with(
        basis: Option<dozed_core::attribution::Basis>,
    ) -> dozed_core::coverage::MachineTotals {
        dozed_core::coverage::MachineTotals {
            wakeups_per_sec: Some(3_600.0),
            cpus: 16,
            sample_window: Duration::from_secs(60),
            basis,
            gpu_busy: Some(0.21),
        }
    }

    /// RAPL is `0400 root` on a stock kernel, so most machines have no measured
    /// total to apportion — and a permanent column of `-` would be a promise the
    /// hardware cannot keep, printed on every invocation.
    #[test]
    fn the_watts_column_is_absent_where_there_is_nothing_to_apportion() {
        let out = plain_render(&Response::Apps {
            apps: vec![sampled_app("firefox", 120.0)],
            machine: machine_with(None),
        });
        assert!(!out.contains("EST W"), "{out}");
        assert!(!out.contains("est. watts"), "and no footer explaining a column nobody sees");
    }

    /// The column, its arithmetic, and the caveat that matters more than either.
    #[test]
    fn the_watts_column_travels_with_the_denominator_it_was_divided_by() {
        // 0.4 of the machine's 2.0 busy cores is a fifth of the 4.00 W measured.
        let basis = dozed_core::attribution::Basis::of(Some(4.0), Some(24.0), Some(2.0));
        let out = plain_render(&Response::Apps {
            apps: vec![sampled_app("firefox", 40.0)],
            machine: machine_with(basis),
        });
        let row = out.lines().find(|l| l.starts_with("firefox")).unwrap();
        assert!(row.contains("0.80"), "a fifth of 4.00 W: {row}");
        // §11.1's rule: a modelled figure that does not carry its denominator is
        // a number nobody can check.
        assert!(out.contains("4.00 W measured core"), "{out}");
        assert!(out.contains("2.00 of the machine's cores busy"), "{out}");
        assert!(out.contains("an estimate, not a measurement"), "{out}");
        // The bias runs against exactly the app this tool exists to catch, so it
        // is on screen and not only in the design document.
        assert!(out.contains("Judge on wakeups/s"), "{out}");
        // And what no app in the list accounts for, rather than a column that
        // silently fails to add up to its own header.
        assert!(out.contains("3.20 W of that goes to what has no window"), "{out}");
    }

    /// Three different reasons for a dash, and none of them is "0.00 W".
    #[test]
    fn an_app_with_no_honest_share_shows_a_dash_rather_than_zero_watts() {
        let basis = dozed_core::attribution::Basis::of(Some(4.0), Some(24.0), Some(2.0));

        // Sampled at more CPU than `/proc/stat` saw the whole machine use — the
        // benign window race `Coverage::leads` documents. Clamping would hand
        // this app the entire measured total and render the guess as a fact.
        let out = plain_render(&Response::Apps {
            apps: vec![sampled_app("firefox", 300.0)],
            machine: machine_with(basis),
        });
        let row = out.lines().find(|l| l.starts_with("firefox")).unwrap();
        assert!(row.trim_end().ends_with('-'), "expected a dash: {row}");

        // Never sampled at all: nothing to take a share of.
        let mut fresh = sampled_app("firefox", 0.0);
        fresh.sampled = false;
        let out = plain_render(&Response::Apps { apps: vec![fresh], machine: machine_with(basis) });
        let row = out.lines().find(|l| l.starts_with("firefox")).unwrap();
        assert!(!row.contains("0.00"), "a measured zero is a different claim: {row}");
    }

    /// The GPU column is a measurement, and it appears only where something in
    /// the list can actually produce one. A column of dashes on a desktop nobody
    /// is rendering on is width spent to say "no".
    #[test]
    fn the_gpu_column_is_absent_where_nothing_holds_a_client() {
        let out = plain_render(&Response::Apps {
            apps: vec![sampled_app("firefox", 40.0)],
            machine: machine_with(None),
        });
        assert!(!out.contains("GPU%"), "{out}");

        let mut with_gpu = sampled_app("firefox", 40.0);
        with_gpu.sample.gpu_pct = Some(7.5);
        let out = plain_render(&Response::Apps {
            apps: vec![with_gpu, sampled_app("thunderbird", 1.0)],
            machine: machine_with(None),
        });
        assert!(out.contains("GPU%"), "{out}");
        let row = out.lines().find(|l| l.starts_with("firefox")).unwrap();
        assert!(row.contains("7.5"), "{row}");
        // The app with no DRM client gets a dash, not a zero: it has not done no
        // GPU work, it cannot do any.
        let row = out.lines().find(|l| l.starts_with("thunderbird")).unwrap();
        assert!(row.contains(" - "), "{row}");
        assert!(!row.contains("0.0"), "a measured zero is a different claim: {row}");
    }

    #[test]
    fn upgrading_presets_appends_without_disturbing_what_is_there() {
        let dir = std::env::temp_dir().join(format!("dozectl-upgrade-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        // A config from before `shared-window-exempt` existed, with a comment
        // its owner wrote and a rule they edited.
        let mut cfg = dozed_core::Config::default();
        cfg.rules.retain(|r| r.name != "shared-window-exempt");
        cfg.rules.iter_mut().find(|r| r.name == "bg-idle").unwrap().priority = 101;
        let mine = "# mine, do not touch\n";
        std::fs::write(&path, format!("{mine}{}", dozed_core::starter::annotate(&cfg))).unwrap();

        // Without --write nothing moves. The file is byte-identical, which is
        // the property that makes this safe to run out of curiosity.
        let before = std::fs::read_to_string(&path).unwrap();
        upgrade_presets(Some(&path), false, false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        upgrade_presets(Some(&path), true, false).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();

        // The comment survives, because this appends rather than regenerating.
        assert!(after.starts_with(mine), "a hand-written comment was lost:\n{after}");
        // The rule arrives, and the edited one keeps the priority it was given.
        let parsed = dozed_core::Config::parse(&after).expect("the result must parse");
        assert!(parsed.rules.iter().any(|r| r.name == "shared-window-exempt"));
        assert_eq!(
            parsed.rules.iter().find(|r| r.name == "bg-idle").unwrap().priority,
            101,
            "an edited rule is one the user has an opinion about"
        );
        // And the original is recoverable.
        assert_eq!(
            std::fs::read_to_string(path.with_extension("toml.bak-preupgrade")).unwrap(),
            before
        );

        // Idempotent: a second run has nothing to add and writes nothing.
        upgrade_presets(Some(&path), true, false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_config_that_does_not_parse_is_not_one_to_append_to() {
        // Appending rules to a broken file produces a differently broken file,
        // and the daemon is meanwhile running on the last config that parsed.
        let dir = std::env::temp_dir().join(format!("dozectl-upgrade-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "this is not toml [[[\n").unwrap();

        assert!(upgrade_presets(Some(&path), true, false).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "this is not toml [[[\n");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
