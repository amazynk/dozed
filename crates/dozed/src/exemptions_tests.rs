//! Tests for [`super`], in a file of their own, so that the file a reader opens to
//! understand what the code does is not mostly assertions about it. Still a child
//! module rather than an integration test: these reach private items, and the
//! `#[path]` attribute on the declaration keeps `super::*` meaning what it would at
//! the bottom of that file.

use super::*;

/// The PIDs one direction's list reports as using a stream: what the
/// exemption was derived from before silence was a question, and still is
/// once the silent ones are taken back out.
fn parse_streams(text: &str, header: &str) -> BTreeSet<u32> {
    active_pids(&parse_stream_list(text, header))
}

fn app_pids() -> BTreeMap<AppKey, Vec<u32>> {
    BTreeMap::from([
        (AppKey::comm("spotify"), vec![100, 101, 102]),
        (AppKey::comm("slack"), vec![200, 201]),
    ])
}

#[test]
fn any_process_in_the_tree_exempts_the_whole_app() {
    // A browser's audio comes out of a child process, not the one the
    // compositor reported a window for.
    let mut t = ExemptionTracker::new();
    let events = t.snapshot(Exemption::Audio, BTreeSet::from([102]), &app_pids());
    assert_eq!(
        events,
        vec![Event::ExemptionChanged {
            app: AppKey::comm("spotify"),
            exemption: Exemption::Audio,
            active: true,
        }]
    );
}

/// An app launched from a terminal, so the terminal's tree contains it.
fn terminal_and_child() -> BTreeMap<AppKey, Vec<u32>> {
    BTreeMap::from([
        // The terminal's tree includes everything started from it.
        (AppKey::desktop("wezterm"), vec![300, 301, 302, 400, 401]),
        (AppKey::desktop("impress"), vec![400, 401]),
    ])
}

#[test]
fn an_exemption_goes_to_the_app_that_holds_it_not_the_terminal_around_it() {
    // The observed bug: opening a LibreOffice presentation marked the
    // terminal it was launched from fullscreen too, because Impress's PID is
    // inside wezterm's tree. One PID, one owner — the most specific one.
    let mut t = ExemptionTracker::new();
    let events = t.snapshot(Exemption::Fullscreen, BTreeSet::from([400]), &terminal_and_child());
    assert_eq!(
        events,
        vec![Event::ExemptionChanged {
            app: AppKey::desktop("impress"),
            exemption: Exemption::Fullscreen,
            active: true,
        }],
        "the terminal must not inherit its child app's exemption"
    );
}

#[test]
fn an_exemption_is_never_dropped_only_attributed() {
    // The safety property. When the process holding an exemption belongs to
    // an app with no window of its own, the containing terminal is the only
    // candidate and must keep it — a narrower net here would mean freezing
    // something that is playing audio.
    let only_the_terminal =
        BTreeMap::from([(AppKey::desktop("wezterm"), vec![300, 301, 302, 400])]);
    let mut t = ExemptionTracker::new();
    let events = t.snapshot(Exemption::Audio, BTreeSet::from([400]), &only_the_terminal);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].app(), Some(&AppKey::desktop("wezterm")));
}

#[test]
fn a_browsers_own_child_still_exempts_the_browser() {
    // The rule this replaces must keep working: the audio PID is a child of
    // the browser, several levels below the window we resolved.
    let apps = BTreeMap::from([
        (AppKey::desktop("firefox"), vec![500, 501, 502, 503]),
        (AppKey::desktop("signal"), vec![600]),
    ]);
    let mut t = ExemptionTracker::new();
    let events = t.snapshot(Exemption::Audio, BTreeSet::from([503]), &apps);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].app(), Some(&AppKey::desktop("firefox")));
}

#[test]
fn one_app_may_own_several_exempt_pids_and_is_reported_once() {
    let mut t = ExemptionTracker::new();
    let events =
        t.snapshot(Exemption::Fullscreen, BTreeSet::from([400, 401]), &terminal_and_child());
    assert_eq!(events.len(), 1, "one app, one event: {events:?}");
    assert_eq!(events[0].app(), Some(&AppKey::desktop("impress")));
}

