//! The shared visibility fixtures.
//!
//! `DESIGN.md` §5 is one rule with three producers: a KWin script, a GNOME Shell
//! extension and the daemon's native X11 source. They need shared test
//! fixtures so they cannot silently disagree; centralising the *rule* in
//! `dozed_core::visibility` gets most of the way there, and this file is the
//! rest — a table of desktop situations, each with the answer every platform
//! must give.
//!
//! The fixtures are also written out as JSON next to this file's expectations so
//! a helper can be checked against them by hand:
//!
//! ```sh
//! cargo test -p dozed-core --test visibility_fixtures -- --nocapture
//! ```

use dozed_core::visibility::{WindowFacts, most_visible, resolve};
use dozed_core::{Rect, Visibility};

const SCREEN: Rect = Rect { x: 0, y: 0, w: 1920, h: 1080 };
const HALF: Rect = Rect { x: 0, y: 0, w: 960, h: 1080 };

fn w(pid: u32, stack: u32, geom: Rect) -> WindowFacts {
    WindowFacts { pid, stack, geometry: Some(geom), output: "0".into(), ..Default::default() }
}

/// One named situation and the visibility every source must report for it.
struct Fixture {
    name: &'static str,
    windows: Vec<WindowFacts>,
    expect: Vec<Visibility>,
}

fn fixtures() -> Vec<Fixture> {
    use Visibility::{Focused, Hidden, VisibleUnfocused};

    let focused = |mut win: WindowFacts| {
        win.focused = true;
        win
    };
    let minimised = |mut win: WindowFacts| {
        win.minimized = true;
        win
    };
    let elsewhere = |mut win: WindowFacts| {
        win.on_current_desktop = false;
        win
    };
    let on_output = |mut win: WindowFacts, out: &str| {
        win.output = out.to_owned();
        win
    };

    vec![
        Fixture {
            name: "a single focused window",
            windows: vec![focused(w(1, 0, SCREEN))],
            expect: vec![Focused],
        },
        Fixture {
            name: "one window, not focused, nothing over it",
            windows: vec![w(1, 0, SCREEN)],
            expect: vec![VisibleUnfocused],
        },
        Fixture {
            name: "a maximised window covering another",
            windows: vec![w(1, 0, SCREEN), focused(w(2, 1, SCREEN))],
            expect: vec![Hidden, Focused],
        },
        Fixture {
            name: "two half-screen windows side by side",
            windows: vec![
                w(1, 0, Rect { x: 0, y: 0, w: 960, h: 1080 }),
                focused(w(2, 1, Rect { x: 960, y: 0, w: 960, h: 1080 })),
            ],
            expect: vec![VisibleUnfocused, Focused],
        },
        Fixture {
            name: "a half-covering window hides nothing",
            windows: vec![w(1, 0, SCREEN), focused(w(2, 1, HALF))],
            expect: vec![VisibleUnfocused, Focused],
        },
        Fixture {
            name: "minimised is hidden regardless of geometry",
            windows: vec![minimised(w(1, 0, SCREEN))],
            expect: vec![Hidden],
        },
        Fixture {
            name: "another workspace is hidden",
            windows: vec![elsewhere(w(1, 0, SCREEN))],
            expect: vec![Hidden],
        },
        Fixture {
            name: "the second monitor: unfocused but fully visible",
            windows: vec![
                on_output(w(1, 0, SCREEN), "1"),
                focused(on_output(w(2, 1, SCREEN), "0")),
            ],
            expect: vec![VisibleUnfocused, Focused],
        },
        Fixture {
            name: "a minimised window on top covers nothing",
            windows: vec![w(1, 0, SCREEN), minimised(w(2, 1, SCREEN))],
            expect: vec![VisibleUnfocused, Hidden],
        },
        Fixture {
            name: "a window on another workspace covers nothing",
            windows: vec![w(1, 0, SCREEN), elsewhere(w(2, 1, SCREEN))],
            expect: vec![VisibleUnfocused, Hidden],
        },
        Fixture {
            name: "stacking order decides, not list order",
            windows: vec![w(1, 5, SCREEN), w(2, 1, SCREEN)],
            expect: vec![VisibleUnfocused, Hidden],
        },
        Fixture {
            name: "a platform that reports no geometry never gets anything frozen",
            windows: vec![WindowFacts { pid: 1, ..Default::default() }],
            expect: vec![VisibleUnfocused],
        },
    ]
}

#[test]
fn every_platform_must_agree_on_these() {
    let mut failures = Vec::new();
    for f in fixtures() {
        let got = resolve(&f.windows);
        if got != f.expect {
            failures
                .push(format!("{}\n    expected {:?}\n    got      {:?}", f.name, f.expect, got));
        }
    }
    assert!(failures.is_empty(), "visibility fixtures failed:\n  {}", failures.join("\n  "));
}

#[test]
fn print_the_fixtures_as_json_for_helper_authors() {
    // Run with --nocapture to get a table a helper can be checked against by
    // hand. Cheap to keep, and the alternative is a second copy of the rules in
    // prose that goes stale.
    println!("\n{:<56} EXPECTED", "SITUATION");
    for f in fixtures() {
        let got = resolve(&f.windows);
        let states: Vec<&str> = got.iter().map(|v| v.as_str()).collect();
        println!("{:<56} {}", f.name, states.join(", "));
    }
    println!();
}

#[test]
fn an_app_with_one_visible_window_is_visible() {
    // §5's folding rule, which the daemon applies after this: the maximum over
    // an app's windows. A browser with a hidden window and a focused one is
    // focused, not half-frozen.
    let windows = vec![
        WindowFacts { pid: 100, minimized: true, ..Default::default() },
        WindowFacts { pid: 100, focused: true, ..Default::default() },
    ];
    let got = resolve(&windows);
    assert_eq!(got, vec![Visibility::Hidden, Visibility::Focused]);
    assert_eq!(most_visible(got), Some(Visibility::Focused));
}

#[test]
fn resolving_is_stable_under_reordering() {
    // Sources emit windows in whatever order the compositor hands them over.
    // The answer must depend on `stack`, never on position in the list.
    let a = vec![w(1, 0, SCREEN), w(2, 1, SCREEN)];
    let b = vec![w(2, 1, SCREEN), w(1, 0, SCREEN)];
    let ra = resolve(&a);
    let rb = resolve(&b);
    assert_eq!(ra[0], rb[1]);
    assert_eq!(ra[1], rb[0]);
}
