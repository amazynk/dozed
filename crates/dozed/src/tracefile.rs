//! Writing a session to disk as it happens.
//!
//! One file per daemon run, JSON lines, so a trace can be tailed, grepped and
//! replayed without a parser — and so a torn write at the end costs one record
//! rather than the file.
//!
//! The cost of this has to stay negligible or the tool becomes the problem it
//! exists to solve (`DESIGN.md` §13). Two things keep it there: writes are
//! buffered and flushed once per tick (at most every two seconds) rather than
//! per record, and the records themselves are edge-triggered — the planner emits
//! a derivation when it *changes*, not on every evaluation. `dozectl status`
//! reports the resulting file size so the claim can be checked rather than
//! believed.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use std::time::Duration;

use dozed_core::time::{HumanDuration, MonoTime};
use dozed_core::trace::{GapKind, TraceHeader, TraceRecord, is_trace_name, trace_file_name};

/// Roll over to a new file at this size rather than letting one grow without
/// bound. A day of ordinary desktop use is a few megabytes; anything
/// approaching this is a bug worth noticing, so it is logged loudly and once
/// per file.
///
/// This used to stop recording for the run, which was the wrong half of the
/// right idea. The bound that matters is [`MAX_TRACES_BYTES`] — the disk a
/// diagnostic may hold — and one file hitting a limit says nothing about that.
/// Worse, the two defects compounded: [`prune`] only runs when a trace is
/// created, so a recorder that never created another one never pruned again,
/// and a directory measured on a real machine sat 2.9 MB over its budget with
/// no recording going on at all. Rotating fixes both, because the thing that
/// bounds the directory is the same thing that resumes the recording.
const MAX_TRACE_BYTES: u64 = 64 * 1024 * 1024;

/// Bytes as a human reads them. Small and local: this is one message and one
/// status line, and `dozectl` has its own copy for its tables.
///
/// Binary units, *labelled* as binary units. They were divided by 1024 and
/// called `MB`, which made every number here disagree with the same number
/// anywhere else by 4.9%: `DESIGN.md` recorded one measured run as "67.1 MB,
/// hitting the 64 MB cap", two readings of the identical 67,108,864 bytes that
/// could not both be right. The cap is 64 MiB; saying so costs one character.
fn human_bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.0} GiB", n as f64 / (1u64 << 30) as f64),
        n if n >= 1 << 20 => format!("{:.0} MiB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.0} KiB", n as f64 / (1u64 << 10) as f64),
        n => format!("{n} bytes"),
    }
}

/// The most disk the whole trace directory may hold, *excluding the file being
/// written*.
///
/// [`prune`] exempts the newest entry, because deleting the recording in
/// progress to make room for nothing is not a saving — so the real ceiling is
/// this plus one [`MAX_TRACE_BYTES`], and calling it the cap on the directory
/// overstates it by a file. Measured on a live machine: 283.6 MiB on disk, a
/// 252.2 MiB retained set that is correctly under budget, plus a 31.4 MiB file
/// still being appended to.
///
/// [`MAX_TRACE_BYTES`] bounds one file and [`KEEP_TRACES`] bounds the count, and
/// neither bounds the product: ten runs at the file cap is 640 MB of somebody's
/// disk, held by a diagnostic nobody asked for. Measured on a real machine, a
/// run reached the file cap in 3.6 days, so this is reachable rather than
/// theoretical.
///
/// 256 MB is several days of sessions at the sizes now observed and still small
/// enough to sit unnoticed in a state directory. It is also the bound that does
/// the real work now that a long run rotates rather than stopping: one run can
/// reach the file cap repeatedly, so what holds the total down is this, applied
/// at every rotation.
const MAX_TRACES_BYTES: u64 = 256 * 1024 * 1024;

/// How many past runs to keep. Enough to compare this week against last, not
/// enough to accumulate unbounded.
const KEEP_TRACES: usize = 10;

/// How long to wait between attempts to start recording again.
///
/// A write or a flush that fails used to stop recording *for the run*, and
/// `DESIGN.md` §13 gives a good reason — a trace with unmarked gaps is worse
/// than no trace, because §14 replays it as though it were complete. That is
/// right about the gap and wrong about the remedy. `ENOSPC` is the case it was
/// written for, and a full disk is a condition that **ends**: somebody empties a
/// download folder at lunchtime and the recorder stays dead until the next
/// restart, having thrown away the afternoon as well as the morning.
///
/// So the gap is marked instead ([`dozed_core::trace::TraceRecord::Gap`]) and
/// this is how often we go back and try. Every minute, for as long as the daemon
/// runs — there is no attempt limit, because there is no number of failures that
/// makes the next attempt less worth making, and a recorder that gave up after
/// ten tries would be the original bug with extra steps.
///
/// A minute rather than a tick: a retry is an `open` plus a small write, and on
/// a machine whose disk is full the ordinary tick can be two seconds. Cheap is
/// not free, and the cost of being a minute late to resume is a minute of trace
/// against a hole that is already minutes long.
const RETRY_EVERY: Duration = Duration::from_secs(60);

pub struct TraceFile {
    writer: Option<BufWriter<File>>,
    path: PathBuf,
    /// The run's header, kept so a rotation can write it again.
    ///
    /// Every part of a run has to open with one or it does not replay: §14 reads
    /// a trace standalone, and a file that began mid-session with no version, no
    /// config and no `wall_start` has no way to turn its monotonic offsets back
    /// into times of day. `None` only for `--no-trace`.
    header: Option<Box<TraceHeader>>,
    /// Which file of this run [`Self::path`] is, counting from one.
    ///
    /// Reported, because `dozectl status` prints the current file's size as the
    /// evidence that recording is affordable, and after a rotation that number
    /// is small for a reason that has nothing to do with affordability.
    part: u32,
    /// The unix seconds in the current file's name.
    ///
    /// Names are chronological, which is what [`Self::list`] sorts on, and a
    /// rotation needs a name that sorts *after* the file it is continuing. The
    /// wall clock supplies that; this is kept only so the name can be forced
    /// past it in the degenerate case of two parts inside one second.
    part_wall: u64,
    seq: u64,
    /// Bytes in [`Self::path`] — this file, not the run. A rotation resets it,
    /// because what it answers is "how big is the file named beside it".
    bytes: u64,
    records: u64,
    /// Records that were a *conclusion* rather than an observation — a
    /// derivation or an intent. What makes a trace worth keeping: a session with
    /// none of these cannot be replayed into anything, tuned against, or
    /// compared with another. See [`TraceFile::drop`].
    conclusions: u64,
    /// Set when the size cap is hit **and** rotating past it failed, so the
    /// attempt is not repeated per record.
    ///
    /// Reaching the cap is ordinary — [`Self::rotate`] opens the next file and
    /// recording carries on — so this is not the cap but the one case where the
    /// cap still stops something: a next file that cannot be opened.
    ///
    /// It *is* retryable, and used not to be. The argument for never retrying
    /// was that the cap is "a limit dozed sets on itself rather than the world
    /// refusing", which was true while hitting the cap was itself the stop. It
    /// stopped being true the moment the cap learned to rotate: the only way to
    /// reach this flag now is an `open` the filesystem turned down — `ENOSPC`, a
    /// read-only remount, a directory that lost its permissions — and those are
    /// the world refusing by exactly the definition [`Self::go_down`] uses for a
    /// failed write. Leaving them alone meant a disk that filled and emptied
    /// revived a recorder that failed a *write* and left one that failed a
    /// *rotation* dead for the rest of the run: same fault, same cure, opposite
    /// outcomes, decided by which line of this file happened to notice first.
    ///
    /// So this is now a companion to [`Self::down_since`] rather than an
    /// alternative to it — both are set, and it says only *which* recovery to
    /// attempt: reopening this file would be wrong, because this file is full.
    capped: bool,
    /// Why recording stopped, when it stopped for a reason worth reporting.
    ///
    /// `None` while healthy. Kept as text rather than a flag because the whole
    /// value of it is in `dozectl status`: a trace that stopped at 200 KB
    /// otherwise looks exactly like a cheap one, and the number `dozectl status`
    /// prints to show that recording is affordable is the same number that goes
    /// *down* when it breaks.
    stopped: Option<String>,
    /// The `at` of the last record that reached **disk**.
    ///
    /// This module has no clock of its own and does not want one: every record
    /// it writes already carries the daemon's monotonic time, so the recorder
    /// can know when it last worked without reading anything. That is the right
    /// anchor for the start of a hole — the gap begins after the last thing
    /// successfully recorded, not whenever the next tick happens to notice.
    ///
    /// Advanced by a successful *flush*, not by a successful `writeln!`, and the
    /// difference is not pedantry: a `BufWriter` over a full disk accepts bytes
    /// happily and only fails when they are pushed, so the record that provokes
    /// the failure is itself one of the lost ones. Anchoring here to what was
    /// merely buffered would date the hole from a record that is not in the file,
    /// and under-report its length by exactly the interval nobody noticed.
    last_at: MonoTime,
    /// The `at` of the last record handed to the buffer, promoted to
    /// [`Self::last_at`] when a flush confirms it.
    pending_at: MonoTime,
    /// When recording went down, if it is down now.
    ///
    /// `Some` is the whole of "we are in an outage and should keep trying" —
    /// there is no separate flag, so the state cannot disagree with itself.
    /// Cleared on a successful resume, and never set for the size cap or for
    /// `--no-trace`.
    down_since: Option<MonoTime>,
    /// When to make the next attempt. Only meaningful while [`Self::down_since`]
    /// is set.
    retry_at: MonoTime,
    /// Records that are in the buffer but not yet on disk, and their size.
    ///
    /// Kept because a failure makes them retrospectively *not* written: a
    /// `BufWriter` over a full disk takes bytes without complaint and only fails
    /// when they are pushed, so at the moment of failure everything since the
    /// last successful flush is lost. Without this the recorder would go on
    /// reporting those records in its own `records` and `bytes` — the two
    /// numbers `dozectl status` offers as evidence that recording is working —
    /// and count them as written when they are exactly the ones that were not.
    unflushed: u64,
    unflushed_bytes: u64,
    /// Records missing from this outage: what the buffer was holding when it
    /// failed, plus everything offered while it was down.
    ///
    /// Exact rather than an estimate, which is what [`Self::unflushed`] buys.
    lost: u64,
    /// How many times this run has recovered from an outage. Reported for as
    /// long as the daemon lives, because the holes stay in the file after the
    /// recorder is healthy again.
    gaps: u32,
}

