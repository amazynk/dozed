//! Application identity.
//!
//! A PID is meaningless across a restart, so every rule is keyed on an
//! [`AppKey`]: the best stable identifier we could find for an app, plus a note
//! of *how* we found it. Keeping the method around is what lets `dozectl why`
//! say "matched com.slack.Slack by flatpak id" instead of leaving the user to
//! guess why their rule did or did not fire.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// How an [`AppKey`] was derived, in descending order of trustworthiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IdentityKind {
    /// Flatpak application id, e.g. `com.slack.Slack`.
    Flatpak,
    /// Snap package name.
    Snap,
    /// Desktop entry id without the `.desktop` suffix, e.g. `org.mozilla.firefox`.
    ///
    /// This is the rung that keeps Chrome PWAs and separate Firefox profiles
    /// apart, which users expect even though they share one executable.
    DesktopId,
    /// Wayland `app_id` as reported by the compositor helper.
    AppId,
    /// Resolved `/proc/<pid>/exe`. Lumps together every Electron app that
    /// shares a launcher, so it is a last resort.
    ExePath,
    /// `/proc/<pid>/comm`, when even `exe` is unreadable.
    Comm,
}

impl IdentityKind {
    /// Whether this identity was *declared* by packaging rather than inferred
    /// from the process in front of us.
    ///
    /// The three declared rungs are deliberate statements about what an
    /// application is, so two different ones are two different apps even when
    /// they share an executable and a process tree — that is exactly what keeps
    /// Chrome PWAs and separate Firefox profiles apart. The inferred rungs carry
    /// no such promise: `app_id` is whatever the window happened to set, and
    /// `exe`/`comm` are last resorts. Only an inferred identity may be folded
    /// into another app's.
    #[must_use]
    pub const fn is_declared(self) -> bool {
        matches!(self, Self::Flatpak | Self::Snap | Self::DesktopId)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Flatpak => "flatpak",
            Self::Snap => "snap",
            Self::DesktopId => "desktop",
            Self::AppId => "app-id",
            Self::ExePath => "exe",
            Self::Comm => "comm",
        }
    }

    /// Resolution order. Lower is preferred.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Flatpak => 0,
            Self::Snap => 1,
            Self::DesktopId => 2,
            Self::AppId => 3,
            Self::ExePath => 4,
            Self::Comm => 5,
        }
    }
}

impl fmt::Display for IdentityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for IdentityKind {
    type Err = IdentityParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "flatpak" => Self::Flatpak,
            "snap" => Self::Snap,
            "desktop" => Self::DesktopId,
            "app-id" => Self::AppId,
            "exe" => Self::ExePath,
            "comm" => Self::Comm,
            other => return Err(IdentityParseError::UnknownKind(other.to_owned())),
        })
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdentityParseError {
    #[error("unknown identity kind `{0}`")]
    UnknownKind(String),
    #[error("app key `{0}` is empty")]
    Empty(String),
}

/// A stable handle for an application.
///
/// Renders as `kind:value` (`flatpak:com.slack.Slack`, `exe:/usr/bin/foo`) so
/// it survives a round trip through a JSON map key or a log line.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AppKey {
    pub kind: IdentityKind,
    pub value: String,
}

impl AppKey {
    pub fn new(kind: IdentityKind, value: impl Into<String>) -> Self {
        Self { kind, value: value.into() }
    }

    pub fn flatpak(v: impl Into<String>) -> Self {
        Self::new(IdentityKind::Flatpak, v)
    }

    pub fn desktop(v: impl Into<String>) -> Self {
        Self::new(IdentityKind::DesktopId, v)
    }

    pub fn exe(v: impl Into<String>) -> Self {
        Self::new(IdentityKind::ExePath, v)
    }

    pub fn comm(v: impl Into<String>) -> Self {
        Self::new(IdentityKind::Comm, v)
    }

    /// Short label for UI: the last path segment for executables, the whole
    /// value otherwise.
    #[must_use]
    pub fn label(&self) -> &str {
        if self.kind == IdentityKind::ExePath {
            self.value.rsplit('/').next().unwrap_or(&self.value)
        } else {
            &self.value
        }
    }

    /// Match a pattern written in a config file.
    ///
    /// A qualified pattern (`flatpak:com.slack.Slack`) must match exactly. A
    /// bare pattern (`slack`) matches the value or the short label of any kind,
    /// case-insensitively, because making users memorise which rung of the
    /// resolver chain matched their app would be user-hostile.
    #[must_use]
    pub fn matches_pattern(&self, pattern: &str) -> bool {
        if let Some((kind, value)) = pattern.split_once(':')
            && let Ok(kind) = kind.parse::<IdentityKind>()
        {
            return self.kind == kind && self.value == value;
        }
        self.value.eq_ignore_ascii_case(pattern) || self.label().eq_ignore_ascii_case(pattern)
    }
}

impl fmt::Display for AppKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind, self.value)
    }
}

impl FromStr for AppKey {
    type Err = IdentityParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (kind, value) =
            s.split_once(':').ok_or_else(|| IdentityParseError::UnknownKind(s.to_owned()))?;
        if value.is_empty() {
            return Err(IdentityParseError::Empty(s.to_owned()));
        }
        Ok(Self { kind: kind.parse()?, value: value.to_owned() })
    }
}

