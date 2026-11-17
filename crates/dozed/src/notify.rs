//! Desktop notifications, and the weekly digest that is the only one dozed
//! sends on its own.
//!
//! ## Why a digest at all
//!
//! A tool whose whole argument is a number nobody reads is a tool nobody keeps.
//! `dozectl report` has always been able to say what dozing saved, and asking a
//! user to remember to run it weekly is asking for the number never to be seen —
//! so the one case where dozed speaks first is the one where it has something to
//! report about itself.
//!
//! ## Why exactly one, and weekly
//!
//! Because the alternative is what every other background tool does. A
//! notification per freeze would be dozens a day for a daemon whose entire
//! premise is that you cannot tell it is running (§1); a daily one is still
//! noise from something with nothing new to say. Weekly is roughly the interval
//! at which the figure has actually changed, and it is the one a user can ignore
//! without missing anything.
//!
//! The digest is **off during dry-run's first week on purpose**: it reports what
//! was saved or would have been, and either is worth reading, but a report with
//! no hours behind it is a notification that says nothing. So it waits for a
//! week of data, not merely a week of clock.

use std::time::{Duration, SystemTime};

use dozed_core::Report;

/// How often the digest fires.
pub const EVERY: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
}

/// Send one notification, or quietly do without.
///
/// Failure is never worth escalating: a machine with no notification daemon is a
/// normal machine, and a battery tool that logs an error every time it cannot
/// congratulate itself is worse than one that says nothing.
pub async fn send(summary: &str, body: &str) {
    let Ok(conn) = zbus::Connection::session().await else { return };
    let Ok(proxy) = NotificationsProxy::new(&conn).await else { return };
    let hints = std::collections::HashMap::new();
    // Low urgency and a normal timeout: this is information, never an alert.
    match proxy.notify("dozed", 0, "media-playback-start", summary, body, &[], hints, 10_000).await
    {
        Ok(_) => tracing::debug!(summary, "notified"),
        Err(e) => tracing::debug!(error = %e, "no notification daemon"),
    }
}

/// The weekly digest's text, from the same report `dozectl report` prints.
///
/// Split out from the sending so it can be tested without a session bus, and
/// worded so the dry-run case reads as an invitation rather than as a claim:
/// "would have" is the whole point of the first week, and rounding it into
/// "did" is the one thing this notification must never do.
///
/// The figure quoted is [`Report::covered`], not `total_dozed`. That is
/// deliberate and it is the conservative choice: app-time is a sum across apps
/// and legitimately exceeds the wall clock — six apps held for five hours is
/// twenty-five hours of it — which is defensible in a table with a legend beside
/// it and indefensible in a one-line notification, where "dozed saved you 25
/// hours this week" reads as a straightforward lie about a 168-hour week.
#[must_use]
pub fn digest(report: &Report, battery_wh: Option<f64>) -> (String, String) {
    let days = report.window.as_secs_f64() / 86_400.0;

    let summary = if report.dry_run {
        "dozed: what it would have saved".to_owned()
    } else {
        "dozed: what it saved this week".to_owned()
    };

    let mut body = if report.dry_run {
        format!(
            "Over {days:.0} days of dry-run, dozed would have held an app for {} of the \
             time, saving {} it never actually touched.",
            dozed_core::HumanDuration(report.covered),
            cpu_time(report.cpu_seconds)
        )
    } else {
        format!(
            "Over {days:.0} days, dozed held an app for {} of the time, saving {}.",
            dozed_core::HumanDuration(report.covered),
            cpu_time(report.cpu_seconds)
        )
    };

    // Modelled, and said as a range because that is what §11.1 requires of a
    // number nobody can catch being wrong. Suppressed on mains for the reason
    // the report suppresses it: there is no charge being spent to be a fraction
    // of.
    if !report.on_ac
        && let Some(wh) = battery_wh
        && let Some((low, high)) = report.charge_fraction(wh)
        && high >= 0.5
    {
        body.push_str(&format!("\n\nRoughly {low:.0}-{high:.0}% of a charge, modelled."));
    }

    // The measured figure, when a trial has produced one. It is the only number
    // here that is not a model, so it is said separately rather than blended
    // into the sentence above.
    match &report.measured {
        Some(v) if v.saving_is_real() => {
            if let Some(w) = v.saving_w {
                body.push_str(&format!("\n\nMeasured by trial: {w:.2} W saved."));
            }
        }
        Some(_) => body.push_str(
            "\n\nThe trial you ran found no measurable difference, which is also an answer.",
        ),
        None if report.dry_run => body.push_str(
            "\n\n`dozectl report` has the detail, `dozectl trial start` measures it for real, \
             and `dozectl mode live` is how it stops being hypothetical.",
        ),
        None => {}
    }
    body.push_str("\n\nThis is the only notification dozed sends, once a week.");
    (summary, body)
}

/// The CPU time saved, in a unit a person can picture.
///
/// Up to a thousand it stays in seconds, where the number is still readable;
/// past that "86400 CPU-seconds" is a figure nobody converts in their head, so
/// it becomes hours, and days once there is at least one of them.
fn cpu_time(secs: f64) -> String {
    const HOUR: f64 = 3600.0;
    const DAY: f64 = 24.0 * HOUR;
    if secs <= 1000.0 {
        format!("{secs:.0} CPU-seconds")
    } else if secs < DAY {
        format!("{:.1} CPU-hours", secs / HOUR)
    } else {
        format!("{:.1} CPU-days", secs / DAY)
    }
}