impl TraceFile {
    /// A recorder that writes nothing, for `--no-trace`.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            writer: None,
            path: PathBuf::new(),
            header: None,
            part: 0,
            part_wall: 0,
            seq: 0,
            bytes: 0,
            records: 0,
            conclusions: 0,
            capped: false,
            stopped: None,
            last_at: MonoTime::ZERO,
            pending_at: MonoTime::ZERO,
            unflushed: 0,
            unflushed_bytes: 0,
            down_since: None,
            retry_at: MonoTime::ZERO,
            lost: 0,
            gaps: 0,
        }
    }

    /// Open a new trace in `dir` and write its header.
    ///
    /// A trace we cannot write is not a reason to refuse to run: the daemon's
    /// job is still worth doing, so this degrades to disabled and says so.
    #[must_use]
    pub fn create(dir: &Path, header: &TraceHeader) -> Self {
        if let Err(e) = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir) {
            tracing::warn!(dir = %dir.display(), error = %e, "cannot record traces");
            return Self::disabled();
        }
        prune(dir, KEEP_TRACES);

        let path = dir.join(trace_file_name(header.wall_start));
        // Owner-only, stated here as well as in the daemon's umask. A trace is the
        // highest-resolution record this tool keeps — every window that appeared,
        // every app that was focused, and when — and ten of them are retained.
        let file = match OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot record traces");
                return Self::disabled();
            }
        };
        let mut me = Self {
            writer: Some(BufWriter::new(file)),
            path,
            header: Some(Box::new(header.clone())),
            part: 1,
            part_wall: header.wall_start,
            seq: 0,
            bytes: 0,
            records: 0,
            conclusions: 0,
            capped: false,
            stopped: None,
            last_at: MonoTime::ZERO,
            pending_at: MonoTime::ZERO,
            unflushed: 0,
            unflushed_bytes: 0,
            down_since: None,
            retry_at: MonoTime::ZERO,
            lost: 0,
            gaps: 0,
        };
        me.write(&TraceRecord::Header(Box::new(header.clone())));
        me
    }

    #[must_use]
    pub fn is_recording(&self) -> bool {
        self.writer.is_some()
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    #[must_use]
    pub fn records(&self) -> u64 {
        self.records
    }

    /// Which file of this run is being written, counting from one.
    #[must_use]
    pub fn part(&self) -> u32 {
        self.part
    }

    /// Take the next sequence number. Shared across every record kind, which is
    /// what lets a reader put an event, the derivation it caused and the intent
    /// that followed back into the order they happened.
    pub fn next_seq(&mut self) -> u64 {
        let seq = self.seq;
        self.seq += 1;
        seq
    }

    pub fn write(&mut self, record: &TraceRecord) {
        if self.writer.is_none() || self.capped {
            // Count what we were asked for while down, so the gap record can say
            // how much is missing rather than only how long it lasted. Eleven
            // minutes of an idle desktop and eleven minutes of a login storm are
            // very different holes.
            //
            // Counted through a stall at the cap too. That count is read twice
            // over: by `dozectl status` while the stall lasts — "how much of
            // this run is not in the file", as fair to ask of a stalled trace as
            // of a full disk — and then by whichever marker closes the file, the
            // rotation that recovers or the truncation at [`Drop`]. On the
            // machine that prompted this the cap was hit on day four of a run
            // that went on for another three days, and every number anyone could
            // see said the recording was fine.
            if self.down_since.is_some() || self.capped {
                self.lost += 1;
            }

            return;
        }
        if self.bytes >= MAX_TRACE_BYTES {
            // Reaching the cap is not a fault: the next file is opened and this
            // record goes into it. Only a refusal from the filesystem gets past
            // the `if`, and that is stalled and retried rather than given up on.
            if let Err(reason) = self.rotate(record.at().unwrap_or(self.pending_at)) {
                self.stall(reason);
                return;
            }
        }
        let line = match serde_json::to_string(record) {
            Ok(line) => line,
            Err(e) => {
                tracing::warn!(error = %e, "could not serialise a trace record");
                return;
            }
        };
        let Some(writer) = &mut self.writer else { return };
        if let Err(e) = writeln!(writer, "{line}") {
            let reason = format!("write failed: {e}");
            self.go_down(&reason);
            return;
        }
        self.bytes += line.len() as u64 + 1;
        self.records += 1;
        self.unflushed += 1;
        self.unflushed_bytes += line.len() as u64 + 1;
        if let Some(at) = record.at() {
            self.pending_at = at;
        }
        if !matches!(record, TraceRecord::Event { .. } | TraceRecord::Header(_)) {
            self.conclusions += 1;
        }

        // Events ride the buffer until the next tick; anything that represents a
        // conclusion goes to disk now.
        //
        // The idle tick backs off to 60s (`sampler::adaptive_interval`), so
        // buffering everything would mean a `SIGKILL` losing up to a minute of
        // trace — and the run that died is the one worth reading. Flushing here
        // bounds that: a decision or an intent flushes the events that led to it
        // along with it, so anything load-bearing is durable, while the ticks and
        // samples that changed nothing are the only thing at risk. Conclusions
        // are edge-triggered and rare, so this is a handful of writes a minute,
        // not a fsync per record.
        if !matches!(record, TraceRecord::Event { .. }) {
            self.flush();
        }
    }

    /// Push the buffer to the kernel.
    ///
    /// A failed flush takes recording down, exactly as a failed write does. It
    /// used to warn and carry on, which is the worst of both: seen live, two
    /// `ENOSPC` warnings sixteen minutes apart while the daemon kept writing for
    /// hours afterwards, leaving a trace with a hole in it and no record of
    /// where the hole was. A trace with unmarked gaps is worse than no trace,
    /// because §14 replays it as though it were complete.
    ///
    /// And the reason is kept, not just the fact. `dozectl status` reports trace
    /// size as the evidence that recording is affordable — so after a failure
    /// that number is small for the opposite reason, and the health signal reads
    /// backwards unless something says so.
    pub fn flush(&mut self) {
        let Some(writer) = &mut self.writer else { return };
        if let Err(e) = writer.flush() {
            let reason = format!("flush failed: {e}");
            self.go_down(&reason);
            return;
        }
        // Everything offered so far is now on disk, so the anchor for a future
        // hole can move up to it and nothing is outstanding.
        self.last_at = self.pending_at;
        self.unflushed = 0;
        self.unflushed_bytes = 0;
    }

    /// Stop writing, remember why, and arm the retry.
    ///
    /// The `MonoTime` here is deliberately *not* taken from a clock this module
    /// reads. It comes from the daemon, which already has one, so an outage and
    /// its recovery are testable against a synthetic clock rather than by
    /// waiting a real minute — the same reasoning `dozed_core` applies to the
    /// planner. `retry_at` starts at zero and is set properly by the first
    /// [`Self::retry`] call, which is also the first one that knows what time it
    /// is.
    fn go_down(&mut self, reason: &str) {
        // Dropping the writer flushes it again on the way out, which fails
        // again and is ignored. Whatever it was holding is gone; that is what
        // the gap record's `lost` count exists to admit.
        self.writer = None;
        // What was in the buffer never reached the file, so it was never
        // recorded — and the counters that said otherwise are corrected here
        // rather than left overstating a broken recorder's output.
        self.lost += self.unflushed;
        self.records = self.records.saturating_sub(self.unflushed);
        self.bytes = self.bytes.saturating_sub(self.unflushed_bytes);
        self.unflushed = 0;
        self.unflushed_bytes = 0;
        if self.down_since.is_none() {
            // The hole starts after the last thing that reached disk, not at the
            // tick that noticed — which can be a minute later on an idle desktop.
            self.down_since = Some(self.last_at);
            // Cleared so the *first* attempt of every outage is prompt rather
            // than waiting out an interval armed by the previous one. Without
            // this a recorder that recovered and broke again inherits a deadline
            // up to a minute in the future and sits out a transient failure it
            // could have shrugged off on the next tick.
            self.retry_at = MonoTime::ZERO;
            tracing::warn!(
                path = %self.path.display(),
                // Named and formatted. `bytes` beside a failure read as a size
                // report, and it is the opposite: this is what survived on disk
                // after the records the buffer was still holding were taken back
                // off the count. A raw integer here was also the last
                // unformatted number in a module that formats every other one —
                // the `67108864` defect, in the log instead of the status line.
                written = %human_bytes(self.bytes),
                // How many holes this file already has. Without it each outage
                // is logged as an unrelated incident: three in one day on a real
                // machine read as three separate faults, and the pattern only
                // appeared to somebody who grepped and sorted. The recorder has
                // always known; it just never said.
                prior_gaps = self.gaps,
                reason,
                "trace recording stopped; retrying every {}s",
                RETRY_EVERY.as_secs()
            );
        }
        self.stopped = Some(reason.to_owned());
    }

    /// If recording is down and the interval has passed, try to start again.
    ///
    /// Driven from the daemon's existing tick, so this adds no timer and no
    /// wakeup — §13's constraint is untouched, and on a healthy recorder it is
    /// two comparisons.
    ///
    /// ## Opening the file is not the test
    ///
    /// A full disk lets you `open` all day. It refuses the *write*, and even
    /// then a `BufWriter` will accept bytes into memory and only fail on the
    /// flush. So the probe is a real record — the gap marker itself — followed
    /// by a real flush, and only a flush that returns `Ok` counts as recovery.
    /// Anything less would clear the warning while writing nothing, which is the
    /// same class of lie as the health signal this whole area exists to fix.
    ///
    /// ## Why the newline first
    ///
    /// The failed write may have left a partial line in the file. Appending
    /// straight onto it would fuse the torn record and the gap marker into one
    /// unparseable line, costing both. A bare newline closes the torn line off,
    /// so the damage stays at the one record that was already lost — which is
    /// the property this file's header claims for a torn write at the end.
    pub fn retry(&mut self, now: MonoTime) {
        let Some(down_since) = self.down_since else { return };
        if now < self.retry_at {
            return;
        }
        self.retry_at = now.plus(RETRY_EVERY);

        // Stalled at the cap: what recovery means here is opening the *next*
        // file, not reopening this one. This one is full, and appending to it
        // would push it past the limit it stopped at and never give the run its
        // second part. `rotate` clears the whole down state on success and
        // writes the marker that says what the stall cost.
        if self.capped {
            if let Err(reason) = self.rotate(now) {
                // Keep the newest reason rather than the first: a disk that has
                // gone from `ENOSPC` to `EROFS` is telling you something.
                self.stopped = Some(reason);
            }
            return;
        }

        let Ok(file) = OpenOptions::new().append(true).mode(0o600).open(&self.path) else {
            return;
        };
        let mut writer = BufWriter::new(file);

        let down_ms = now.since(down_since).as_millis() as u64;
        let reason = self.stopped.clone().unwrap_or_else(|| "unknown".into());
        let seq = self.next_seq();
        let marker = TraceRecord::Gap {
            seq,
            at: now,
            kind: GapKind::Hole,
            down_ms,
            lost: self.lost,
            reason,
        };
        let Ok(line) = serde_json::to_string(&marker) else { return };

        // Both halves have to succeed, and the flush is the one that matters.
        if writeln!(writer, "\n{line}").is_err() || writer.flush().is_err() {
            // Still down. The writer is dropped here rather than kept, so a disk
            // that stays full does not leave a file handle and a buffer of dead
            // bytes lying around for every minute of the outage.
            return;
        }

        self.bytes += line.len() as u64 + 2;
        self.records += 1;
        self.gaps += 1;
        let lost = std::mem::take(&mut self.lost);
        self.down_since = None;
        self.stopped = None;
        self.writer = Some(writer);
        // `warn`, not `info`, and logged after the counters so `gaps` is the
        // total including this one.
        //
        // The break was a warning and the recovery was not, so an operator
        // filtering at `warn` — which is the ordinary thing to do, and what was
        // actually done on this machine — saw four failures and no recoveries:
        // a recorder that broke four times and never came back. That is the same
        // inversion as a size that reads as good news, in a different stream.
        //
        // It earns the level on its own merits besides. This line reports lost
        // records and a hole that stays in the file after the recorder is
        // healthy again, and `gaps` only ever goes up.
        tracing::warn!(
            path = %self.path.display(),
            down_for = %HumanDuration(std::time::Duration::from_millis(down_ms)),
            lost,
            gaps = self.gaps,
            "recording resumed; the gap is marked in the trace"
        );
    }

    /// Why recording is stopped, if it is. `None` while healthy.
    ///
    /// Clears on its own when a retry succeeds, which is the point: the message
    /// describes a *current* state, and leaving "no space left on device" on
    /// screen after the space came back is the same kind of stale claim as the
    /// size that goes down when the recorder breaks.
    #[must_use]
    pub fn stopped(&self) -> Option<&str> {
        self.stopped.as_deref()
    }

    /// Stand in for a full disk, from the kernel rather than from a mock.
    ///
    /// `/dev/full` accepts the `open` and refuses every write with `ENOSPC`,
    /// which is the exact shape of the failure this module recovers from. Here
    /// rather than in the test module because `daemon_tests` needs it too and
    /// `writer` is private — and it stays `#[cfg(test)]`, because a recorder
    /// with a public "break yourself" button is a footgun in a daemon that has
    /// one job it must not stop doing quietly.
    #[cfg(test)]
    pub fn fill_the_disk(&mut self) {
        self.writer = Some(BufWriter::new(
            OpenOptions::new().write(true).open("/dev/full").expect("/dev/full"),
        ));
    }

    /// Close this file at the size cap and open the next one.
    ///
    /// Returns the reason it did not work, if it did not. An `Err` leaves the
    /// recorder exactly as it was — still open on the full file, nothing
    /// written — so the caller can stall and come back to it. The reason is
    /// carried out rather than logged and dropped because it is the one thing
    /// that says *which* refusal this was, and it ends up in front of the user.
    ///
    /// ## The new file is opened first
    ///
    /// Before the old one is marked or closed, because that is the step that can
    /// fail, and failing after writing "continues in ..." would leave a trace
    /// pointing at a file that does not exist. A reader that cannot trust a
    /// marker is worse off than one that has none.
    ///
    /// ## Every part carries the run's header
    ///
    /// Including `wall_start`, unchanged: it is the unix time at
    /// `MonoTime::ZERO` and every `at` in every part is an offset from it, so a
    /// part that "corrected" it to its own start would decode every timestamp in
    /// itself wrongly. The *file name* is the rotation's wall clock, which is
    /// what keeps [`Self::list`] chronological.
    ///
    /// ## And pruning happens here
    ///
    /// [`prune`] used to run only when a trace was created, which is why a
    /// recorder that stopped at the cap also stopped enforcing the directory
    /// budget — the two defects were one defect. The current file is the newest
    /// and is therefore exempt, as it is in [`Self::create`]: deleting the
    /// recording being written to make room for nothing is not a saving.
    fn rotate(&mut self, at: MonoTime) -> Result<(), String> {
        let Some(header) = self.header.clone() else {
            return Err("there is no trace header to start the next file with".to_owned());
        };
        let Some(dir) = self.path.parent().map(Path::to_path_buf) else {
            return Err("the trace file has no directory to continue in".to_owned());
        };

        // Chronological, and forced past the current name in the one case the
        // clock does not do it for us: two parts inside the same second.
        let wall = header.wall_of(at).max(self.part_wall + 1);
        let path = dir.join(trace_file_name(wall));
        let file = match OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot continue the trace");
                // Named for what actually refused. This reason is what reaches
                // `dozectl status` and the GUI, and it used to read "hit the
                // 64 MB size cap" — the harmless half of the event, with the
                // half that is a fault left out and the errno saying which fault
                // dropped altogether. A user was told the recorder had reached a
                // limit by design when the disk was full.
                return Err(format!("could not open the next trace file: {e}"));
            }
        };

        // Same seam, two different truths about it: a rotation recovering from a
        // stall lost every record offered while it was stuck, and says so; a
        // clean one lost nothing and must not be counted as a hole.
        let (kind, down_ms, lost) = match self.down_since {
            Some(since) => (GapKind::Hole, at.since(since).as_millis() as u64, self.lost),
            None => (GapKind::Rotated, 0, 0),
        };
        let reason = format!(
            "hit the {} size cap; continues in {}",
            human_bytes(MAX_TRACE_BYTES),
            trace_file_name(wall)
        );
        self.mark_gap(at, kind, reason, down_ms, lost);

        // The marker's own flush pushed whatever the buffer was still holding,
        // so nothing is outstanding against the file being closed — and these
        // must not carry over, or a later failure would have `go_down` subtract
        // the old file's buffered records from the new file's totals.
        self.unflushed = 0;
        self.unflushed_bytes = 0;

        let full = std::mem::replace(&mut self.path, path);
        // Dropping the old writer flushes it; the marker above was flushed
        // already, so there is nothing here that matters and nothing to lose.
        self.writer = Some(BufWriter::new(file));
        self.part += 1;
        self.part_wall = wall;
        // Per-file, both of them: they describe `self.path`, which `dozectl
        // status` prints beside them. `seq` is *not* reset — it is what lets a
        // reader put the whole run's records back into the order they happened,
        // across the boundary as well as within a file.
        self.bytes = 0;
        self.records = 0;
        // Judged per file too, so a part holding nothing but a header is
        // discarded on the same grounds an empty run is. See [`Self::drop`].
        self.conclusions = 0;
        // Cleared before the header goes in, because `write` refuses while the
        // recorder is stalled and the header would be the first thing it
        // refused. Whatever was lost is in the marker above; the counter resets
        // with it, exactly as the retry path resets it.
        let recovered = self.capped.then_some(lost);
        self.capped = false;
        self.down_since = None;
        self.stopped = None;
        self.lost = 0;
        self.write(&TraceRecord::Header(header));

        match recovered {
            Some(lost) => tracing::info!(
                full = %full.display(),
                path = %self.path.display(),
                part = self.part,
                down_for = %HumanDuration(std::time::Duration::from_millis(down_ms)),
                lost,
                "recording resumed in a new file; the gap is marked in the one it left"
            ),
            None => tracing::info!(
                full = %full.display(),
                path = %self.path.display(),
                part = self.part,
                "trace reached its size cap; continuing in a new file"
            ),
        }
        prune(&dir, KEEP_TRACES);
        Ok(())
    }

    /// Stop at the cap because the next file could not be opened, and arm the
    /// retry.
    ///
    /// No marker is written here, and that is the difference between a stall and
    /// a truncation. The file is not finished: the recorder has not given up, it
    /// is waiting on a filesystem that may yet relent, and the marker that closes
    /// this file is written when it is actually closed — by the rotation that
    /// succeeds, or by [`Drop`] if the daemon exits first. Writing one now *and*
    /// another on recovery would leave the file asserting both that it ended
    /// there and that it goes on.
    fn stall(&mut self, reason: String) {
        if self.capped {
            // Already stalled; `retry` owns the schedule from here, and saying
            // so once a record would be the per-record warning this flag exists
            // to prevent.
            self.stopped = Some(reason);
            return;
        }
        self.capped = true;
        // The hole starts after the last thing that reached disk, not at the
        // record that found the door shut. Nothing is outstanding against the
        // full file — every conclusion is flushed as it is written, and the
        // record that tripped the cap was never handed to the buffer.
        self.down_since = Some(self.last_at);
        // Zeroed so the first attempt is prompt rather than waiting out an
        // interval armed by something else. See [`Self::go_down`].
        self.retry_at = MonoTime::ZERO;
        tracing::warn!(
            path = %self.path.display(),
            written = %human_bytes(self.bytes),
            prior_gaps = self.gaps,
            reason = %reason,
            "trace reached its size cap and the next file could not be opened; recording \
             stopped, retrying every {}s",
            RETRY_EVERY.as_secs()
        );
        self.stopped = Some(reason);
    }

    /// Write the record that says this trace stops here, and why.
    ///
    /// Shares [`TraceRecord::Gap`] with the retry path rather than inventing a
    /// second shape, because a reader's question is the same either way — "the
    /// stream is not continuous across this point, and here is the reason" — and
    /// every consumer that already counts gaps then counts this one for free.
    /// The difference between a hole and a truncation is that nothing follows a
    /// truncation, which the file itself says by ending.
    ///
    /// `kind` says which of the three this is, and whether anything is actually
    /// *missing* across the point — only that counts towards [`Self::gaps`]. A
    /// stop loses everything after it; a rotation loses nothing, and counting
    /// one would have `dozectl status` report an incomplete recording for a
    /// recorder that is working perfectly — the same false alarm, in the
    /// opposite direction, as the small file that used to read as good news.
    /// It is a field on the record rather than a shape of its prose, so that
    /// every reader gets the distinction without parsing English.
    ///
    /// Best-effort: a marker that cannot be written changes nothing about the
    /// decision to stop, and there is no one left to tell.
    fn mark_gap(&mut self, at: MonoTime, kind: GapKind, reason: String, down_ms: u64, lost: u64) {
        let seq = self.next_seq();
        let marker = TraceRecord::Gap { seq, at, kind, down_ms, lost, reason };
        let Ok(line) = serde_json::to_string(&marker) else { return };
        let Some(writer) = &mut self.writer else { return };
        if writeln!(writer, "{line}").is_err() || writer.flush().is_err() {
            return;
        }
        self.bytes += line.len() as u64 + 1;
        self.records += 1;
        if kind.loses_records() {
            self.gaps += 1;
        }
        self.last_at = at;
    }

    /// Records the daemon offered while recording was stopped.
    ///
    /// Zero while healthy, and reset by whatever folds it into a marker — the
    /// retry that resumes, or the rotation that gets past a stall. While it is
    /// climbing it is the right number for `dozectl status` to show beside a
    /// stopped trace, because it answers the question the word "STOPPED" does
    /// not: how much of the run is going missing while it stays stopped.
    #[must_use]
    pub fn lost(&self) -> u64 {
        self.lost
    }

    /// Whether the recorder is going to try again on its own.
    ///
    /// Asked rather than inferred from [`Self::stopped`]'s text, which is what
    /// `dozectl status` used to do — it appended "retrying every 60s" to every
    /// stop reason, including the size cap, which is never retried. A status
    /// line promising something that cannot happen is the same failure as one
    /// reporting a broken recorder's small file as good news, and it was printed
    /// on a real machine for a day.
    #[must_use]
    pub fn retrying(&self) -> bool {
        self.down_since.is_some()
    }

    /// How many holes this run's trace has.
    ///
    /// Outlives the outage that caused it, deliberately. Once recording resumes
    /// there is nothing wrong with the recorder and [`Self::stopped`] is `None`
    /// — but the *file* still has a stretch missing, and everything that reads it
    /// afterwards is reading an incomplete recording. §14's replay compares an
    /// intent stream against what was recorded; `dozectl suggest` projects
    /// savings from what the samples show. Neither is entitled to be told the
    /// recording is whole when it is not.
    #[must_use]
    pub fn gaps(&self) -> u32 {
        self.gaps
    }

    /// Whether a trace file was opened for this run at all, healthy or not.
    ///
    /// Distinct from [`TraceFile::is_recording`], which answers whether it is
    /// still being written. A run that started recording and stopped has
    /// something to report; `--no-trace` has nothing.
    #[must_use]
    pub fn was_started(&self) -> bool {
        !self.path.as_os_str().is_empty()
    }

    /// Traces in `dir`, newest first.
    #[must_use]
    pub fn list(dir: &Path) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(is_trace_name))
            .collect();
        // Names are a wall-clock time in unix seconds, zero-padded — the run's
        // start, or the moment a rotation opened the file — so lexicographic
        // order is chronological order.
        files.sort();
        files.reverse();
        files
    }
}