impl Serialize for AppKey {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for AppKey {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Every identifier we managed to observe for an app.
///
/// The winning one becomes the [`AppKey`]; the rest are kept so the UI can
/// explain the choice and so a user can write a more specific rule.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityEvidence {
    pub flatpak_id: Option<String>,
    pub snap_name: Option<String>,
    pub desktop_id: Option<String>,
    pub app_id: Option<String>,
    pub exe_path: Option<String>,
    pub comm: Option<String>,
    pub cgroup_scope: Option<String>,
}

impl IdentityEvidence {
    /// Resolve the chain described in `DESIGN.md` §4: first hit wins.
    #[must_use]
    pub fn resolve(&self) -> Option<AppKey> {
        let candidates: [(IdentityKind, Option<&String>); 6] = [
            (IdentityKind::Flatpak, self.flatpak_id.as_ref()),
            (IdentityKind::Snap, self.snap_name.as_ref()),
            (IdentityKind::DesktopId, self.desktop_id.as_ref()),
            (IdentityKind::AppId, self.app_id.as_ref()),
            (IdentityKind::ExePath, self.exe_path.as_ref()),
            (IdentityKind::Comm, self.comm.as_ref()),
        ];
        candidates
            .into_iter()
            .find_map(|(kind, v)| v.filter(|s| !s.is_empty()).map(|s| AppKey::new(kind, s)))
    }

    /// Human-readable summary for `dozectl why`.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        for (name, v) in [
            ("flatpak", &self.flatpak_id),
            ("snap", &self.snap_name),
            ("desktop", &self.desktop_id),
            ("app_id", &self.app_id),
            ("exe", &self.exe_path),
            ("comm", &self.comm),
        ] {
            if let Some(v) = v {
                parts.push(format!("{name}={v}"));
            }
        }
        parts.join(", ")
    }
}

/// Coarse behavioural class, used by rules to separate "misbehaving" from
/// "doing exactly what I asked".
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum AppClass {
    #[default]
    Normal,
    /// Session infrastructure. Never actionable, not overridable.
    System,
    /// A build, backup, download, VM or container: high CPU in the background
    /// is the point, not a symptom.
    LongRunningJob,
}

impl AppClass {
    /// The spelling used in config files, so an explanation can be pasted
    /// straight into a rule.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::System => "system",
            Self::LongRunningJob => "long-running-job",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_packaged_identities_count_as_declared() {
        // What may be folded into another app and what may not: a declared
        // identity is a statement about what the app *is*, so two of them are two
        // apps even over one process tree.
        for kind in [IdentityKind::Flatpak, IdentityKind::Snap, IdentityKind::DesktopId] {
            assert!(kind.is_declared(), "{kind:?}");
        }
        for kind in [IdentityKind::AppId, IdentityKind::ExePath, IdentityKind::Comm] {
            assert!(!kind.is_declared(), "{kind:?}");
        }
        // And the rungs stay in trustworthiness order, which the fold's tie-break
        // relies on.
        assert!(IdentityKind::Flatpak < IdentityKind::DesktopId);
        assert!(IdentityKind::DesktopId < IdentityKind::AppId);
        assert!(IdentityKind::AppId < IdentityKind::ExePath);
        assert!(IdentityKind::ExePath < IdentityKind::Comm);
    }

    #[test]
    fn key_roundtrips() {
        let k = AppKey::flatpak("com.slack.Slack");
        assert_eq!(k.to_string(), "flatpak:com.slack.Slack");
        assert_eq!(k.to_string().parse::<AppKey>().unwrap(), k);
    }

    #[test]
    fn qualified_pattern_is_exact() {
        let k = AppKey::flatpak("com.slack.Slack");
        assert!(k.matches_pattern("flatpak:com.slack.Slack"));
        assert!(!k.matches_pattern("desktop:com.slack.Slack"));
    }

    #[test]
    fn bare_pattern_matches_any_kind() {
        assert!(AppKey::flatpak("com.slack.Slack").matches_pattern("com.slack.Slack"));
        assert!(AppKey::exe("/usr/bin/slack").matches_pattern("slack"));
        assert!(AppKey::desktop("Slack").matches_pattern("slack"));
    }

    #[test]
    fn resolver_prefers_flatpak_over_exe() {
        let ev = IdentityEvidence {
            flatpak_id: Some("com.slack.Slack".into()),
            exe_path: Some("/app/bin/slack".into()),
            ..Default::default()
        };
        assert_eq!(ev.resolve(), Some(AppKey::flatpak("com.slack.Slack")));
    }

    #[test]
    fn resolver_falls_through_empty_values() {
        let ev = IdentityEvidence {
            flatpak_id: Some(String::new()),
            desktop_id: Some("firefox".into()),
            ..Default::default()
        };
        assert_eq!(ev.resolve(), Some(AppKey::desktop("firefox")));
    }

    #[test]
    fn resolver_gives_up_cleanly() {
        assert_eq!(IdentityEvidence::default().resolve(), None);
    }
}