#[test]
fn equally_specific_claims_resolve_the_same_way_every_time() {
    // Two trees of the same size holding the PID: a replay that picked a
    // different owner would not be a replay, so Ord on AppKey decides.
    let tied = BTreeMap::from([
        (AppKey::desktop("b-app"), vec![700, 701]),
        (AppKey::desktop("a-app"), vec![700, 702]),
    ]);
    for _ in 0..3 {
        let mut t = ExemptionTracker::new();
        let events = t.snapshot(Exemption::Audio, BTreeSet::from([700]), &tied);
        assert_eq!(events[0].app(), Some(&AppKey::desktop("a-app")));
    }
}

/// `pactl list sink-inputs`, trimmed to the fields that matter.
const SINK_INPUTS: &str = r#"
Sink Input #48
	Driver: PipeWire
	Owner Module: n/a
	Client: 47
	Sink: 900
	Corked: no
	Mute: no
	Volume: front-left: 65536 / 100% / 0.00 dB,   front-right: 65536 / 100% / 0.00 dB
	        balance 0.00
	Properties:
		application.name = "mplayer"
		application.process.id = "12345"
		media.name = "audio stream"
Sink Input #49
	Driver: PipeWire
	Sink: 900
	Corked: yes
	Properties:
		application.name = "Spotify"
		application.process.id = "6789"
Sink Input #50
	Driver: PipeWire
	Sink: 900
	Corked: no
	Mute: no
	Volume: front-left: 65536 / 100% / 0.00 dB,   front-right: 65536 / 100% / 0.00 dB
	Properties:
		application.name = "Firefox"
		application.process.id = "222"
		media.name = "(505) Discord | #general"
"#;

/// `pactl list source-outputs`, trimmed the same way. The record header is
/// the only structural difference from the playback direction, which is why
/// one parser serves both.
const SOURCE_OUTPUTS: &str = r#"
Source Output #12
	Driver: PipeWire
	Owner Module: n/a
	Client: 47
	Corked: no
	Mute: no
	Properties:
		application.name = "Zoom"
		application.process.id = "4242"
		media.name = "capture stream"
Source Output #13
	Driver: PipeWire
	Corked: yes
	Properties:
		application.name = "Discord"
		application.process.id = "555"
"#;

#[test]
fn a_process_holding_the_microphone_is_capturing() {
    // The gap `Exemption::Capture` exists for. Playback detection cannot see
    // this at all: a dictation tool, a recorder, or somebody on a call whose
    // side is silent has a source-output and no sink-input, so every
    // playback-shaped detector reads it as an idle app.
    let capturing = parse_streams(SOURCE_OUTPUTS, "Source Output #");
    assert!(capturing.contains(&4242), "Zoom holds the microphone: {capturing:?}");
}

#[test]
fn a_corked_capture_stream_is_not_capturing() {
    // Corked means the same thing in both directions: the client has the
    // stream open and is not reading from it. A muted-at-the-OS Discord is a
    // perfectly good freeze candidate.
    assert!(!parse_streams(SOURCE_OUTPUTS, "Source Output #").contains(&555));
}

#[test]
fn the_two_directions_do_not_read_each_others_records() {
    // The headers are what separate them, so a parser given the wrong one
    // must find nothing rather than quietly matching on `Corked:` alone and
    // attributing playback PIDs to the microphone.
    assert!(parse_streams(SINK_INPUTS, "Source Output #").is_empty());
    assert!(parse_streams(SOURCE_OUTPUTS, "Sink Input #").is_empty());
}

#[test]
fn a_portal_session_path_names_the_client_that_owns_it() {
    // The `Closed` signal comes from the portal, not from the app, so the
    // only thing tying it back to a process is this path convention.
    assert_eq!(
        session_owner("/org/freedesktop/portal/desktop/session/1_42/dozed0"),
        Some(":1.42".to_owned())
    );
    // A unique name with more than two components still round-trips.
    assert_eq!(
        session_owner("/org/freedesktop/portal/desktop/session/1_2_3/tok"),
        Some(":1.2.3".to_owned())
    );
    assert_eq!(session_owner("/org/freedesktop/portal/desktop/request/1_42/tok"), None);
    assert_eq!(session_owner("/something/else"), None);
}

