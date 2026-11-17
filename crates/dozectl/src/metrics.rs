//! `dozectl export`: the current state as Prometheus metrics.
//!
//! §14 asks for a week of dry-run before switching live mode on, and a report read
//! once at the end of that week is an anecdote. The same numbers plotted over the
//! week are where the pattern is — a rule that only ever fires in the evening, an
//! app whose wakeup rate never settles, a saving that is really one bad afternoon.
//!
//! Written for `node_exporter`'s textfile collector, which reads a directory of
//! `.prom` files on its own schedule and therefore needs no daemon here and no
//! port to listen on. That is the whole reason this is a command rather than an
//! HTTP endpoint: a battery tool that opens a socket and serves it has to be woken
//! to do so (§13), and this way the scrape costs one short-lived process every few
//! minutes.

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use dozed_core::ipc::{self, Request, Response};

/// Ask for everything, in one round trip.
///
/// One batch rather than three connections because the daemon answers a packet in
/// one hop and one wakeup — the same reason `ipcserver::Command` carries a `Vec`.
pub async fn export() -> Result<()> {
    let path = ipc::socket_path();
    // Same-UID checked, like every other client connection (`crate::connect`).
    // It matters more here than anywhere: these numbers go into a time series
    // somebody will read a week later, long after the shell that produced them.
    let stream = crate::connect(&path).await?;
    let (read, mut write) = stream.into_split();

    let mut batch = String::new();
    // Not `watching`, however often Prometheus scrapes: each scrape is a fresh
    // process that exits before a faster tick could produce anything, and a
    // scrape interval is not somebody looking at a screen.
    for request in
        [Request::Status, Request::ListApps { watching: false }, Request::Report { since: None }]
    {
        batch.push_str(&ipc::encode(&request)?);
    }
    write.write_all(batch.as_bytes()).await?;

    let mut lines = BufReader::new(read).lines();
    let mut out = String::new();
    let mut seen = 0;
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response: Response =
            serde_json::from_str(&line).with_context(|| format!("unparseable response: {line}"))?;
        out.push_str(&render(&response));
        seen += 1;
        // Three questions, three answers; `Report` is last and there is no
        // end-of-stream marker on this path.
        if seen == 3 {
            break;
        }
    }
    crate::emit(&out);
    Ok(())
}

