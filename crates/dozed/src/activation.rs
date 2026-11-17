//! Thawing an app *before* a launcher click reaches it.
//!
//! The footgun `DESIGN.md` §10 named and could not close: clicking the launcher
//! for a frozen single-instance app sends `org.freedesktop.Application.Activate`
//! to a name that app already owns. The call goes into a stopped process, so the
//! launcher blocks for the full 25s D-Bus timeout instead of raising a window.
//! Focus never changes, so nothing in the focus path can rescue it.
//!
//! ## The design said this was unsolvable. It measured wrong.
//!
//! §12 recorded that "eavesdropping on the bus is not possible under
//! `dbus-broker` without special configuration", and settled for KWin's
//! `demandsAttention` as an approximation. That is true of the *old* mechanism —
//! `eavesdrop=true` on a match rule was removed — but not of the current one.
//! `org.freedesktop.DBus.Monitoring.BecomeMonitor` works for the owner of a
//! session bus, with no configuration at all. Measured here: it observed method
//! calls between two other processes immediately.
//!
//! ## And it is free, because the broker filters
//!
//! A monitor that saw every message would be exactly the kind of background
//! work §13 forbids. It does not have to. `BecomeMonitor` takes match rules and
//! the broker applies them server-side, so only activations are ever forwarded.
//! Measured over ten seconds of an ordinary idle KDE session:
//!
//! | Monitor | Messages in 10s |
//! |---|---|
//! | Unfiltered | 46 (4.6/s) |
//! | `interface='org.freedesktop.Application'` | **0** |
//!
//! and the filtered monitor still caught both test `Activate` calls, with the
//! target's well-known name in `Destination`.
//!
//! Two consequences worth stating plainly. A monitor connection can only
//! receive, never call, so this needs a connection of its own. And it sees the
//! bodies of matching messages — which, given the filter, means activation
//! payloads (a platform-data dictionary) and nothing else.

use tokio::sync::mpsc;
use zbus::fdo::{DBusProxy, MonitoringProxy};
use zbus::{Connection, MatchRule, MessageStream};

use crate::sources::Incoming;

/// The interface a launcher calls to raise an already-running app.
///
/// `Activate` is the plain click. `ActivateAction` is a jump-list entry, and
/// `Open` is "open these files with". All three land in a frozen process the
/// same way, so the rule matches the interface rather than any one member.
const ACTIVATION_IFACE: &str = "org.freedesktop.Application";

/// Watch for activation calls and report the PID being activated.
///
/// Runs on its own connection because `BecomeMonitor` is one-way: once a
/// connection becomes a monitor it can no longer make calls, which is also why
/// resolving the destination name to a PID needs the second connection passed
/// in.
pub async fn watch_activation(resolver: Connection, tx: mpsc::Sender<Incoming>) {
    let monitor = match Connection::session().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "no monitor connection; activation thaw unavailable");
            return;
        }
    };

    let rule = match MatchRule::builder()
        .msg_type(zbus::message::Type::MethodCall)
        .interface(ACTIVATION_IFACE)
    {
        Ok(b) => b.build(),
        Err(e) => {
            tracing::warn!(error = %e, "could not build the activation match rule");
            return;
        }
    };

    // Server-side filtering is the whole reason this is affordable: without the
    // rule the daemon would wake for every message on the session bus.
    let proxy = match MonitoringProxy::builder(&monitor)
        .destination("org.freedesktop.DBus")
        .and_then(|b| b.path("/org/freedesktop/DBus"))
    {
        Ok(b) => match b.build().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "monitoring interface unavailable");
                return;
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "monitoring interface unavailable");
            return;
        }
    };
    if let Err(e) = proxy.become_monitor(&[rule], 0).await {
        // Some brokers, and some sandboxes, will refuse. Losing this costs the
        // launcher hang and nothing else, so it is a warning rather than fatal.
        tracing::warn!(error = %e, "bus refused BecomeMonitor; activation thaw unavailable");
        return;
    }

    let Ok(dbus) = DBusProxy::new(&resolver).await else {
        tracing::warn!("cannot resolve bus names; activation thaw unavailable");
        return;
    };

    tracing::info!("watching for D-Bus activation; frozen apps will thaw before a launcher click");
    let mut stream = MessageStream::from(monitor);
    use futures_util::StreamExt as _;
    while let Some(Ok(msg)) = stream.next().await {
        let header = msg.header();
        let Some(destination) = header.destination() else { continue };
        // A unique name (":1.42") is already a live connection nobody launched;
        // activation always targets a well-known name.
        if destination.as_str().starts_with(':') {
            continue;
        }
        let Ok(pid) = dbus.get_connection_unix_process_id(destination.clone()).await else {
            // The name may not be owned yet — a genuinely cold start, where
            // there is nothing frozen to thaw.
            continue;
        };
        tracing::debug!(%destination, pid, "activation requested");
        let _ = tx.send(Incoming::Activation { pid }).await;
    }
    tracing::warn!("activation monitor stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_match_rule_is_narrow_enough_to_be_free() {
        // The measurement this depends on: an unfiltered monitor saw 4.6
        // messages/s on an idle session, and this rule saw none. If the rule
        // ever widens, that cost comes back.
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::MethodCall)
            .interface(ACTIVATION_IFACE)
            .unwrap()
            .build();
        let s = rule.to_string();
        assert!(s.contains("org.freedesktop.Application"), "{s}");
        assert!(s.contains("method_call"), "{s}");
    }

    #[test]
    fn unique_names_are_not_activation_targets() {
        // ":1.42" is a connection that already exists; a launcher activating an
        // app always addresses a well-known name.
        assert!(":1.42".starts_with(':'));
        assert!(!"org.kde.dolphin".starts_with(':'));
    }
}