#[test]
fn a_cancelled_share_does_not_exempt_an_app_forever() {
    // Why sessions are counted rather than `Start` calls being latched: the
    // user opening the picker and pressing cancel is indistinguishable from
    // a real share until the portal closes the session, and an app latched on
    // `Start` would stay exempt for the rest of its life.
    let mut s = ScreenCastSessions::default();
    s.opened(":1.42", 4242);
    assert_eq!(s.open.len(), 1);
    assert!(s.closed(":1.42"));
    assert!(s.open.is_empty(), "the session closed, so nothing is being shared");
}

#[test]
fn two_shares_from_one_app_need_two_closes() {
    // Sharing a second window while the first is still up. Dropping the
    // exemption on the first close would freeze an app that is still on
    // somebody's screen.
    let mut s = ScreenCastSessions::default();
    s.opened(":1.42", 4242);
    s.opened(":1.42", 4242);
    assert!(s.closed(":1.42"));
    assert_eq!(s.open.get(":1.42").map(|e| e.1), Some(1), "still sharing the other one");
    assert!(s.closed(":1.42"));
    assert!(s.open.is_empty());
}

#[test]
fn closing_a_session_nobody_opened_changes_nothing() {
    // `Session.Close` is not unique to screencasts — the portal uses the same
    // interface for file chooser and remote desktop sessions, which this
    // source never saw opened. Treating those as a change would publish a
    // snapshot per unrelated portal call.
    let mut s = ScreenCastSessions::default();
    assert!(!s.closed(":1.99"));
}

#[test]
fn a_source_says_what_it_cannot_see() {
    // The rule this enforces: anything the daemon cannot do, it says so
    // where a user looks. A missing `pactl` used to be one line in a log.
    let mut t = ExemptionTracker::new();
    t.set_health(Exemption::Capture, None, Some("no `pactl`".into()));
    let status = t.status(false);
    let capture = status.iter().find(|s| s.name == "capture").expect("capture is a source");
    assert_eq!(capture.degraded.as_deref(), Some("no `pactl`"));

    // And stops saying it once the utility works again.
    t.set_health(Exemption::Capture, None, None);
    let status = t.status(false);
    let capture = status.iter().find(|s| s.name == "capture").unwrap();
    assert_eq!(capture.degraded, None);
}

#[test]
fn a_platform_limit_outranks_a_reported_one() {
    // Clipboard on Wayland is not observable at all, and a source that also
    // failed to start there must not downgrade that to "degraded" — the user
    // would read it as fixable.
    let mut t = ExemptionTracker::new();
    t.set_health(Exemption::ClipboardOwner, Some("could not start".into()), None);
    let status = t.status(false);
    let clip = status.iter().find(|s| s.name == "clipboard-owner").unwrap();
    assert!(
        clip.unavailable.as_deref().is_some_and(|w| w.contains("Wayland")),
        "{:?}",
        clip.unavailable
    );
}

#[test]
fn a_player_that_speaks_no_mpris_is_still_seen_playing() {
    // The gap this closes, measured over a real session: MPlayer played video
    // twice and the audio exemption fired zero times, because it implements no
    // MPRIS interface — there were no MPRIS names on the bus at all. Only
    // `fullscreen-exempt` saved it; a windowed player would have been frozen
    // mid-playback, which is what these sources exist to prevent.
    let playing = parse_streams(SINK_INPUTS, "Sink Input #");
    assert!(playing.contains(&12345), "mplayer is playing: {playing:?}");
    assert!(playing.contains(&222), "so is firefox: {playing:?}");
}

#[test]
fn a_corked_stream_is_not_playing() {
    // Corked is the difference between "has a stream" and "is playing". A
    // paused Spotify is a perfectly good freeze candidate, and `slow-to-wake`
    // exists precisely to give it a longer leash rather than an exemption.
    assert!(!parse_streams(SINK_INPUTS, "Sink Input #").contains(&6789));
}