/// One response as exposition-format text.
#[must_use]
pub fn render(response: &Response) -> String {
    let mut out = String::new();
    match response {
        Response::Status(s) => {
            metric(
                &mut out,
                "dozed_up",
                "gauge",
                "1 when the daemon answered this scrape.",
                &[("", 1.0)],
            );
            // The mode as a label rather than a number, because "1" and "0" for
            // dry-run and live is a mapping every dashboard would have to
            // remember, and a label is self-describing in a legend.
            out.push_str("# HELP dozed_mode The mode in force, as a label.\n");
            out.push_str("# TYPE dozed_mode gauge\n");
            out.push_str(&format!("dozed_mode{{mode=\"{}\"}} 1\n", s.mode.as_str()));
            out.push_str(&format!(
                "# HELP dozed_desktop_state What the human at the keyboard is doing.\n\
                 # TYPE dozed_desktop_state gauge\n\
                 dozed_desktop_state{{state=\"{}\"}} 1\n",
                s.desktop.as_str()
            ));
            simple(
                &mut out,
                "dozed_uptime_seconds",
                "counter",
                "How long this run has been up.",
                s.uptime.as_secs_f64(),
            );
            simple(
                &mut out,
                "dozed_on_ac",
                "gauge",
                "1 while the machine is plugged in.",
                f64::from(u8::from(s.power.on_ac)),
            );
            if let Some(pct) = s.power.percentage {
                simple(
                    &mut out,
                    "dozed_battery_ratio",
                    "gauge",
                    "Charge remaining, 0 to 1.",
                    f64::from(pct) / 100.0,
                );
            }
            simple(
                &mut out,
                "dozed_apps_known",
                "gauge",
                "Apps the daemon can see.",
                s.apps_known as f64,
            );
            simple(
                &mut out,
                "dozed_apps_held",
                "gauge",
                "Apps currently frozen or capped.",
                s.apps_held as f64,
            );
            simple(
                &mut out,
                "dozed_focus_source_healthy",
                "gauge",
                "0 means the daemon cannot see focus and is holding nothing.",
                f64::from(u8::from(s.focus_source_healthy)),
            );
            // The daemon's own cost, which §13 insists is never hidden — and the
            // one series worth an alert: a battery tool costing more than it saves
            // is the failure nobody notices.
            simple(
                &mut out,
                "dozed_self_wakeups_per_second",
                "gauge",
                "The daemon's own wakeup rate. Its cost, not its benefit.",
                f64::from(s.self_wakeups_per_sec),
            );
            if let Some(calls) = s.bus_calls_per_sec {
                simple(
                    &mut out,
                    "dozed_bus_calls_per_second",
                    "gauge",
                    "D-Bus property reads a second by the system watcher.",
                    f64::from(calls),
                );
            }
            if let Some(e) = &s.energy {
                if let Some(w) = e.discharge_w {
                    simple(
                        &mut out,
                        "dozed_discharge_watts",
                        "gauge",
                        "Battery discharge. Absent on mains, where it is unmeasurable.",
                        w,
                    );
                }
                if let Some(w) = e.package_w {
                    simple(
                        &mut out,
                        "dozed_package_watts",
                        "gauge",
                        "CPU package power from RAPL. Absent unless the counter is readable, \
                         which needs a permission the kernel does not grant by default.",
                        w,
                    );
                }
                // One `metric` call for the whole family, not one per domain:
                // the exposition format allows a single HELP and TYPE per name
                // and a collector rejects the entire file over a repeat.
                //
                // Labelled and deliberately *not* added to
                // `dozed_package_watts`. A dashboard that summed these beside
                // the package would double-count the cores, which is the trap
                // `DESIGN.md` §11.1 names; the HELP text says so in words, for
                // whoever reads the exposition rather than the design.
                let domains: Vec<(String, f64)> =
                    e.package_parts.parts().map(|(d, w)| (format!("domain=\"{d}\""), w)).collect();
                if !domains.is_empty() {
                    let rows: Vec<(&str, f64)> =
                        domains.iter().map(|(l, w)| (l.as_str(), *w)).collect();
                    metric(
                        &mut out,
                        "dozed_package_domain_watts",
                        "gauge",
                        "A RAPL domain INSIDE dozed_package_watts, not beside it. Summing these \
                         with the package double-counts the cores.",
                        &rows,
                    );
                }
                if let Some(w) = e.gpu_w {
                    simple(
                        &mut out,
                        "dozed_gpu_watts",
                        "gauge",
                        "GPU power from amdgpu hwmon. NOT addable to dozed_package_watts: on an \
                         integrated GPU it is the same silicon reported twice, and nothing in \
                         sysfs tells that case from a discrete card.",
                        w,
                    );
                }
                // The fitted coefficients, and the one boolean worth alerting
                // on. A package at its firmware limit is not a degraded reading —
                // it is the state in which holding an app frees budget for
                // another rather than saving power, so a dashboard that plots
                // `dozed_app_estimated_watts` needs this series beside it to know
                // what those watts mean (`DESIGN.md` §11.5).
                if let Some(v) = &e.power_model {
                    use dozed_core::power_model::Verdict;
                    simple(
                        &mut out,
                        "dozed_package_power_limited",
                        "gauge",
                        "1 when the CPU package is pinned at a firmware power limit, where                          freeing an app reallocates its budget instead of saving power.",
                        f64::from(u8::from(matches!(v, Verdict::PowerLimited { .. }))),
                    );
                    if let Verdict::Fitted(f) = v {
                        simple(
                            &mut out,
                            "dozed_watts_per_busy_core",
                            "gauge",
                            "Watts per fully-busy core, fitted to this machine's own package                              counter rather than assumed.",
                            f.watts_per_core,
                        );
                        simple(
                            &mut out,
                            "dozed_joules_per_wakeup",
                            "gauge",
                            "What one wakeup costs on this machine, fitted. The coefficient                              the wakeup metric was always missing.",
                            f.joules_per_wakeup,
                        );
                        simple(
                            &mut out,
                            "dozed_package_floor_watts",
                            "gauge",
                            "Fitted package draw with nothing running. Belongs to no app and                              is charged to none.",
                            f.floor_w,
                        );
                        simple(
                            &mut out,
                            "dozed_power_model_r2",
                            "gauge",
                            "How much of the variation in package watts the fit explains.                              Below 0.5 the model is refused and the watts fall back to                              CPU-time share.",
                            f.r2,
                        );
                    }
                }
                if let Some(c) = e.machine_cores_busy {
                    simple(
                        &mut out,
                        "dozed_machine_cores_busy",
                        "gauge",
                        "Cores busy machine-wide from /proc/stat. The denominator per-app watt \
                         estimates are divided by.",
                        c,
                    );
                }
                if let Some(f) = e.deepest_idle {
                    simple(
                        &mut out,
                        "dozed_deepest_idle_ratio",
                        "gauge",
                        "Fraction of CPU-time in the deepest idle state. The physical \
                         consequence of a wakeup.",
                        f,
                    );
                }
                if let Some(w) = e.system_wakeups_per_sec {
                    simple(
                        &mut out,
                        "dozed_system_wakeups_per_second",
                        "gauge",
                        "Machine-wide context switches. What the per-app series should sum to.",
                        w,
                    );
                }
            }
            if let Some(t) = &s.trial {
                simple(
                    &mut out,
                    "dozed_trial_blocks_done",
                    "gauge",
                    "Blocks of the self-trial completed.",
                    t.blocks_done as f64,
                );
                simple(
                    &mut out,
                    "dozed_trial_blocks_total",
                    "gauge",
                    "Blocks the self-trial will collect.",
                    t.blocks_total as f64,
                );
            }
            // A source that has never reported is a safety net drawn but not
            // connected — the failure that shipped once already, silently, when
            // the audio and dbus-owner rules matched on state nothing set. Worth a
            // series precisely because nothing else says so.
            if !s.exemption_sources.is_empty() {
                out.push_str(
                    "# HELP dozed_exemption_source_reporting 0 means this exemption has never \
                     reported, so the rules relying on it cannot fire.\n\
                     # TYPE dozed_exemption_source_reporting gauge\n",
                );
                for source in &s.exemption_sources {
                    out.push_str(&format!(
                        "dozed_exemption_source_reporting{{source=\"{}\"}} {}\n",
                        escape(&source.name),
                        u8::from(source.reported)
                    ));
                }
            }
        }
        // `machine` is deliberately unused here: the system-wide wakeup rate is
        // already exported from `Status`, and exporting it twice would give two
        // series with the same meaning and different sampling.
        Response::Apps { apps, machine } => {
            out.push_str(
                "# HELP dozed_app_wakeups_per_second Wakeups a second, per app, as sampled.\n\
                 # TYPE dozed_app_wakeups_per_second gauge\n",
            );
            for a in apps {
                out.push_str(&format!(
                    "dozed_app_wakeups_per_second{{app=\"{}\",state=\"{}\"}} {}\n",
                    escape(&a.label),
                    state_of(a.applied),
                    f64::from(a.sample.wakeups_per_sec)
                ));
            }
            out.push_str(
                "# HELP dozed_app_cpu_ratio CPU in use per app, 1.0 being one core.\n\
                 # TYPE dozed_app_cpu_ratio gauge\n",
            );
            for a in apps {
                out.push_str(&format!(
                    "dozed_app_cpu_ratio{{app=\"{}\"}} {}\n",
                    escape(&a.label),
                    f64::from(a.sample.cpu_pct) / 100.0
                ));
            }
            // A measurement, unlike the watts below: the kernel counts engine
            // nanoseconds per DRM client, so nothing here is apportioned. Absent
            // for an app that holds no client rather than exported as zero.
            let gpu: Vec<(String, f64)> = apps
                .iter()
                .filter(|a| a.sampled)
                .filter_map(|a| Some((escape(&a.label), f64::from(a.sample.gpu_pct?))))
                .map(|(app, p)| (format!("app=\"{app}\""), p / 100.0))
                .collect();
            if !gpu.is_empty() {
                let rows: Vec<(&str, f64)> = gpu.iter().map(|(l, p)| (l.as_str(), *p)).collect();
                metric(
                    &mut out,
                    "dozed_app_gpu_ratio",
                    "gauge",
                    "GPU engine time per app, 1.0 being one engine fully busy. Measured per DRM \
                     client, not apportioned. Absent for an app that holds no client.",
                    &rows,
                );
            }
            // An ESTIMATE, and the series name says so rather than leaving it to
            // a dashboard's axis label — a scraped figure outlives every caveat
            // printed beside it, and `dozed_app_watts` would be read as a
            // measurement forever. Emitted only where a basis exists and only for
            // rows with both a sample and an honest share: an app with neither is
            // absent, never zero, for the same reason `dozed_discharge_watts` is
            // absent on mains.
            let watts: Vec<(String, f64)> = machine
                .basis
                .map(|b| {
                    apps.iter()
                        .filter(|a| a.sampled)
                        .filter_map(|a| {
                            Some((
                                escape(&a.label),
                                b.watts(a.sample.cpu_pct, a.sample.wakeups_per_sec)?,
                            ))
                        })
                        .map(|(app, w)| (format!("app=\"{app}\""), w))
                        .collect()
                })
                .unwrap_or_default();
            if !watts.is_empty() {
                let rows: Vec<(&str, f64)> = watts.iter().map(|(l, w)| (l.as_str(), *w)).collect();
                metric(
                    &mut out,
                    "dozed_app_estimated_watts",
                    "gauge",
                    "ESTIMATE, not a measurement: an app's share of a measured RAPL total. Linux \
                     has no per-process energy counter.",
                    &rows,
                );
            }
        }
        Response::Report(r) => {
            // Counters, and they reset when the daemon restarts. That is exactly
            // what a Prometheus counter is allowed to do, and `rate()` handles it —
            // whereas a gauge would average a restart into nonsense.
            out.push_str(
                "# HELP dozed_avoided_wakeups_total Wakeups avoided, or that would have been.\n\
                 # TYPE dozed_avoided_wakeups_total counter\n",
            );
            for a in &r.apps {
                out.push_str(&format!(
                    "dozed_avoided_wakeups_total{{app=\"{}\"}} {}\n",
                    escape(a.app.label()),
                    a.wakeups.max(0.0)
                ));
            }
            out.push_str(
                "# HELP dozed_avoided_cpu_seconds_total CPU-seconds avoided, or that would \
                 have been.\n# TYPE dozed_avoided_cpu_seconds_total counter\n",
            );
            for a in &r.apps {
                out.push_str(&format!(
                    "dozed_avoided_cpu_seconds_total{{app=\"{}\"}} {}\n",
                    escape(a.app.label()),
                    a.cpu_seconds.max(0.0)
                ));
            }
            out.push_str(
                "# HELP dozed_dozed_seconds_total Time held, per app.\n\
                 # TYPE dozed_dozed_seconds_total counter\n",
            );
            for a in &r.apps {
                out.push_str(&format!(
                    "dozed_dozed_seconds_total{{app=\"{}\"}} {}\n",
                    escape(a.app.label()),
                    a.dozed.as_secs_f64()
                ));
            }
            // The series most worth alerting on: a hold that is not holding makes
            // every other number on this page meaningless.
            out.push_str(
                "# HELP dozed_leaking 1 when an app was measurably still running while held.\n\
                 # TYPE dozed_leaking gauge\n",
            );
            for a in &r.apps {
                out.push_str(&format!(
                    "dozed_leaking{{app=\"{}\"}} {}\n",
                    escape(a.app.label()),
                    u8::from(a.leaking())
                ));
            }
            if let Some(m) = &r.measured {
                simple(
                    &mut out,
                    "dozed_measured_saving_watts",
                    "gauge",
                    "Watts saved, measured by the self-trial. Only when the trial found a \
                     difference it could distinguish from zero.",
                    if m.saving_is_real() { m.saving_w.unwrap_or(0.0) } else { 0.0 },
                );
                if let Some(p) = m.saving_p {
                    simple(
                        &mut out,
                        "dozed_measured_saving_p_value",
                        "gauge",
                        "The trial's p-value. Above 0.05 the saving above is reported as zero.",
                        p,
                    );
                }
            }
        }
        // Nothing else is asked for on this path.
        _ => {}
    }
    out
}

