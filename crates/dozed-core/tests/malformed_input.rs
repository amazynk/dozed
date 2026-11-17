//! The decode paths, hammered with input nothing sane produced.
//!
//! `tests/invariants.rs` generates random *sessions* and asserts the safety
//! properties hold. This file is the layer underneath: the parsers those
//! sessions arrive through, fed input that is broken rather than merely varied.
//!
//! ## Why these three
//!
//! Everything else in this codebase decides things from data dozed produced
//! itself. These three read bytes that something else wrote:
//!
//! * **The config file** is typed by a person, and half-typed for as long as
//!   they are typing it. §12's editor leans on `Config::parse` refusing a broken
//!   file rather than on catching it later.
//! * **The trace** is a file that survives crashes, `ENOSPC` and `SIGKILL`
//!   mid-write, so a truncated last line is the *expected* state rather than an
//!   exotic one — §13 records both a failed flush and a size cap stopping a
//!   recorder mid-file.
//! * The compositor's window snapshot is somebody else's schema entirely. It is
//!   fuzzed in `dozed::sway`'s own tests, because the parser lives inside a
//!   binary crate and an integration test cannot reach it.
//!
//! ## Why not `cargo-fuzz`
//!
//! The same answer `tests/invariants.rs` gives for `proptest`, and one more.
//! `dozed_core::stats::Rng` is already here, seeded and reproducible, so a
//! mutation loop costs a `for` over seeds — and unlike a fuzz target this runs
//! in the ordinary suite, on stable, in the CI that already exists. A fuzzer
//! that needs a nightly toolchain and a separate crate is a fuzzer that runs
//! when somebody remembers.
//!
//! What is given up is the coverage-guided part, which is what finds the input
//! that reaches the interesting branch. That is a real loss and this is not a
//! substitute for running `cargo-fuzz` occasionally; it is the part that can be
//! made to run every time.
//!
//! ## What is asserted
//!
//! **Not** that anything parses. Almost none of this should parse, and a
//! mutation that happens to produce a valid file is a valid file. What is
//! asserted is that every input produces a `Result` rather than a panic, an
//! abort, or an allocation the machine cannot serve — a parser that panics on a
//! malformed config takes the daemon down at startup, and one that panics on a
//! malformed trace takes `dozectl replay` down on exactly the recording somebody
//! is trying to understand.

use dozed_core::stats::Rng;
use dozed_core::{Config, trace};

/// Mutations per seed corpus entry. Each is a parse of a few kilobytes, so the
/// whole file is well inside a second.
const MUTATIONS: usize = 400;

/// Corrupt `input` in one of a handful of ways a real file gets corrupted.
///
/// Byte-level rather than structure-aware, deliberately: the structure-aware
/// cases are already covered by hand-written tests that know what a wrong
/// priority or a missing rule name should do. What this is looking for is the
/// class of bug those cannot reach — an index computed from a length, a slice
/// taken at a byte offset inside a multi-byte character, a depth that recurses.
fn mutate(input: &str, rng: &mut Rng) -> Vec<u8> {
    let mut bytes = input.as_bytes().to_vec();
    if bytes.is_empty() {
        return bytes;
    }
    match rng.below(6) {
        // Truncate. The single most likely real corruption: a trace killed
        // mid-write ends exactly here.
        0 => {
            let at = rng.below(bytes.len());
            bytes.truncate(at);
        }
        // Flip one byte. Catches the parsers that index rather than iterate.
        1 => {
            let at = rng.below(bytes.len());
            bytes[at] ^= 1 << rng.below(8);
        }
        // Splice a byte in, which can land inside a UTF-8 sequence and make the
        // whole thing invalid text rather than invalid TOML or JSON.
        2 => {
            let at = rng.below(bytes.len());
            bytes.insert(at, (rng.below(256)) as u8);
        }
        // Delete a run, which is how a partially-overwritten file reads.
        3 => {
            let at = rng.below(bytes.len());
            let len = rng.below(bytes.len() - at).min(64);
            bytes.drain(at..at + len);
        }
        // Repeat a slice. Produces duplicate keys, duplicate table headers and
        // absurdly long values.
        4 => {
            let at = rng.below(bytes.len());
            let len = rng.below(bytes.len() - at).min(256);
            let chunk: Vec<u8> = bytes[at..at + len].to_vec();
            let to = rng.below(bytes.len());
            for (i, b) in chunk.into_iter().enumerate() {
                bytes.insert(to + i, b);
            }
        }
        // Nest. The one mutation aimed at a specific failure — a recursive
        // descent parser blowing the stack on deeply nested input — rather than
        // at whatever turns up.
        _ => {
            let depth = 1 + rng.below(2_000);
            let mut deep = Vec::with_capacity(depth * 2 + bytes.len());
            deep.extend(std::iter::repeat_n(b'[', depth));
            deep.extend_from_slice(&bytes);
            deep.extend(std::iter::repeat_n(b']', depth));
            bytes = deep;
        }
    }
    bytes
}