#[test]
fn nothing_playing_and_nothing_readable_are_different_answers() {
    // An empty list means silence. A *failure* must not be reported as
    // silence, or a transient error makes a playing player freezable — which
    // is why `playing_streams` returns `Option` and the caller keeps its last
    // belief. Here, only the parse half can be tested without a sound server.
    assert!(parse_streams("", "Sink Input #").is_empty());
    assert!(
        parse_streams("Sink Input #1\n\tCorked: no\n", "Sink Input #").is_empty(),
        "no PID, no claim"
    );
}

#[test]
fn the_last_stream_in_the_list_is_not_dropped() {
    // The parser flushes on the *next* header, so the final block needs a
    // flush of its own — an off-by-one here would silently lose whichever
    // player happened to be listed last.
    let one = "Sink Input #7\n\tCorked: no\n\tapplication.process.id = \"99\"\n";
    assert_eq!(parse_streams(one, "Sink Input #"), BTreeSet::from([99]));
}

#[test]
fn an_unchanged_snapshot_says_nothing() {
    let mut t = ExemptionTracker::new();
    let pids = BTreeSet::from([100]);
    assert_eq!(t.snapshot(Exemption::Audio, pids.clone(), &app_pids()).len(), 1);
    assert!(
        t.snapshot(Exemption::Audio, pids, &app_pids()).is_empty(),
        "republishing the same snapshot must not churn the planner"
    );
}

#[test]
fn clearing_a_snapshot_retracts_the_exemption() {
    let mut t = ExemptionTracker::new();
    t.snapshot(Exemption::Audio, BTreeSet::from([100]), &app_pids());
    let events = t.snapshot(Exemption::Audio, BTreeSet::new(), &app_pids());
    assert_eq!(
        events,
        vec![Event::ExemptionChanged {
            app: AppKey::comm("spotify"),
            exemption: Exemption::Audio,
            active: false,
        }]
    );
}

#[test]
fn sources_do_not_interfere_with_each_other() {
    let mut t = ExemptionTracker::new();
    t.snapshot(Exemption::Audio, BTreeSet::from([100]), &app_pids());
    let events = t.snapshot(Exemption::DbusNameOwner, BTreeSet::from([200]), &app_pids());
    assert_eq!(events.len(), 1, "only the D-Bus exemption changed");
    assert_eq!(events[0].app(), Some(&AppKey::comm("slack")));
    // And clearing one must not clear the other.
    let events = t.snapshot(Exemption::DbusNameOwner, BTreeSet::new(), &app_pids());
    assert_eq!(events.len(), 1);
    assert!(t.pids_for(Exemption::Audio).unwrap().contains(&100));
}

#[test]
fn an_app_discovered_after_the_snapshot_still_becomes_exempt() {
    // A media player is usually already playing by the time the compositor
    // reports its window, so ordering here is the normal case, not the edge.
    let mut t = ExemptionTracker::new();
    let events = t.snapshot(Exemption::Audio, BTreeSet::from([300]), &app_pids());
    assert!(events.is_empty(), "nothing known owns pid 300 yet");

    let mut later = app_pids();
    later.insert(AppKey::comm("vlc"), vec![300]);
    let events = t.reresolve(&later);
    assert_eq!(
        events,
        vec![Event::ExemptionChanged {
            app: AppKey::comm("vlc"),
            exemption: Exemption::Audio,
            active: true,
        }]
    );
}

#[test]
fn an_app_that_exits_stops_being_reported() {
    let mut t = ExemptionTracker::new();
    t.snapshot(Exemption::Audio, BTreeSet::from([100]), &app_pids());
    let mut gone = app_pids();
    gone.remove(&AppKey::comm("spotify"));
    let events = t.reresolve(&gone);
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0],
        Event::ExemptionChanged {
            app: AppKey::comm("spotify"),
            exemption: Exemption::Audio,
            active: false,
        }
    );
}

// ---- audio that cannot be heard ------------------------------------

fn meter() -> SilenceMeter {
    SilenceMeter::new(dozed_core::config::AudioConfig::default())
}

/// Fold a pass in which every sink the streams point at was measured.
///
/// The ordinary case, and the one nearly every test below is about. The
/// pass where a sink could *not* be measured has a test of its own, because
/// that is the interesting one.
fn fold_all(m: &mut SilenceMeter, streams: &[Stream], quiet: &BTreeSet<u32>) {
    let heard: BTreeSet<u32> = streams.iter().filter_map(|s| s.sink).collect();
    m.fold(streams, quiet, &heard);
}