fn simple(out: &mut String, name: &str, kind: &str, help: &str, value: f64) {
    metric(out, name, kind, help, &[("", value)]);
}

fn metric(out: &mut String, name: &str, kind: &str, help: &str, values: &[(&str, f64)]) {
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
    for (labels, value) in values {
        if labels.is_empty() {
            out.push_str(&format!("{name} {value}\n"));
        } else {
            out.push_str(&format!("{name}{{{labels}}} {value}\n"));
        }
    }
}

/// Escape a label value per the exposition format.
///
/// App labels are derived from desktop files, executable paths and `comm`, so a
/// backslash or a quote in one is unlikely but entirely possible — and an
/// unescaped one does not produce a wrong number, it produces a file the collector
/// refuses in its entirety.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

fn state_of(action: dozed_core::Action) -> &'static str {
    match action {
        dozed_core::Action::Restore => "running",
        dozed_core::Action::Throttle { .. } => "throttled",
        dozed_core::Action::Freeze { .. } => "dozing",
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use dozed_core::coverage::MachineTotals;
    use dozed_core::ipc::{AppSummary, EnergyStatus, StatusReport, TrialSummary};
    use dozed_core::{Action, AppKey, Mode, MonoTime, PowerState, ProcSample, Visibility};

    fn status() -> StatusReport {
        StatusReport {
            mode: Mode::DryRun,
            configured_mode: None,
            mode_driven_by_trial: false,
            trial_restores: None,
            uptime: Duration::from_secs(3600),
            now: MonoTime::ZERO,
            power: PowerState { on_ac: false, percentage: Some(77.0) },
            desktop: dozed_core::DesktopState::IdlePresent,
            focus_source_healthy: true,
            session_bus_healthy: true,
            compositor: Some("kwin".into()),
            focus_source: None,
            apps_known: 9,
            apps_held: 5,
            snooze_remaining: None,
            self_wakeups_per_sec: 6.8,
            bus_calls_per_sec: Some(0.2),
            wakeup_counter: Some("schedstat".into()),
            frozen_file_error: None,
            config_path: None,
            trace: None,
            exemption_sources: vec![dozed_core::ipc::ExemptionSource {
                name: "audio".into(),
                reported: true,
                pids: 1,
                unavailable: None,
                degraded: None,
            }],
            energy: Some(EnergyStatus {
                discharge_w: Some(6.4),
                package_w: Some(3.1),
                package_denied: false,
                deepest_idle: Some(0.82),
                system_wakeups_per_sec: Some(1400.0),
                cpus: 16,
                package_parts: dozed_core::energy::PackageBreakdown::default(),
                gpu_w: None,
                gpu_label: None,
                machine_cores_busy: None,
                power_model: None,
                gpu_busy: None,
            }),
            trial: Some(TrialSummary {
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

    /// Everything a Prometheus parser insists on: every sample's name is declared
    /// with a HELP and a TYPE, values parse as numbers, and nothing is duplicated.
    fn check_exposition(text: &str) {
        let mut declared: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        let mut typed: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        let mut samples: Vec<&str> = Vec::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# HELP ") {
                let name = rest.split_whitespace().next().expect("HELP names a metric");
                assert!(declared.insert(name), "duplicate HELP for {name}");
            } else if let Some(rest) = line.strip_prefix("# TYPE ") {
                let mut parts = rest.split_whitespace();
                let name = parts.next().expect("TYPE names a metric");
                let kind = parts.next().expect("TYPE states a kind");
                assert!(
                    ["gauge", "counter", "histogram", "summary", "untyped"].contains(&kind),
                    "{name}: unknown type {kind}"
                );
                assert!(typed.insert(name), "duplicate TYPE for {name}");
            } else if !line.is_empty() {
                let (series, value) = line.rsplit_once(' ').expect("a sample is `series value`");
                value.parse::<f64>().unwrap_or_else(|e| panic!("{line}: {e}"));
                let name = series.split('{').next().unwrap();
                assert!(declared.contains(name), "{name} has no HELP");
                assert!(typed.contains(name), "{name} has no TYPE");
                samples.push(series);
            }
        }
        assert!(!samples.is_empty(), "no samples at all");
        // A repeated series with different values is the one error that makes a
        // collector reject the whole file.
        let unique: std::collections::BTreeSet<&&str> = samples.iter().collect();
        assert_eq!(unique.len(), samples.len(), "a series appears twice: {samples:?}");
    }

    #[test]
    fn the_status_metrics_parse_as_exposition_format() {
        let text = render(&Response::Status(Box::new(status())));
        check_exposition(&text);
        assert!(text.contains("dozed_up 1\n"), "{text}");
        assert!(text.contains("dozed_mode{mode=\"dry-run\"} 1"), "{text}");
        assert!(text.contains("dozed_self_wakeups_per_second 6.8"), "{text}");
        assert!(text.contains("dozed_discharge_watts 6.4"), "{text}");
        assert!(text.contains("dozed_battery_ratio 0.77"), "{text}");
    }

    #[test]
    fn a_machine_with_no_battery_exports_no_battery_series() {
        // An absent reading must be absent, not zero: `dozed_discharge_watts 0`
        // would graph as a machine that uses no power, which is the same mistake
        // the report itself refuses to make on mains.
        let mut s = status();
        s.energy = Some(EnergyStatus {
            discharge_w: None,
            package_w: None,
            package_denied: false,
            deepest_idle: None,
            system_wakeups_per_sec: None,
            cpus: 0,
            package_parts: dozed_core::energy::PackageBreakdown::default(),
            gpu_w: None,
            gpu_label: None,
            machine_cores_busy: None,
            power_model: None,
            gpu_busy: None,
        });
        s.power = PowerState { on_ac: true, percentage: None };
        let text = render(&Response::Status(Box::new(s)));
        check_exposition(&text);
        assert!(!text.contains("dozed_discharge_watts"), "{text}");
        // Same argument, and the commoner case: RAPL is unreadable on a stock
        // kernel, so this series is absent on most machines rather than zero.
        assert!(!text.contains("dozed_package_watts"), "{text}");
        assert!(!text.contains("dozed_battery_ratio"), "{text}");
        assert!(text.contains("dozed_on_ac 1"), "{text}");
    }

    #[test]
    fn the_report_exports_a_counter_per_app_and_flags_a_leak() {
        let saving = |name: &str, leak: f64| dozed_core::journal::AppSaving {
            app: AppKey::desktop(name),
            dozed: Duration::from_secs(3600),
            throttled: Duration::ZERO,
            cpu_seconds: 61.0,
            wakeups: 240_000.0,
            episodes: 3,
            leaked_cpu_seconds: leak,
            leaked_wakeups: 0.0,
        };
        let report = dozed_core::Report {
            power_fit: None,
            window: Duration::from_secs(7200),
            apps: vec![saving("slack", 0.0), saving("brave", 90.0)],
            total_dozed: Duration::from_secs(7200),
            covered: Duration::from_secs(3600),
            cpu_seconds: 122.0,
            wakeups: 480_000.0,
            dry_run: true,
            battery_wh: Some(60.0),
            on_ac: false,
            measured: None,
        };
        let text = render(&Response::Report(Box::new(report)));
        check_exposition(&text);
        assert!(text.contains("dozed_avoided_wakeups_total{app=\"slack\"} 240000"), "{text}");
        assert!(text.contains("dozed_leaking{app=\"brave\"} 1"), "{text}");
        assert!(text.contains("dozed_leaking{app=\"slack\"} 0"), "{text}");
    }

    #[test]
    fn an_app_name_with_a_quote_in_it_cannot_break_the_whole_file() {
        // App labels come from desktop files, executable paths and `comm`. A quote
        // in one is unlikely and entirely possible, and an unescaped one does not
        // produce a wrong number — it produces a file the collector discards.
        let apps = vec![AppSummary {
            key: AppKey::comm("odd"),
            label: r#"we"ird\name"#.into(),
            visibility: Visibility::Hidden,
            applied: Action::Freeze { reclaim: false },
            mode: Mode::Live,
            sample: ProcSample { cpu_pct: 1.0, wakeups_per_sec: 2.0, gpu_pct: None },
            sampled: true,
            pids: 1,
            exempt: Vec::new(),
            held_off: None,
            never_actionable: false,
            audio_streams: Vec::new(),
            top_processes: Vec::new(),
            pending: None,
        }];
        let text = render(&Response::Apps { apps, machine: MachineTotals::default() });
        check_exposition(&text);
        assert!(text.contains(r#"app="we\"ird\\name""#), "{text}");
    }

    #[test]
    fn the_measured_saving_is_only_exported_once_it_is_real() {
        // A trial that found nothing must export zero rather than its point
        // estimate, or a dashboard shows a saving the arithmetic refused to claim.
        let mut trial =
            dozed_core::trial::Trial::new(dozed_core::trial::TrialPlan::default(), Mode::DryRun);
        trial.stop();
        let mut verdict = trial.verdict();
        verdict.saving_w = Some(1.5);
        verdict.saving_p = Some(0.4); // nowhere near significant
        verdict.saving_interval_w = Some((-0.5, 3.5));
        verdict.discharge_pairs = 4;

        let report =
            dozed_core::Report { measured: Some(verdict), ..dozed_core::Report::default() };
        let text = render(&Response::Report(Box::new(report)));
        check_exposition(&text);
        assert!(text.contains("dozed_measured_saving_watts 0"), "{text}");
        assert!(text.contains("dozed_measured_saving_p_value 0.4"), "{text}");
    }

    #[test]
    fn every_series_name_is_prefixed_so_a_shared_dashboard_stays_sane() {
        let mut text = render(&Response::Status(Box::new(status())));
        text.push_str(&render(&Response::Report(Box::default())));
        for line in text.lines() {
            let name = line
                .strip_prefix("# HELP ")
                .or_else(|| line.strip_prefix("# TYPE "))
                .map(|r| r.split_whitespace().next().unwrap())
                .unwrap_or_else(|| line.split(['{', ' ']).next().unwrap());
            assert!(name.starts_with("dozed_"), "unprefixed series {name:?}");
        }
    }

    /// A package at its firmware limit inverts what every per-app watt figure
    /// means, so a dashboard plotting `dozed_app_estimated_watts` needs this
    /// series beside it — and it has to be a number, not a missing metric,
    /// because "absent" and "false" graph identically in an alert rule.
    #[test]
    fn a_power_limited_package_is_exported_as_a_number_a_rule_can_fire_on() {
        let mut s = status();
        let energy = s.energy.clone().unwrap();
        s.energy = Some(EnergyStatus {
            power_model: Some(dozed_core::power_model::Verdict::PowerLimited {
                watts: 24.2,
                cores_range: 9.5,
            }),
            ..energy.clone()
        });
        let text = render(&Response::Status(Box::new(s)));
        check_exposition(&text);
        assert!(text.contains("dozed_package_power_limited 1"), "{text}");
        // And no coefficients, because there is no fit to publish.
        assert!(!text.contains("dozed_joules_per_wakeup"), "{text}");

        let mut s = status();
        s.energy = Some(EnergyStatus {
            power_model: Some(dozed_core::power_model::Verdict::Fitted(
                dozed_core::power_model::Fit {
                    floor_w: 6.0,
                    watts_per_core: 2.0,
                    joules_per_wakeup: 3e-5,
                    r2: 0.87,
                    samples: 90,
                    se_watts_per_core: 0.05,
                    se_joules_per_wakeup: 1e-6,
                    cov_core_wakeup: 0.0,
                },
            )),
            ..energy
        });
        let text = render(&Response::Status(Box::new(s)));
        check_exposition(&text);
        assert!(text.contains("dozed_package_power_limited 0"), "{text}");
        assert!(text.contains("dozed_watts_per_busy_core 2"), "{text}");
        assert!(text.contains("dozed_joules_per_wakeup 0.00003"), "{text}");
        assert!(text.contains("dozed_package_floor_watts 6"), "{text}");
        assert!(text.contains("dozed_power_model_r2 0.87"), "{text}");
    }

    /// GPU engine time is a measurement, so it is exported for the apps that have
    /// a client and absent for the rest — never zero, which would claim the app
    /// was asked and answered nothing.
    #[test]
    fn gpu_engine_time_is_exported_only_for_apps_that_have_a_client() {
        let row = |label: &str, gpu_pct| AppSummary {
            key: AppKey::comm(label),
            label: label.into(),
            visibility: Visibility::Hidden,
            applied: Action::Restore,
            mode: Mode::Live,
            sample: ProcSample { cpu_pct: 1.0, wakeups_per_sec: 2.0, gpu_pct },
            sampled: true,
            pids: 1,
            exempt: Vec::new(),
            held_off: None,
            never_actionable: false,
            audio_streams: Vec::new(),
            top_processes: Vec::new(),
            pending: None,
        };
        let with_gpu = row("firefox", Some(7.5));
        let without = row("thunderbird", None);
        let text = render(&Response::Apps {
            apps: vec![with_gpu, without],
            machine: MachineTotals::default(),
        });
        check_exposition(&text);
        assert!(text.contains(r#"dozed_app_gpu_ratio{app="firefox"} 0.075"#), "{text}");
        assert!(!text.contains(r#"dozed_app_gpu_ratio{app="thunderbird"}"#), "{text}");
    }
}