/// When the digest was last sent, and when it is next due.
///
/// Persisted rather than kept in memory, because the daemon restarts more often
/// than once a week — a package upgrade, a reboot, a `systemctl --user restart`
/// — and an in-memory timer would mean the digest fires on whichever machine
/// happens to stay up, which is to say never on a laptop.
pub struct Digest {
    path: std::path::PathBuf,
    last: Option<SystemTime>,
}

impl Digest {
    #[must_use]
    pub fn new(path: std::path::PathBuf) -> Self {
        let last = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs));
        Self { path, last }
    }

    /// Whether it is due, given how long this daemon has been running.
    ///
    /// `uptime` is the gate that makes this a digest rather than a greeting: a
    /// fresh install whose state file is missing would otherwise fire one
    /// immediately, reporting on the four minutes since it started.
    #[must_use]
    pub fn due(&self, now: SystemTime, uptime: Duration) -> bool {
        if uptime < Duration::from_secs(3600) {
            return false;
        }
        match self.last {
            Some(last) => now.duration_since(last).is_ok_and(|since| since >= EVERY),
            // Never sent. Wait out one full period from first sight rather than
            // reporting on a partial week — `mark` is called on the first check
            // precisely so this has a starting point.
            None => false,
        }
    }

    /// Record that it has been sent, or seed the clock on first sight.
    pub fn mark(&mut self, now: SystemTime) {
        self.last = Some(now);
        let secs = now.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&self.path, secs.to_string()) {
            tracing::debug!(error = %e, "could not record the digest time");
        }
    }

    /// Whether the clock has ever been started.
    #[must_use]
    pub const fn seeded(&self) -> bool {
        self.last.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_at(dir: &std::path::Path) -> Digest {
        Digest::new(dir.join("digest"))
    }

    #[test]
    fn a_fresh_install_does_not_congratulate_itself_on_four_minutes() {
        // The failure this guards: no state file reads as "overdue" under the
        // obvious implementation, so the first notification a user ever sees
        // would report on the minutes since login.
        let dir = std::env::temp_dir().join(format!("dozed-digest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut d = digest_at(&dir);

        assert!(!d.seeded());
        assert!(!d.due(SystemTime::now(), Duration::from_secs(10)));

        // Seeding starts the clock without sending anything.
        let now = SystemTime::now();
        d.mark(now);
        assert!(!d.due(now + Duration::from_secs(86_400), Duration::from_secs(86_400)));
        assert!(d.due(now + EVERY, Duration::from_secs(EVERY.as_secs())));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_clock_survives_a_restart() {
        // A laptop restarts more often than once a week, so an in-memory timer
        // would mean the digest never fires at all.
        let dir = std::env::temp_dir().join(format!("dozed-digest2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut first = digest_at(&dir);
        let now = SystemTime::now();
        first.mark(now);

        let second = digest_at(&dir);
        assert!(second.seeded(), "a new daemon must find the old clock");
        assert!(!second.due(now + Duration::from_secs(60), Duration::from_secs(7200)));
        assert!(second.due(now + EVERY, Duration::from_secs(7200)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_hour_of_uptime_is_required_whatever_the_clock_says() {
        // Otherwise a daemon restarted eight days after the last digest fires
        // one during login, alongside everything else starting up, which is the
        // least useful moment available.
        let dir = std::env::temp_dir().join(format!("dozed-digest3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut d = digest_at(&dir);
        let now = SystemTime::now();
        d.mark(now);

        let later = now + EVERY + Duration::from_secs(86_400);
        assert!(!d.due(later, Duration::from_secs(30)), "just started");
        assert!(d.due(later, Duration::from_secs(3601)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dry_run_digest_never_claims_to_have_saved_anything() {
        // The counterfactual has to survive into the notification, which is the
        // one place the number is read without any surrounding context.
        let report = Report {
            dry_run: true,
            window: EVERY,
            covered: Duration::from_secs(7200),
            cpu_seconds: 400.0,
            ..Report::default()
        };
        let (summary, body) = digest(&report, Some(50.0));
        assert!(summary.contains("would have saved"), "{summary}");
        assert!(body.contains("would have held"), "{body}");
        assert!(!body.contains("dozed held an app"), "{body}");
    }

    #[test]
    fn large_cpu_savings_are_said_in_hours_or_days() {
        assert_eq!(cpu_time(400.0), "400 CPU-seconds");
        assert_eq!(cpu_time(1000.0), "1000 CPU-seconds");
        assert_eq!(cpu_time(1200.0), "0.3 CPU-hours");
        assert_eq!(cpu_time(9000.0), "2.5 CPU-hours");
        assert_eq!(cpu_time(86_400.0), "1.0 CPU-days");
        assert_eq!(cpu_time(216_000.0), "2.5 CPU-days");

        let report = Report { window: EVERY, cpu_seconds: 9000.0, ..Report::default() };
        let (_, body) = digest(&report, None);
        assert!(body.contains("saving 2.5 CPU-hours"), "{body}");
    }

    #[test]
    fn the_digest_quotes_wall_clock_coverage_not_app_time() {
        // App-time is a sum across apps and legitimately exceeds the window —
        // six apps held for five hours is twenty-five hours of it. That is fine
        // in a table with a legend and a lie in a one-line notification.
        let report = Report {
            dry_run: false,
            window: EVERY,
            covered: Duration::from_secs(3600),
            total_dozed: Duration::from_secs(90_000),
            cpu_seconds: 1200.0,
            ..Report::default()
        };
        let (_, body) = digest(&report, None);
        assert!(body.contains("1h"), "the covered hour: {body}");
        assert!(!body.contains("25h"), "not the app-time sum: {body}");
    }
}