/// The two uncorked streams from `SINK_INPUTS`, both on sink 900.
fn open_streams() -> Vec<Stream> {
    parse_stream_list(SINK_INPUTS, "Sink Input #")
}

#[test]
fn a_stream_record_carries_the_sink_it_feeds() {
    // Without this the silence measurement has nothing to listen to: a
    // sink-input is addressed by index, and the only thing that can be
    // addressed by name is the sink underneath it.
    let streams = open_streams();
    let firefox = streams.iter().find(|s| s.pid == Some(222)).expect("firefox is listed");
    assert_eq!(firefox.index, 50);
    assert_eq!(firefox.sink, Some(900));
    assert!(!firefox.corked && !firefox.muted && !firefox.volume_zero);
}

#[test]
fn a_stream_carries_what_the_app_calls_it() {
    // The question `audio-silent` provokes: "firefox is holding a silent
    // stream" is a fact about a process, and what a reader wants is which
    // tab. `media.name` is where a browser puts the tab title.
    let streams = open_streams();
    let firefox = streams.iter().find(|s| s.pid == Some(222)).unwrap();
    assert_eq!(firefox.title.as_deref(), Some("(505) Discord | #general"));
    // A record with no `media.name` claims no title rather than an empty one.
    let spotify = streams.iter().find(|s| s.pid == Some(6789)).unwrap();
    assert_eq!(spotify.title, None);
}

#[test]
fn only_streams_actually_in_use_are_named() {
    // A paused tab is not what anyone is asking about, and a corked stream
    // is exactly that. Duplicates collapse: two streams with one name should
    // say the name once.
    let mut streams = open_streams();
    streams.push(Stream {
        index: 51,
        pid: Some(222),
        title: Some("(505) Discord | #general".into()),
        ..Stream::default()
    });
    let titles = stream_report(&streams, &meter());
    assert_eq!(titles.get(&222).map(Vec::len), Some(1), "{titles:?}");
    assert_eq!(titles.get(&12345).map(|v| v[0].name.as_str()), Some("audio stream"), "{titles:?}");
    assert!(!titles.contains_key(&6789), "the corked Spotify stream is not in use");
}

#[test]
fn a_stream_name_lands_on_the_app_that_holds_it() {
    // Attributed through the same rule the exemption uses, so the browser
    // gets its child's stream and the terminal an app was launched from does
    // not get that app's.
    let mut t = ExemptionTracker::new();
    t.set_stream_titles(BTreeMap::from([(
        400,
        vec![AudioStream { name: "a presentation".into(), silent_for: None }],
    )]));
    let apps = terminal_and_child();
    let named: Vec<String> =
        t.titles_for(&AppKey::desktop("impress"), &apps).into_iter().map(|s| s.name).collect();
    assert_eq!(named, vec!["a presentation"]);
    assert!(t.titles_for(&AppKey::desktop("wezterm"), &apps).is_empty());
}

#[test]
fn an_app_with_nothing_open_is_named_nothing() {
    let t = ExemptionTracker::new();
    assert!(t.titles_for(&AppKey::comm("spotify"), &app_pids()).is_empty());
}

#[test]
fn a_volume_line_is_silent_only_when_every_channel_is() {
    assert!(all_channels_silent(" front-left: 0 / 0% / -inf dB,   front-right: 0 / 0% / -inf"));
    assert!(!all_channels_silent(" front-left: 0 / 0% / -inf dB,   front-right: 65536 / 100%"));
    assert!(!all_channels_silent(" front-left: 65536 / 100% / 0.00 dB"));
    // The continuation line under a `Volume:` claims nothing either way; a
    // parser that read `balance 0.00` as "no channels above zero" would call
    // every stream on the machine silent.
    assert!(!all_channels_silent(" balance 0.00"));
}

