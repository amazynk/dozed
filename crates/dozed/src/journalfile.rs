//! Appending the journal to disk as JSON lines.
//!
//! One object per line so it can be tailed, grepped and replayed without a
//! parser, and so a torn write at the end costs one entry rather than the file.
//!
//! ## Why this file is bounded and the trace already was
//!
//! A trace is one file per run and ten runs are kept ([`crate::tracefile`]), so
//! it could never grow without limit. The journal is the opposite shape — one
//! file, appended to for as long as the tool has been installed — and nothing
//! here bounded it. Measured on a real machine: 1.9 MB and 3,947 entries after
//! thirteen days, read into memory *in full* on each of 135 restarts, which is
//! the likeliest reason that daemon's peak RSS ranged from 6.0 MB to 58.7 MB
//! across runs. Extrapolated to a year it is a 55 MB file that a battery daemon
//! parses at every login.
//!
//! Two independent bounds, because they answer different questions:
//!
//! * [`MAX_JOURNAL_BYTES`] caps what is on disk. One previous generation is
//!   kept beside the live file, so rotating costs history rather than deleting
//!   it.
//! * [`MAX_BACKLOG`] caps what is held in memory. `dozectl journal` shows the
//!   recent past; nobody scrolls to last March, and the daemon should not be
//!   paying to hold it.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use dozed_core::JournalEntry;

/// Roll the journal over at this size.
///
/// A day of ordinary desktop use is about 150 KB, so this is roughly two months
/// before a rotation and four before anything is actually lost — long enough
/// that `dozectl report --week` and any comparison against "last month" are
/// answered from the live file alone.
const MAX_JOURNAL_BYTES: u64 = 8 * 1024 * 1024;

/// How many entries a fresh daemon loads back into memory.
///
/// Roughly a month of real use at the rate measured above. The number that
/// matters is not this one but that there *is* one: the previous behaviour had
/// the daemon's resident memory growing monotonically with how long ago the
/// tool was installed, which is the wrong quantity for it to depend on.
const MAX_BACKLOG: usize = 5_000;

pub struct JournalFile {
    file: Option<File>,
    path: PathBuf,
    /// Size on disk, tracked rather than stat'd: this is checked on every
    /// append and a syscall per decision to learn something we just changed
    /// ourselves is exactly the cost §13 objects to.
    bytes: u64,
}

/// The one previous generation, kept beside the live file.
fn previous_of(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    path.with_file_name(name)
}

impl JournalFile {
    /// Open for appending, creating the state directory if needed.
    ///
    /// A journal we cannot write is not a reason to refuse to run: the daemon's
    /// job is still worth doing, so this degrades to in-memory only and says so.
    pub fn open(path: PathBuf) -> Self {
        if let Some(parent) = path.parent() {
            let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent);
        }
        // Owner-only, stated here as well as in the daemon's umask. This file is a
        // line per decision for as long as the tool has been installed — which
        // applications this user runs, and when they were last touched — and it
        // outlives every process that could have been protecting it.
        let file = OpenOptions::new().create(true).append(true).mode(0o600).open(&path);
        match file {
            Ok(f) => {
                let bytes = f.metadata().map(|m| m.len()).unwrap_or_default();
                let mut me = Self { file: Some(f), path, bytes };
                // Checked at open as well as on append, because the common case
                // is a daemon restarting into a file that grew past the cap
                // during the previous run — and a rotation is a rename, so
                // doing it before the first append keeps every write in the
                // same generation as the header of its run.
                me.rotate_if_full();
                me
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "journal unavailable; continuing in memory");
                Self { file: None, path, bytes: 0 }
            }
        }
    }

    /// Move the journal aside and start a new one, keeping one generation.
    ///
    /// Deleting outright was the alternative and is worse: the entries most
    /// worth having after a rotation are the ones immediately before it, and a
    /// user asking "what happened last month" would get silence rather than an
    /// answer. [`JournalFile::read_all`] reads both, so a rotation is invisible
    /// until the *second* one.
    fn rotate_if_full(&mut self) {
        if self.bytes < MAX_JOURNAL_BYTES {
            return;
        }
        let previous = previous_of(&self.path);
        if let Err(e) = std::fs::rename(&self.path, &previous) {
            // Not fatal, and not worth stopping over: the journal keeps growing,
            // which is what it did before this existed. Said once per attempt so
            // a full or read-only state directory is visible rather than
            // inferred from a file that never rolls.
            tracing::warn!(
                path = %self.path.display(), error = %e,
                "could not roll the journal over; it will keep growing"
            );
            return;
        }
        match OpenOptions::new().create(true).append(true).mode(0o600).open(&self.path) {
            Ok(f) => {
                tracing::info!(
                    previous = %previous.display(), bytes = self.bytes,
                    "journal rolled over"
                );
                self.file = Some(f);
                self.bytes = 0;
            }
            Err(e) => {
                tracing::warn!(
                    path = %self.path.display(), error = %e,
                    "rolled the journal over but could not reopen it; continuing in memory"
                );
                self.file = None;
                self.bytes = 0;
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&mut self, entry: &JournalEntry) {
        let Some(file) = &mut self.file else { return };
        match serde_json::to_string(entry) {
            Ok(line) => {
                if let Err(e) = writeln!(file, "{line}") {
                    tracing::warn!(error = %e, "could not append to journal");
                    return;
                }
                self.bytes += line.len() as u64 + 1;
            }
            Err(e) => tracing::warn!(error = %e, "could not serialise journal entry"),
        }
        // After the write, never before: a rotation must not split the entry
        // it was triggered by across two files.
        self.rotate_if_full();
    }

    /// Read entries back, newest last, at most [`MAX_BACKLOG`] of them.
    ///
    /// Reads the previous generation first so a rotation does not blank the
    /// history — the two together are one stream in the order it happened.
    /// Malformed lines are skipped rather than aborting the read: a truncated
    /// final line is normal after a crash, and after a rotation the previous
    /// generation ends in exactly that state.
    pub fn read_all(path: &Path) -> Vec<JournalEntry> {
        let mut out = std::collections::VecDeque::with_capacity(MAX_BACKLOG.min(1024));
        for file in [previous_of(path), path.to_path_buf()] {
            let Ok(file) = File::open(&file) else { continue };
            for entry in BufReader::new(file)
                .lines()
                .map_while(Result::ok)
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str(&l).ok())
            {
                // Bounded as it is read, not after: the whole point is that a
                // daemon's memory must not depend on how long ago the tool was
                // installed, and collecting first and truncating second would
                // have paid the cost anyway.
                if out.len() == MAX_BACKLOG {
                    out.pop_front();
                }
                out.push_back(entry);
            }
        }
        out.into()
    }
}