impl Drop for TraceFile {
    /// Flush, and then throw this trace away if it never concluded anything.
    ///
    /// Only [`KEEP_TRACES`] runs are retained, and the ring was being spent on
    /// runs that had nothing in them. Seen live: three restarts inside two
    /// seconds wrote three 3.8 KB traces containing a header and the windows
    /// that happened to be open, and pushed three real sessions — one of them
    /// 398 KB — out of the ring. Eleven kilobytes of startup banner displaced
    /// most of a day's evidence.
    ///
    /// Judged on conclusions rather than on size or record count, because that
    /// is what a trace is *for*: `dozectl replay`, `dozectl suggest` and every
    /// comparison between two runs read derivations and intents. A session with
    /// none of them replays to an empty intent stream, which is not a finding
    /// about the session, it is the absence of one.
    ///
    /// A genuinely quiet run is deleted too, and that is the intended answer:
    /// there is nothing in it to read. Nothing else is lost — the journal keeps
    /// what was decided, and this file recorded no decisions.
    fn drop(&mut self) {
        // A run that ends while stalled at the cap has never said so in the
        // file, because a stall deliberately writes no marker — it is waiting,
        // not finished. This is where it becomes finished, so this is where it
        // is marked, and the module's own standard is kept: "a trace with
        // unmarked gaps is worse than no trace, because §14 replays it as though
        // it were complete".
        //
        // `down_ms` is zero because a truncation has no end inside the file to
        // measure to; `lost` is what the stall cost up to here, which is the
        // number that was climbing in `dozectl status`.
        if self.capped && self.was_started() {
            let reason =
                self.stopped.clone().unwrap_or_else(|| "recording stopped at the size cap".into());
            let (at, lost) = (self.pending_at, self.lost);
            self.mark_gap(at, GapKind::Truncated, reason, 0, lost);
        }
        self.flush();
        if !self.was_started() || self.conclusions > 0 {
            return;
        }
        // Close before unlinking, so the space is actually returned rather than
        // held by our own open descriptor — which matters most in the case that
        // produces these, a machine whose disk is full.
        self.writer = None;
        match std::fs::remove_file(&self.path) {
            Ok(()) => tracing::debug!(
                path = %self.path.display(),
                "discarded a trace with nothing in it; the ring is for sessions that decided something"
            ),
            Err(e) => {
                tracing::debug!(path = %self.path.display(), error = %e, "could not discard an empty trace");
            }
        }
    }
}