#[tokio::test(start_paused = true)]
async fn a_stream_that_makes_no_sound_stops_counting_as_audio() {
    // The case this exists for, measured on a real desktop: a Firefox with a
    // call tab open held a sink-input marked `Corked: no` at 100% volume
    // whose peak was exactly zero over four seconds, and `playing_audio` was
    // true the whole time.
    let mut m = meter();
    let streams = open_streams();
    let quiet = BTreeSet::from([900]);

    fold_all(&mut m, &streams, &quiet);
    assert!(
        m.silent_pids(&streams).is_empty(),
        "one quiet moment is a gap between tracks, not a silent app"
    );

    tokio::time::advance(Duration::from_secs(61)).await;
    fold_all(&mut m, &streams, &quiet);
    assert_eq!(
        m.silent_pids(&streams),
        BTreeSet::from([12345, 222]),
        "a minute of measured silence is not audio"
    );
}

#[tokio::test(start_paused = true)]
async fn one_audible_moment_puts_the_clock_back_to_zero() {
    // Silence has to be *continuous*. Without the reset an app that plays
    // for fifty-nine seconds out of every sixty accumulates its way to
    // silent and gets frozen mid-track.
    let mut m = meter();
    let streams = open_streams();

    fold_all(&mut m, &streams, &BTreeSet::from([900]));
    tokio::time::advance(Duration::from_secs(59)).await;
    fold_all(&mut m, &streams, &BTreeSet::new());
    tokio::time::advance(Duration::from_secs(59)).await;
    fold_all(&mut m, &streams, &BTreeSet::from([900]));
    assert!(m.silent_pids(&streams).is_empty(), "the audible moment restarted the clock");
}

#[tokio::test(start_paused = true)]
async fn an_app_with_one_audible_stream_is_playing_whatever_its_others_do() {
    // "Every stream silent", not "any". A browser with a silent call tab and
    // a video playing in another is playing.
    let mut m = meter();
    let mut streams = open_streams();
    // Both uncorked streams now belong to one app, on two different sinks.
    for s in &mut streams {
        if s.pid == Some(12345) {
            s.pid = Some(222);
            s.sink = Some(901);
        }
    }
    fold_all(&mut m, &streams, &BTreeSet::from([900]));
    tokio::time::advance(Duration::from_secs(61)).await;
    fold_all(&mut m, &streams, &BTreeSet::from([900]));
    assert!(m.silent_pids(&streams).is_empty(), "sink 901 was never heard to be quiet");
}

#[tokio::test(start_paused = true)]
async fn a_muted_stream_needs_no_measurement() {
    // Nothing comes out of a muted stream or one turned all the way down,
    // and the sound server says so in the listing — so this half of the
    // answer costs nothing and works even where `parec` does not.
    let mut m = meter();
    let mut streams = open_streams();
    for s in &mut streams {
        s.muted = true;
    }
    fold_all(&mut m, &streams, &BTreeSet::new());
    tokio::time::advance(Duration::from_secs(61)).await;
    fold_all(&mut m, &streams, &BTreeSet::new());
    assert_eq!(m.silent_pids(&streams), BTreeSet::from([12345, 222]));
}

#[tokio::test(start_paused = true)]
async fn a_sink_that_could_not_be_measured_neither_confirms_nor_resets() {
    // Observed on a real desktop: the sink carrying a browser's stream was
    // index 82826, then 52418, then 83175, then 83893 within a few minutes
    // of HDMI churn. A pass whose stream list straddles a re-enumeration
    // names a sink that is not in the sink list any more and can measure
    // nothing — and reading that as "audible" threw away the run of silence
    // every few minutes and pushed the crossing well past `silence_after`.
    let mut m = meter();
    let streams = open_streams();
    fold_all(&mut m, &streams, &BTreeSet::from([900]));
    tokio::time::advance(Duration::from_secs(30)).await;

    // A pass that measured nothing at all: sink 900 is in neither set.
    m.fold(&streams, &BTreeSet::new(), &BTreeSet::new());
    tokio::time::advance(Duration::from_secs(31)).await;
    fold_all(&mut m, &streams, &BTreeSet::from([900]));
    assert_eq!(
        m.silent_pids(&streams),
        BTreeSet::from([12345, 222]),
        "a missed measurement is not evidence of sound"
    );
}