#[cfg(test)]
mod tests {
    use dozed_core::{Action, AppKey, Intent, MonoTime, Outcome, Reason, ReasonKind};

    use super::*;

    fn entry(n: u64) -> JournalEntry {
        let intent = Intent::new(
            AppKey::comm("slack"),
            Action::Freeze { reclaim: false },
            Reason::new(ReasonKind::RuleFired, "test"),
            MonoTime::from_millis(n),
        );
        JournalEntry::new(&intent, Outcome::WouldHave, n)
    }

    #[test]
    fn roundtrips_through_a_file() {
        let dir = std::env::temp_dir().join(format!("dozed-jt-{}", std::process::id()));
        let path = dir.join("journal.jsonl");
        let _ = std::fs::remove_dir_all(&dir);

        let mut j = JournalFile::open(path.clone());
        j.append(&entry(1));
        j.append(&entry(2));
        drop(j);

        let back = JournalFile::read_all(&path);
        assert_eq!(back.len(), 2);
        assert_eq!(back[1].at, MonoTime::from_millis(2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_torn_final_line_costs_one_entry_not_the_file() {
        let dir = std::env::temp_dir().join(format!("dozed-jt2-{}", std::process::id()));
        let path = dir.join("journal.jsonl");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let good = serde_json::to_string(&entry(1)).unwrap();
        std::fs::write(&path, format!("{good}\n{{\"at\":\"truncated")).unwrap();

        let back = JournalFile::read_all(&path);
        assert_eq!(back.len(), 1, "the intact entry must survive");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_journal_rolls_over_and_the_previous_generation_is_still_read() {
        let dir = std::env::temp_dir().join(format!("dozed-jt3-{}", std::process::id()));
        let path = dir.join("journal.jsonl");
        let _ = std::fs::remove_dir_all(&dir);

        // One entry is a few hundred bytes, so this is comfortably past the cap
        // without writing eight megabytes in a unit test: the file is opened,
        // filled by hand, and reopened.
        let mut j = JournalFile::open(path.clone());
        j.append(&entry(1));
        drop(j);
        let filler = "x".repeat(usize::try_from(MAX_JOURNAL_BYTES).unwrap());
        {
            use std::io::Write as _;
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(f, "{filler}").unwrap();
        }

        // Reopening rolls it over before writing anything.
        let mut j = JournalFile::open(path.clone());
        j.append(&entry(2));
        drop(j);

        assert!(previous_of(&path).exists(), "the previous generation is kept, not deleted");
        assert!(
            std::fs::metadata(&path).unwrap().len() < MAX_JOURNAL_BYTES,
            "and the live file starts again from nothing"
        );

        // Both generations read as one stream, in the order they happened.
        let back = JournalFile::read_all(&path);
        assert_eq!(back.len(), 2, "a rotation must not lose the entries either side of it");
        assert_eq!(back[0].at, MonoTime::from_millis(1));
        assert_eq!(back[1].at, MonoTime::from_millis(2));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_backlog_is_bounded_and_keeps_the_newest() {
        // The bug this closes: a daemon's resident memory grew with how long ago
        // the tool was installed. 3,947 entries were being loaded at every one
        // of 135 restarts on the machine this was found on.
        let dir = std::env::temp_dir().join(format!("dozed-jt4-{}", std::process::id()));
        let path = dir.join("journal.jsonl");
        let _ = std::fs::remove_dir_all(&dir);

        let mut j = JournalFile::open(path.clone());
        for n in 0..(MAX_BACKLOG as u64 + 50) {
            j.append(&entry(n));
        }
        drop(j);

        let back = JournalFile::read_all(&path);
        assert_eq!(back.len(), MAX_BACKLOG);
        assert_eq!(back[0].at, MonoTime::from_millis(50), "the oldest 50 were dropped");
        assert_eq!(
            back[MAX_BACKLOG - 1].at,
            MonoTime::from_millis(MAX_BACKLOG as u64 + 49),
            "and the newest entry is the one that survives, not the first"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unwritable_journal_does_not_stop_the_daemon() {
        let mut j = JournalFile::open(PathBuf::from("/proc/definitely/not/writable.jsonl"));
        j.append(&entry(1)); // must not panic
        assert!(JournalFile::read_all(Path::new("/nope")).is_empty());
    }
}