/// Keep the `keep` newest traces and delete the rest.
/// Drop old traces, by count **and** by total size.
///
/// The count alone was the whole bound, on the assumption that a run costs a few
/// megabytes. It does not: measured on a real machine, 67 MB in 3.6 days, so ten
/// retained runs is most of a gigabyte of somebody's disk held by a diagnostic
/// they did not ask for. `MAX_TRACE_BYTES` caps one file and said nothing about
/// the directory.
///
/// Newest first, so what survives is what somebody would actually want to read.
/// A single run larger than the whole budget still keeps its own file — deleting
/// the recording currently being written to make room for nothing is not a
/// saving — which is why the check runs before the current trace is created.
fn prune(dir: &Path, keep: usize) {
    let mut total = 0u64;
    for (n, old) in TraceFile::list(dir).into_iter().enumerate() {
        let size = std::fs::metadata(&old).map(|m| m.len()).unwrap_or(0);
        let over_budget = n > 0 && total + size > MAX_TRACES_BYTES;
        if n < keep && !over_budget {
            total += size;
            continue;
        }
        let why =
            if over_budget { "over the directory budget" } else { "beyond the retained runs" };
        match std::fs::remove_file(&old) {
            // `info`, not `debug`. This deletes a recording the user may be
            // about to read, and at default levels it did so in silence — on a
            // filesystem at 96% full, the one piece of housekeeping anybody
            // would want to see. The size is here because "over the directory
            // budget" is a claim about bytes and was made without any.
            Ok(()) => tracing::info!(
                path = %old.display(),
                freed = %human_bytes(size),
                why,
                "pruned an old trace"
            ),
            Err(e) => tracing::warn!(path = %old.display(), error = %e, "could not prune"),
        }
    }
}