#[tokio::test(start_paused = true)]
async fn silence_nobody_has_confirmed_lately_stops_counting() {
    // The other half of that: not resetting on a missed pass must not let a
    // stream cross on one measurement followed by a minute of failures, so
    // the evidence has to be recent as well as old enough.
    let mut m = meter();
    let streams = open_streams();
    fold_all(&mut m, &streams, &BTreeSet::from([900]));
    // Three probe intervals of nothing, and the one confirmation goes stale.
    tokio::time::advance(Duration::from_secs(61)).await;
    assert!(
        m.silent_pids(&streams).is_empty(),
        "60s is past silence_after and past three 15s probes with no confirmation"
    );
    // One good pass brings it straight back: the run was never broken.
    fold_all(&mut m, &streams, &BTreeSet::from([900]));
    assert_eq!(m.silent_pids(&streams), BTreeSet::from([12345, 222]));
}

#[tokio::test(start_paused = true)]
async fn a_stream_reports_how_long_it_has_been_silent() {
    // What the dashboard shows while an app is still counted as audio: the
    // measurement agreeing with the user and partway to the threshold, which
    // is a different thing from the measurement disagreeing with them.
    let mut m = meter();
    let streams = open_streams();
    assert!(stream_report(&streams, &m)[&222][0].silent_for.is_none(), "nothing measured yet");

    fold_all(&mut m, &streams, &BTreeSet::from([900]));
    tokio::time::advance(Duration::from_secs(20)).await;
    fold_all(&mut m, &streams, &BTreeSet::from([900]));
    assert_eq!(stream_report(&streams, &m)[&222][0].silent_for, Some(Duration::from_secs(20)));
    assert!(m.silent_pids(&streams).is_empty(), "20s is not yet a minute");
}

#[tokio::test(start_paused = true)]
async fn a_stream_that_ends_does_not_leave_its_silence_behind() {
    // Sink-input indices are reused. A dead stream's accumulated silence
    // left in the map would be inherited by the next stream to take its
    // number, which on a machine where one just ended is very often the same
    // app about to play something.
    let mut m = meter();
    let streams = open_streams();
    fold_all(&mut m, &streams, &BTreeSet::from([900]));
    tokio::time::advance(Duration::from_secs(61)).await;

    let reborn: Vec<Stream> =
        streams.iter().map(|s| Stream { pid: Some(4242), ..s.clone() }).collect();
    m.forget_dead(&[]);
    fold_all(&mut m, &reborn, &BTreeSet::from([900]));
    assert!(m.silent_pids(&reborn).is_empty(), "index 50 is a different stream now");
}