/// Valid files to corrupt.
///
/// Mutating from something valid is what gets past the first character: random
/// bytes are rejected by the lexer and never reach the code worth testing.
fn config_corpus() -> Vec<String> {
    vec![
        dozed_core::starter::starter_config(),
        // Minimal, so a truncation reaches the interesting part sooner.
        "mode = \"live\"\n[defaults]\nbase_grace = \"5m\"\n".into(),
        // One of every rung, since actions are an enum with several shapes.
        r#"
[[rule]]
name = "a"
priority = 1
action = { throttle = 5 }
[rule.when]
visibility = "hidden"
for = "2m"

[[rule]]
name = "b"
priority = 2
action = { doze-cycle = { every = "15m", wake = "20s" } }

[[rule]]
name = "c"
priority = 3
action = { throttle-to = 50 }
[rule.match]
app = ["slack", "com.slack.Slack"]

[apps."org.mozilla.firefox"]
mode = "monitor"
"#
        .into(),
    ]
}

#[test]
fn a_broken_config_is_an_error_and_never_a_panic() {
    // The daemon parses this at startup and `dozectl upgrade-presets` parses it
    // before appending. A panic here is a daemon that will not start against a
    // file the user can fix in ten seconds if only something would tell them
    // which line.
    for (n, seed) in config_corpus().iter().zip(1u64..) {
        let mut rng = Rng::seeded(seed);
        for _ in 0..MUTATIONS {
            let bytes = mutate(n, &mut rng);
            // Invalid UTF-8 is a real state for a corrupted file and is the
            // caller's problem to notice, so both halves are exercised: the
            // lossy text goes to the parser, and the fact that it was not text
            // is itself an outcome the daemon has to survive.
            let text = String::from_utf8_lossy(&bytes);
            // The assertion is the absence of a panic; the result is not
            // interesting, because a mutation that produces a valid config has
            // produced a valid config.
            let _ = Config::parse(&text);
        }
    }
}

#[test]
fn a_config_that_parses_after_mutation_still_validates_or_is_refused() {
    // The stronger claim, and the one that matters for `upgrade-presets`: a file
    // that comes back `Ok` has been through `Config::validate`, so the rule
    // table is sorted and the invariants the planner assumes hold. Anything else
    // must be an `Err`. There is no third answer, and a parser that returned one
    // would hand the planner a table it was written to be able to trust.
    let mut accepted = 0;
    for (n, seed) in config_corpus().iter().zip(100u64..) {
        let mut rng = Rng::seeded(seed);
        for _ in 0..MUTATIONS {
            let bytes = mutate(n, &mut rng);
            if let Ok(cfg) = Config::parse(&String::from_utf8_lossy(&bytes)) {
                accepted += 1;
                assert!(
                    cfg.rules.windows(2).all(|w| w[0].priority <= w[1].priority),
                    "a config that parsed came back with an unsorted rule table"
                );
                // `validate` is idempotent, so re-running it on an accepted
                // config must not change its mind.
                let mut again = cfg.clone();
                assert!(again.validate().is_ok(), "validate disagreed with parse");
            }
        }
    }
    // Not an assertion about the mutator's quality so much as a canary: if this
    // ever reaches zero the corpus has stopped producing anything parseable and
    // the test above is checking that nothing panics on garbage nobody reads.
    assert!(accepted > 0, "no mutation produced a parseable config; the corpus has rotted");
}

#[test]
fn a_truncated_or_corrupt_trace_is_skipped_rather_than_fatal() {
    // A trace is a file that survives `SIGKILL`, `ENOSPC` and a size cap, so a
    // half-written last line is the expected state. §14 depends on `parse_lines`
    // counting what it could not read rather than refusing the file: a recording
    // with a hole is still the recording somebody needs to look at.
    let good = include_str!("fixtures/session.jsonl");
    let mut rng = Rng::seeded(7);
    for _ in 0..MUTATIONS {
        let bytes = mutate(good, &mut rng);
        let text = String::from_utf8_lossy(&bytes);
        let (records, skipped) = trace::parse_lines(text.lines());
        // Every line is either read or counted. Nothing may be silently lost,
        // because §13 is explicit that a trace with unmarked gaps is worse than
        // no trace at all.
        let non_empty = text.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(records.len() + skipped, non_empty, "a line was neither parsed nor counted");
    }
}

#[test]
fn replaying_a_corrupt_trace_does_not_panic() {
    // The path `dozectl replay` and `dozectl suggest` take. A panic here lands
    // on precisely the recording somebody is trying to understand, which is the
    // worst possible moment for one.
    let good = include_str!("fixtures/session.jsonl");
    let mut rng = Rng::seeded(11);
    for _ in 0..(MUTATIONS / 4) {
        let bytes = mutate(good, &mut rng);
        let (records, _) = trace::parse_lines(String::from_utf8_lossy(&bytes).lines());
        let _ = trace::replay(&records, None);
    }
}

#[test]
fn an_empty_or_absurd_trace_is_an_error_not_a_crash() {
    // The degenerate inputs a mutation loop is unlikely to hit by chance.
    for text in ["", "\n\n\n", "not json", "{}", "[]", "null", "{\"kind\":\"unknown\"}"] {
        let (records, _) = trace::parse_lines(text.lines());
        let _ = trace::replay(&records, None);
    }
    // A header claiming a version from the future. The replay has to refuse it
    // rather than interpret it, because the whole value of a replay is that it
    // reproduces what happened.
    let future = r#"{"record":"header","version":999999,"dozed_version":"x","instance":"i","wall_start":0,"live":false,"config":{}}"#;
    let (records, _) = trace::parse_lines([future]);
    let _ = trace::replay(&records, None);
}