#[cfg(test)]
mod tests {
    use dozed_core::trace::{TRACE_VERSION, parse_lines};
    use dozed_core::{Config, Event, MonoTime};

    use super::*;

    /// What a reader does: slurp the file, hand the lines to the core parser.
    fn read(path: &Path) -> (Vec<TraceRecord>, usize) {
        let text = std::fs::read_to_string(path).unwrap();
        parse_lines(text.lines())
    }

    /// A directory of this test's own.
    ///
    /// Unique per *call*, not per tag. Two tests both saying `tmpdir("torn")`
    /// get the same path, and since this starts by removing it, cargo running
    /// them in parallel means each one deletes the other's trace mid-write. That
    /// arrives as a pair of unrelated-looking failures in whichever two tests
    /// happened to collide — `dozed_linux::thermal`'s fixture carries the same
    /// counter for the same reason, having been bitten first.
    fn tmpdir(tag: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("dozed-trace-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A conclusion, which is what makes a trace worth keeping — see
    /// [`TraceFile::drop`]. Tests that only write events would otherwise be
    /// writing a trace the recorder is right to throw away.
    fn an_intent(seq: u64) -> TraceRecord {
        at_intent(seq, MonoTime::ZERO)
    }

    /// The same, at a chosen time — for the outage tests, where *when* a record
    /// landed is what the gap's length gets measured against.
    fn at_intent(seq: u64, at: MonoTime) -> TraceRecord {
        TraceRecord::Intent {
            seq,
            at,
            intent: Box::new(dozed_core::Intent::new(
                dozed_core::AppKey::comm("slack"),
                dozed_core::Action::Freeze { reclaim: false },
                dozed_core::Reason::new(dozed_core::ReasonKind::RuleFired, "test"),
                at,
            )),
            outcome: dozed_core::Outcome::WouldHave,
        }
    }

    fn header(wall_start: u64) -> TraceHeader {
        TraceHeader {
            version: TRACE_VERSION,
            dozed_version: "test".into(),
            instance: "test-1".into(),
            wall_start,
            live: false,
            config: Box::new(Config::empty()),
        }
    }

    #[test]
    fn conclusions_reach_disk_without_waiting_for_a_flush() {
        // The idle tick backs off to 60s, so a buffered decision could be lost
        // to a SIGKILL a minute later — in the run that most needs reading.
        let dir = tmpdir("durable");
        let mut t = TraceFile::create(&dir, &header(1_700_000_100));
        assert!(!read(t.path()).0.is_empty(), "the header alone must not leave an empty file");

        let seq = t.next_seq();
        t.write(&TraceRecord::Event { seq, at: MonoTime::ZERO, event: Box::new(Event::Tick) });
        let seq = t.next_seq();
        t.write(&TraceRecord::Intent {
            seq,
            at: MonoTime::ZERO,
            intent: Box::new(dozed_core::Intent::new(
                dozed_core::AppKey::comm("slack"),
                dozed_core::Action::Freeze { reclaim: false },
                dozed_core::Reason::new(dozed_core::ReasonKind::RuleFired, "test"),
                MonoTime::ZERO,
            )),
            outcome: dozed_core::Outcome::WouldHave,
        });

        // Not dropped, so nothing has flushed on our behalf.
        let (back, _) = read(t.path());
        assert_eq!(back.len(), 3, "the intent flushed the event that preceded it too");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_that_concluded_nothing_does_not_claim_a_slot_in_the_ring() {
        // Three restarts inside two seconds each wrote a 3.8 KB trace of a
        // header and the windows that happened to be open, and pushed three real
        // sessions — one of them 398 KB — out of a ten-deep ring.
        let dir = tmpdir("trivial");

        let mut empty = TraceFile::create(&dir, &header(1_700_000_200));
        let seq = empty.next_seq();
        // Observations, and nothing concluded from them: exactly the shape of a
        // daemon that started and was stopped again.
        empty.write(&TraceRecord::Event { seq, at: MonoTime::ZERO, event: Box::new(Event::Tick) });
        let empty_path = empty.path().to_path_buf();
        drop(empty);
        assert!(!empty_path.exists(), "a trace with no conclusions in it is not worth a slot");

        let mut real = TraceFile::create(&dir, &header(1_700_000_201));
        let seq = real.next_seq();
        real.write(&an_intent(seq));
        let real_path = real.path().to_path_buf();
        drop(real);
        assert!(real_path.exists(), "and one that decided something is kept");

        assert_eq!(
            TraceFile::list(&dir),
            vec![real_path],
            "so only the real session is in the ring"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_recorder_that_broke_says_so_rather_than_reporting_a_small_file() {
        // Seen live: two ENOSPC warnings sixteen minutes apart while the daemon
        // kept writing for hours after, leaving a trace with an unmarked hole in
        // it — and `dozectl status` still reporting its size as evidence that
        // recording is cheap.
        let dir = tmpdir("broken");
        let mut t = TraceFile::create(&dir, &header(1_700_000_300));
        let seq = t.next_seq();
        t.write(&an_intent(seq));
        assert!(t.is_recording() && t.stopped().is_none());

        // Standing in for a full disk: the descriptor beneath the buffer is
        // taken away, so the next flush cannot succeed.
        let path = t.path().to_path_buf();
        t.writer = Some(BufWriter::new(File::open("/dev/full").expect("/dev/full")));
        let seq = t.next_seq();
        t.write(&an_intent(seq));

        assert!(!t.is_recording(), "a failed write or flush stops recording");
        assert!(t.stopped().is_some(), "and keeps the reason: {:?}", t.stopped());
        assert!(t.was_started(), "this is a broken recorder, not an absent one");

        // The records written before the failure are still on disk and still
        // readable — the point is that nothing is added silently afterwards.
        assert_eq!(read(&path).0.len(), 2, "header plus the intent that landed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_outage_ends_when_the_disk_does() {
        // The whole point of the retry. A disk that fills at nine in the morning
        // used to cost the rest of the day's recording, because the recorder
        // stopped for the run — so emptying a download folder at lunchtime
        // changed nothing until the next restart.
        let dir = tmpdir("resume");
        let mut t = TraceFile::create(&dir, &header(1_700_000_400));
        let path = t.path().to_path_buf();

        let seq = t.next_seq();
        t.write(&at_intent(seq, MonoTime::from_millis(1_000)));
        assert!(t.is_recording());

        t.fill_the_disk();
        let seq = t.next_seq();
        t.write(&at_intent(seq, MonoTime::from_millis(2_000)));
        assert!(!t.is_recording(), "a write that cannot land takes recording down");
        assert!(t.stopped().is_some());

        // Records offered while down are counted, so the marker can say how much
        // is missing rather than only how long it lasted.
        for i in 0..4 {
            let seq = t.next_seq();
            t.write(&at_intent(seq, MonoTime::from_millis(3_000 + i * 100)));
        }

        // Space appears. The path is a real file again, so the next attempt
        // works — but only once the interval has passed.
        t.retry(MonoTime::from_millis(2_500));
        assert!(t.is_recording(), "the first attempt is prompt rather than a minute late");
        assert!(t.stopped().is_none(), "the message describes now, and now it is fine");
        assert_eq!(t.gaps(), 1, "but the hole is still in the file, and is still reported");

        // And it keeps recording afterwards.
        let seq = t.next_seq();
        t.write(&at_intent(seq, MonoTime::from_millis(4_000)));
        t.flush();
        assert!(t.is_recording());

        let (records, _) = read(&path);
        let gap = records
            .iter()
            .find(|r| matches!(r, TraceRecord::Gap { .. }))
            .expect("the gap must be in the stream, not only in the status line");
        let TraceRecord::Gap { down_ms, lost, reason, .. } = gap else { unreachable!() };
        // Measured from the last record that reached disk, not from the tick
        // that noticed: 1000ms was the last good write, 2500ms is now.
        assert_eq!(*down_ms, 1_500);
        assert_eq!(*lost, 5, "the one lost from the buffer plus the four offered while down");
        assert!(reason.contains("No space left"), "{reason}");

        // The records either side of the hole are all readable, which is what
        // makes a resumed trace worth having at all.
        assert!(records.iter().any(|r| matches!(r, TraceRecord::Header(_))));
        assert_eq!(
            records.iter().filter(|r| matches!(r, TraceRecord::Intent { .. })).count(),
            2,
            "the one before the outage and the one after"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_disk_that_is_still_full_is_tried_again_and_again_forever() {
        // "Ad infinitum" is the requirement, and it is the right one: there is no
        // number of failures that makes the next attempt less worth making. A
        // recorder that gave up after ten tries would be the original bug with
        // extra steps and a longer fuse.
        let dir = tmpdir("still-full");
        let mut t = TraceFile::create(&dir, &header(1_700_000_500));
        let seq = t.next_seq();
        t.write(&at_intent(seq, MonoTime::from_millis(1_000)));

        // Point the recorder at a path that can never be written, so every
        // attempt fails the way a still-full disk does.
        t.fill_the_disk();
        let seq = t.next_seq();
        t.write(&at_intent(seq, MonoTime::from_millis(2_000)));
        t.path = PathBuf::from("/dev/full");
        assert!(!t.is_recording());

        // A hundred attempts over a hundred minutes, none of which succeeds and
        // none of which gives up.
        for minute in 1..=100u64 {
            t.retry(MonoTime::from_millis(2_000 + minute * 60_000));
            assert!(!t.is_recording(), "attempt {minute} should not have succeeded");
            assert!(t.stopped().is_some(), "attempt {minute} must keep saying why");
        }
        assert_eq!(t.gaps(), 0, "nothing recovered, so nothing was resumed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn attempts_wait_out_the_interval_rather_than_riding_every_tick() {
        // The tick drops to two seconds when a grace deadline is near, and a
        // retry is an `open` and a write. Cheap is not free, and hammering a
        // full disk thirty times a minute is the daemon becoming the problem it
        // exists to remove (§13).
        let dir = tmpdir("interval");
        let mut t = TraceFile::create(&dir, &header(1_700_000_600));
        let seq = t.next_seq();
        t.write(&at_intent(seq, MonoTime::from_millis(1_000)));

        t.fill_the_disk();
        let seq = t.next_seq();
        t.write(&at_intent(seq, MonoTime::from_millis(2_000)));

        // Point at an unwritable path so the first attempt fails, which is what
        // arms the interval.
        let good = t.path().to_path_buf();
        t.path = PathBuf::from("/dev/full");
        t.retry(MonoTime::from_millis(2_000));
        assert!(!t.is_recording());

        // Now the disk is fine again — but the interval has not passed, so
        // nothing should happen yet.
        t.path = good;
        t.retry(MonoTime::from_millis(30_000));
        assert!(!t.is_recording(), "an attempt inside the interval must not be made");

        // Once it has, it resumes.
        t.retry(MonoTime::from_millis(63_000));
        assert!(t.is_recording());
        assert_eq!(t.gaps(), 1);

        // A second outage gets a prompt first attempt of its own, rather than
        // inheriting the deadline the last recovery armed. Otherwise a recorder
        // that broke again a second later would sit out a transient failure it
        // could have shrugged off on the next tick.
        t.fill_the_disk();
        let seq = t.next_seq();
        t.write(&at_intent(seq, MonoTime::from_millis(64_000)));
        assert!(!t.is_recording());
        t.retry(MonoTime::from_millis(65_000));
        assert!(t.is_recording(), "the first attempt of a new outage is not made to wait");
        assert_eq!(t.gaps(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The cap bounds the file. It used to end the run, and that was the bug.
    ///
    /// Recording stopped on day four of a seven-day run, and because [`prune`]
    /// only runs when a trace is created, nothing pruned for the remaining three
    /// days either — the directory sat over its own budget with no recorder
    /// working. Rotating is one change that answers both.
    #[test]
    fn the_size_cap_rolls_over_to_a_new_file_rather_than_ending_the_run() {
        let dir = tmpdir("rotates");
        let mut t = TraceFile::create(&dir, &header(1_700_000_700));
        let first = t.path().to_path_buf();
        assert_eq!(t.part(), 1);

        t.bytes = MAX_TRACE_BYTES;
        let seq = t.next_seq();
        t.write(&at_intent(seq, MonoTime::from_millis(9_000)));

        assert!(!t.capped, "the cap is not a stop any more");
        assert_eq!(t.stopped(), None, "and nothing is broken");
        assert!(t.is_recording(), "recording continues");
        assert_eq!(t.part(), 2);
        assert_ne!(t.path(), first, "in a new file");
        assert_eq!(t.lost(), 0, "and nothing was dropped on the way");

        // Nothing is *missing* across the boundary, so a rotation must not
        // report the run's recording as incomplete.
        assert_eq!(t.gaps(), 0, "a rotation is not a hole");

        // The record that tripped the cap is in the new file, behind the header
        // that makes the new file replayable on its own.
        t.flush();
        let (records, dropped) = read(t.path());
        assert_eq!(dropped, 0);
        assert!(matches!(records.first(), Some(TraceRecord::Header(_))), "{records:?}");
        assert!(
            records.iter().any(|r| matches!(r, TraceRecord::Intent { .. })),
            "the record that tripped the cap is kept, not dropped: {records:?}"
        );

        // And the file it left behind says why it ends, naming its successor.
        let (old, _) = read(&first);
        let TraceRecord::Gap { reason, .. } =
            old.iter().find(|r| matches!(r, TraceRecord::Gap { .. })).expect("the marker")
        else {
            unreachable!()
        };
        assert!(reason.contains("size cap"), "{reason}");
        assert!(
            reason.contains(&t.path().file_name().unwrap().to_string_lossy().to_string()),
            "a reader of the full file is told where the session goes on: {reason}"
        );

        // Chronological, which is what `list` sorts on and what every reader of
        // a trace directory relies on to find the newest.
        let listed = TraceFile::list(&dir);
        assert_eq!(listed.first(), Some(&t.path().to_path_buf()), "{listed:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two rotations inside one second still produce two files.
    ///
    /// Names are the wall clock and the clock has one-second resolution, so
    /// without the forced increment the second rotation would reopen the first
    /// part with `truncate(true)` and delete a full file of evidence. Only
    /// reachable in a test at these sizes, and that is not a reason to let the
    /// name be wrong.
    #[test]
    fn a_rotation_never_reuses_the_name_it_is_continuing() {
        let dir = tmpdir("rotate-twice");
        let mut t = TraceFile::create(&dir, &header(1_700_000_750));
        let mut seen = vec![t.path().to_path_buf()];
        for _ in 0..2 {
            t.bytes = MAX_TRACE_BYTES;
            let seq = t.next_seq();
            t.write(&at_intent(seq, MonoTime::from_millis(10)));
            seen.push(t.path().to_path_buf());
        }
        t.flush();
        assert_eq!(t.part(), 3);
        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 3, "three parts, three files: {seen:?}");
        assert_eq!(TraceFile::list(&dir).len(), 3, "all three are on disk");
        // Sorted order is part order, so the newest is still the newest.
        assert_eq!(TraceFile::list(&dir), {
            let mut r = seen.clone();
            r.reverse();
            r
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cap that cannot open its next file is the one case left that stops —
    /// and it is retried, because it is the world refusing.
    ///
    /// This used to be the one stop that was never retried, on the reasoning
    /// that the cap is a limit dozed sets on itself. That reasoning belonged to
    /// the cap, and the cap now rotates: the only way to arrive here is an
    /// `open` the filesystem turned down, which is the same fault a failed
    /// write reports and was already being retried. The same disk filling and
    /// emptying revived one recorder and not the other, decided by nothing but
    /// which line of this file noticed first.
    #[test]
    fn a_rotation_that_cannot_open_its_next_file_is_retried_until_it_can() {
        let dir = tmpdir("capped");
        let mut t = TraceFile::create(&dir, &header(1_700_000_700));
        let real = t.path().to_path_buf();
        // The writer stays open on a real file; what fails is creating the next
        // part, which is the only way the cap can still stop a run.
        t.path = Path::new("/proc/definitely/not/writable/1700000700.jsonl").to_path_buf();
        t.bytes = MAX_TRACE_BYTES;
        let seq = t.next_seq();
        t.write(&an_intent(seq));

        assert!(t.capped);
        let why = t.stopped().expect("it says it stopped").to_owned();
        // Named for what refused, not for the limit it happened to be passing.
        // "hit the 64 MB size cap" described the harmless half of this and hid
        // the half that is a fault.
        assert!(why.contains("could not open the next trace file"), "{why}");
        assert_eq!(t.part(), 1, "nothing was opened");
        assert!(t.retrying(), "the filesystem refused, and the filesystem changes");

        // Still refused: it goes on trying, and goes on saying so.
        for minute in 1..=5u64 {
            t.retry(MonoTime::from_millis(minute * 60_000));
        }
        assert!(t.stopped().is_some(), "still nowhere to continue");
        assert_eq!(t.part(), 1);

        // What the daemon offers through the stall is counted, and is what the
        // marker reports once there is a file to write one in.
        for _ in 0..7 {
            let seq = t.next_seq();
            t.write(&an_intent(seq));
        }
        assert_eq!(t.lost(), 7);

        // The disk comes back. This is the recovery that used to be unreachable.
        t.path = real.clone();
        t.retry(MonoTime::from_millis(6 * 60_000));

        assert_eq!(t.stopped(), None, "recording resumed");
        assert!(!t.capped);
        assert!(!t.retrying());
        assert_eq!(t.part(), 2, "in the next file");
        assert_eq!(t.lost(), 0, "folded into the marker and reset");

        // And the file it left behind says what the stall cost — as a hole,
        // because records really are missing across it, unlike a clean roll.
        let (old, _) = read(&real);
        let gap = old.iter().find(|r| matches!(r, TraceRecord::Gap { .. })).expect("the marker");
        let TraceRecord::Gap { kind, lost, down_ms, .. } = gap else { unreachable!() };
        assert_eq!(*kind, GapKind::Hole, "records went missing, so it is a hole");
        assert_eq!(*lost, 7);
        assert!(*down_ms > 0, "and it lasted a measurable time");
        assert_eq!(t.gaps(), 1, "which makes this an incomplete recording");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The cap was the one way of stopping that left no mark in the stream.
    ///
    /// This module's own standard, from `RETRY_EVERY`: "a trace with unmarked
    /// gaps is worse than no trace, because §14 replays it as though it were
    /// complete". Every other stop obeyed it. The size cap simply ended the
    /// file mid-session, and a replay would have read that as a session that
    /// ended there — on the machine that prompted this, three days early.
    ///
    /// The cap itself now rotates, so what is pinned here is the remaining stop:
    /// a rotation that could not open its next file, in a run that then ended
    /// before it could. A stall writes nothing — the recorder is waiting, not
    /// finished — so the marker belongs at the point the run actually ends.
    #[test]
    fn a_run_that_ends_while_stalled_says_so_at_its_last_line() {
        let dir = tmpdir("capped-marked");
        let mut t = TraceFile::create(&dir, &header(1_700_000_900));
        let path = t.path().to_path_buf();
        // Something worth keeping, or `drop` discards the run as an empty one
        // and there is no file left to make this assertion about.
        let seq = t.next_seq();
        t.write(&an_intent(seq));
        // Open on a real file, with nowhere to continue to. See
        // `a_rotation_that_cannot_open_its_next_file_is_retried_until_it_can`.
        t.path = Path::new("/proc/definitely/not/writable/1700000900.jsonl").to_path_buf();
        t.bytes = MAX_TRACE_BYTES;
        let seq = t.next_seq();
        t.write(&an_intent(seq));

        // Nothing is written at the stall itself. A file claiming it ended here
        // would be contradicted by the rotation that may yet follow, and two
        // markers disagreeing is worse than the one that arrives late.
        let (during, _) = read(&path);
        assert!(
            !during.iter().any(|r| matches!(r, TraceRecord::Gap { .. })),
            "a stall is not a truncation: {during:?}"
        );

        // What the daemon goes on offering is counted, so `dozectl status` can
        // say how much of the run is missing rather than only that it stopped.
        for _ in 0..7 {
            let seq = t.next_seq();
            t.write(&an_intent(seq));
        }
        assert_eq!(t.lost(), 7, "records offered past the cap are counted");

        // The run ends here, so here is where the file is finished — with the
        // reason, rather than left to read back as a session that stopped.
        drop(t);

        let text = std::fs::read_to_string(&path).unwrap();
        let (records, dropped) = parse_lines(text.lines());
        assert_eq!(dropped, 0, "the marker must be a whole line: {text}");
        let gap = records
            .iter()
            .find(|r| matches!(r, TraceRecord::Gap { .. }))
            .expect("the truncation is in the file, not only in the log");
        let TraceRecord::Gap { kind, reason, lost, .. } = gap else { unreachable!() };
        assert_eq!(*kind, GapKind::Truncated, "nothing follows it in this file");
        assert!(
            reason.contains("could not open the next trace file"),
            "and it says what refused: {reason}"
        );
        assert_eq!(*lost, 7, "and what that cost");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_torn_line_costs_one_record_rather_than_two() {
        // The failed write can leave a partial line behind. Appending the gap
        // marker straight onto it would fuse the two into one unparseable line,
        // costing the marker as well as the record that was already lost — and
        // the marker is the thing that makes the whole resume defensible.
        let dir = tmpdir("torn");
        let mut t = TraceFile::create(&dir, &header(1_700_000_800));
        let path = t.path().to_path_buf();
        let seq = t.next_seq();
        t.write(&at_intent(seq, MonoTime::from_millis(1_000)));
        t.flush();

        // A half-written record, exactly as a write interrupted by ENOSPC leaves.
        {
            use std::io::Write as _;
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            write!(f, "{{\"rec\":\"intent\",\"seq\":9,\"at\"").unwrap();
        }
        t.writer = None;
        t.stopped = Some("write failed: simulated".into());
        t.down_since = Some(MonoTime::from_millis(1_000));

        t.retry(MonoTime::from_millis(61_000));
        assert!(t.is_recording(), "the retry should have taken");

        let (records, skipped) = read(&path);
        assert_eq!(skipped, 1, "the torn line, and only the torn line");
        assert!(
            records.iter().any(|r| matches!(r, TraceRecord::Gap { .. })),
            "the marker survived being written after a torn line"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_directory_is_bounded_by_bytes_as_well_as_by_count() {
        // Ten runs at the per-file cap is 640 MB, and a real machine reached that
        // cap in 3.6 days — so the count bound alone let a diagnostic nobody
        // asked for hold most of a gigabyte.
        let dir = tmpdir("budget");
        std::fs::create_dir_all(&dir).unwrap();
        // Four traces, newest last by name, each a quarter of the budget plus a
        // little — so the two oldest cannot fit.
        let chunk = vec![b'x'; (MAX_TRACES_BYTES / 4) as usize + 1024];
        for wall in [1_700_000_001u64, 1_700_000_002, 1_700_000_003, 1_700_000_004] {
            std::fs::write(dir.join(trace_file_name(wall)), &chunk).unwrap();
        }
        assert_eq!(TraceFile::list(&dir).len(), 4);

        prune(&dir, KEEP_TRACES);

        let left = TraceFile::list(&dir);
        let total: u64 =
            left.iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
        assert!(total <= MAX_TRACES_BYTES, "{} bytes left", total);
        assert!(!left.is_empty(), "the newest is always kept");
        // Newest first: what survives is what somebody would want to read.
        assert!(
            left[0].to_string_lossy().contains("1700000004"),
            "the newest must be the survivor: {left:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_oversized_trace_still_keeps_its_own_file() {
        // Deleting the only recording to make room for nothing is not a saving.
        let dir = tmpdir("oversize");
        std::fs::create_dir_all(&dir).unwrap();
        let huge = vec![b'x'; (MAX_TRACES_BYTES + 4096) as usize];
        std::fs::write(dir.join(trace_file_name(1_700_000_009)), &huge).unwrap();

        prune(&dir, KEEP_TRACES);
        assert_eq!(TraceFile::list(&dir).len(), 1, "the newest survives whatever its size");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_trace_roundtrips_through_a_file() {
        let dir = tmpdir("rt");
        let mut t = TraceFile::create(&dir, &header(1_700_000_000));
        assert!(t.is_recording());
        let seq = t.next_seq();
        t.write(&TraceRecord::Event {
            seq,
            at: MonoTime::from_millis(500),
            event: Box::new(Event::Tick),
        });
        let seq = t.next_seq();
        t.write(&an_intent(seq));
        let path = t.path().to_path_buf();
        drop(t); // flushes

        let (back, skipped) = read(&path);
        assert_eq!(skipped, 0);
        assert_eq!(back.len(), 3, "header, one event, one intent");
        assert!(matches!(back[0], TraceRecord::Header(_)));
        assert_eq!(back[0].seq(), None, "the header is outside the sequence");
        assert_eq!(back[1].seq(), Some(0), "so the first record is zero");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_torn_final_line_costs_one_record_not_the_session() {
        // A SIGKILL mid-write is the normal way a daemon dies, and the session
        // it died in is the one most worth reading.
        let dir = tmpdir("torn");
        let mut t = TraceFile::create(&dir, &header(1_700_000_001));
        let seq = t.next_seq();
        t.write(&TraceRecord::Event { seq, at: MonoTime::ZERO, event: Box::new(Event::Tick) });
        let seq = t.next_seq();
        t.write(&an_intent(seq));
        let path = t.path().to_path_buf();
        drop(t);

        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"rec\":\"event\",\"seq\":2,\"at\"");
        std::fs::write(&path, text).unwrap();

        let (back, skipped) = read(&path);
        assert_eq!(back.len(), 3, "the intact records survive");
        assert_eq!(skipped, 1, "and the torn one is counted, not hidden");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_traces_are_pruned_but_recent_ones_are_kept() {
        let dir = tmpdir("prune");
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..15u64 {
            std::fs::write(dir.join(format!("{:010}.jsonl", 1_700_000_000 + i)), "{}\n").unwrap();
        }
        prune(&dir, 10);
        let left = TraceFile::list(&dir);
        assert_eq!(left.len(), 10);
        assert!(
            left[0].to_string_lossy().contains("1700000014"),
            "the newest must survive: {left:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unwritable_directory_does_not_stop_the_daemon() {
        let t = TraceFile::create(Path::new("/proc/definitely/not/writable"), &header(0));
        assert!(!t.is_recording(), "degrades to not recording");
        assert_eq!(t.bytes(), 0);
    }

    #[test]
    fn a_disabled_recorder_writes_nothing() {
        let mut t = TraceFile::disabled();
        let seq = t.next_seq();
        t.write(&TraceRecord::Event { seq, at: MonoTime::ZERO, event: Box::new(Event::Tick) });
        t.flush();
        assert_eq!(t.records(), 0);
        assert!(!t.is_recording());
    }
}