#[tokio::test(start_paused = true)]
async fn an_app_with_no_stream_at_all_is_not_silent() {
    // `audio-silent` means "holding a stream and not using it", which is a
    // narrower claim than "quiet" — every app on the machine is quiet.
    let mut m = meter();
    fold_all(&mut m, &[], &BTreeSet::from([900]));
    tokio::time::advance(Duration::from_secs(61)).await;
    assert!(m.silent_pids(&[]).is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_corked_stream_is_neither_playing_nor_silent() {
    // A paused Spotify was already a freeze candidate through `slow-to-wake`
    // and gains nothing from being called silent as well.
    let mut m = meter();
    let streams = open_streams();
    fold_all(&mut m, &streams, &BTreeSet::from([900]));
    tokio::time::advance(Duration::from_secs(61)).await;
    fold_all(&mut m, &streams, &BTreeSet::from([900]));
    assert!(!m.silent_pids(&streams).contains(&6789), "6789 is corked");
}

#[tokio::test(start_paused = true)]
async fn switching_the_measurement_off_forgets_what_it_measured() {
    // Turning it back on must start from nothing rather than from a clock
    // that has been running the whole time it was off, or the first pass
    // after a reload reports silence it never heard.
    let mut m = meter();
    let streams = open_streams();
    fold_all(&mut m, &streams, &BTreeSet::from([900]));
    tokio::time::advance(Duration::from_secs(61)).await;

    m.reconfigure(dozed_core::config::AudioConfig {
        detect_silence: false,
        ..dozed_core::config::AudioConfig::default()
    });
    assert!(m.silent_pids(&streams).is_empty());
    assert_eq!(
        m.due_in(true),
        Duration::from_secs(3600),
        "nothing to measure means nothing to wake up for"
    );
}

#[tokio::test(start_paused = true)]
async fn a_busy_audio_graph_cannot_starve_the_measurement() {
    // Why the deadline is absolute. This loop is also woken by `pactl
    // subscribe`, and a relative timer restarted on every one of those would
    // mean a stream that changes twice a second is measured never — which is
    // exactly the stream worth measuring.
    let mut m = meter();
    for _ in 0..20 {
        if m.due_in(true).is_zero() {
            return;
        }
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    panic!("the probe never came due");
}

/// What the whole chain says about this machine, right now.
///
/// Not an assertion — there is no telling what is playing on the machine a
/// test runs on — but the only way to check the parsing, the sink lookup and
/// the measurement against a real sound server rather than a fixture. Run it
/// with a browser open and something paused in it:
///
/// ```text
/// cargo test -p dozed audio_on_this_machine -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "reads the live sound server and prints what it hears"]
async fn audio_on_this_machine() {
    let Some(streams) = playing_streams().await else {
        println!("no `pactl`, or the sound server is not answering");
        return;
    };
    let monitors = sink_monitors().await.unwrap_or_default();
    for s in &streams {
        println!(
            "stream #{} pid {:?} sink {:?} corked {} muted {} volume_zero {} title {:?}",
            s.index, s.pid, s.sink, s.corked, s.muted, s.volume_zero, s.title
        );
    }
    let m = meter();
    println!("streams by pid: {:?}", stream_report(&streams, &m));
    let sinks: BTreeSet<u32> =
        streams.iter().filter(|s| !s.corked).filter_map(|s| s.sink).collect();
    for sink in sinks {
        let Some(monitor) = monitors.get(&sink) else {
            println!("sink {sink} has no monitor in the sink list");
            continue;
        };
        let peak = sink_peak(monitor, Duration::from_millis(750)).await;
        let silent = matches!(peak, Measured::Peak(p) if p <= m.threshold());
        println!("sink {sink} ({monitor}): {peak:?}, silent {silent}");
    }
}

#[tokio::test(start_paused = true)]
async fn nothing_playing_costs_no_wakeups() {
    // §13: this is not a timer the daemon runs, it is a timer that runs
    // while something is playing.
    assert_eq!(meter().due_in(false), Duration::from_secs(3600));
    assert_eq!(meter().due_in(true), Duration::from_secs(15));
}

#[test]
fn the_default_threshold_is_digital_silence_and_not_quiet_music() {
    let m = meter();
    // A stream writing zeros, which is the whole point.
    assert!(0.0 <= m.threshold());
    // A couple of least-significant bits of a 16-bit sample: what a
    // resampler's rounding leaves behind, and nothing anybody can hear.
    // Deliberately under the line, so digital silence that has been through
    // a rate conversion still reads as silence.
    assert!(2.0 / 32768.0 <= m.threshold(), "threshold {}", m.threshold());
    // Music at -60 dBFS is very quiet and is emphatically not silence.
    assert!(0.001 > m.threshold(), "threshold {}", m.threshold());
}

#[test]
fn only_idle_block_inhibitors_count() {
    // Measured on a live KDE session; see the doc comment. The signal-desktop
    // row is the one that matters: a sleep/delay inhibitor is "let me finish
    // before suspend", not "keep me awake", and treating it as an exemption
    // would permanently exempt a chat app.
    assert!(is_idle_inhibitor("idle:handle-lid-switch", "block", 1000, 1000));
    assert!(is_idle_inhibitor("idle", "block", 1000, 1000));

    assert!(!is_idle_inhibitor("sleep", "delay", 1000, 1000), "signal-desktop");
    assert!(!is_idle_inhibitor("idle", "delay", 1000, 1000), "delay is not a hold");
    assert!(!is_idle_inhibitor("shutdown", "block", 1000, 1000), "not about idleness");
    assert!(!is_idle_inhibitor("idle", "block", 0, 1000), "root's business, not ours");
    // `what` is colon-separated and must not match as a substring: an
    // inhibitor for a hypothetical "idleness" key is not an idle inhibitor.
    assert!(!is_idle_inhibitor("handle-lid-switch:idleness", "block", 1000, 1000));
}
