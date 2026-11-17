//! Loading the config file, and writing a starter one.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use dozed_core::Config;

/// Load the config, falling back to the shipped defaults when there is no file.
///
/// Returns the path actually used, if any, so `dozectl status` can say where the
/// rules came from.
///
/// Anything that parses but cannot do what it looks like it does is logged here
/// rather than at the call sites, so startup, `--check` and a live reload all
/// report it — a reload is precisely when someone has just edited the file.
pub fn load(explicit: Option<&PathBuf>) -> Result<(Config, Option<PathBuf>)> {
    let path = explicit.cloned().unwrap_or_else(crate::paths::config_file);
    if !path.exists() {
        return Ok((Config::default(), None));
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let config = Config::parse(&text).with_context(|| format!("in {}", path.display()))?;
    for warning in config.warnings() {
        tracing::warn!(config = %path.display(), "{warning}");
    }
    Ok((config, Some(path)))
}

/// Write the shipped defaults so the user has something to edit.
///
/// Deliberately spells the built-in rules out rather than leaving the file
/// empty: an absent rule table means *no rules*, and rules that appear from
/// nowhere would be the worst kind of surprise.
///
/// The file itself is the documentation — every section and field explained
/// where it appears, so the knob and the reason for it are never in two
/// different places (`dozed_core::starter`).
pub fn write_default(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, dozed_core::starter::starter_config())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Rewrite an existing config with the current explanatory comments, keeping
/// every value it already has.
///
/// Three safeguards, because this edits a file somebody has been tuning for
/// weeks and the whole point of it is cosmetic:
///
/// * the file must already parse — a broken config is not something to rewrite,
///   it is something to fix;
/// * the annotated text is parsed back and compared before anything is written,
///   so a rendering bug cannot silently change what dozed does;
/// * the original is kept alongside, and an existing backup is never clobbered
///   — the pre-annotation copy is the valuable one, and a second run would
///   otherwise replace it with an already-annotated file.
///
/// Comments the user wrote themselves do not survive: the file is regenerated
/// from the parsed config, and TOML comments are not part of that. Reported by
/// the caller rather than guessed at here.
pub fn annotate_in_place(path: &Path) -> Result<PathBuf> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let config = Config::parse(&text).with_context(|| format!("in {}", path.display()))?;

    let annotated = dozed_core::starter::annotate(&config);
    let round_tripped = Config::parse(&annotated)
        .context("the annotated config did not parse; refusing to write it")?;
    if round_tripped != config {
        anyhow::bail!(
            "annotating {} would have changed what it does; refusing to write it",
            path.display()
        );
    }

    let backup = path.with_file_name(format!(
        "{}.bak-preannotate",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    if !backup.exists() {
        std::fs::copy(path, &backup)
            .with_context(|| format!("backing up to {}", backup.display()))?;
    }
    std::fs::write(path, annotated).with_context(|| format!("writing {}", path.display()))?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_the_shipped_defaults() {
        let (cfg, path) = load(Some(&PathBuf::from("/nonexistent/dozed.toml"))).unwrap();
        assert_eq!(cfg.mode, dozed_core::Mode::DryRun);
        assert!(!cfg.rules.is_empty(), "defaults ship with the safety rules");
        assert!(path.is_none());
    }

    #[test]
    fn a_written_default_reloads_identically() {
        let dir = std::env::temp_dir().join(format!("dozed-cfg-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);

        write_default(&path).unwrap();
        let (cfg, used) = load(Some(&path)).unwrap();
        assert_eq!(used.as_ref(), Some(&path));
        assert_eq!(cfg, Config::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The one property that matters: annotating is cosmetic. A file with live
    /// mode, an edited default and a rule of the user's own must come back
    /// saying exactly the same thing.
    #[test]
    fn annotating_keeps_every_value_and_backs_the_original_up() {
        let dir = std::env::temp_dir().join(format!("dozed-annotate-{}", std::process::id()));
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            "mode = \"live\"\n\
             [defaults]\non_ac = \"enabled\"\n\
             [[rule]]\nname = \"mine\"\npriority = 70\naction = \"freeze\"\n\
             [rule.when]\nvisibility = \"hidden\"\nfor = \"1m\"\n",
        )
        .unwrap();
        let before = load(Some(&path)).unwrap().0;

        let backup = annotate_in_place(&path).unwrap();

        let after = load(Some(&path)).unwrap().0;
        assert_eq!(after, before, "annotating changed what the config does");
        assert_eq!(after.mode, dozed_core::Mode::Live, "live mode survived");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# Every section and field"), "no comments were added:\n{text}");
        assert!(
            text.contains("Your own rule"),
            "a rule dozed did not ship went uncommented:\n{text}"
        );

        // The pre-annotation original is the valuable copy, so a second run
        // must not replace it with an already-annotated one.
        assert_eq!(load(Some(&backup)).unwrap().0, before);
        let again = annotate_in_place(&path).unwrap();
        assert_eq!(again, backup);
        assert!(
            !std::fs::read_to_string(&backup).unwrap().contains("# Every section and field"),
            "the backup was overwritten with the annotated file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file that does not parse is something to fix, not to rewrite — and
    /// rewriting it would mean writing back whatever the parser salvaged.
    #[test]
    fn annotating_refuses_a_config_that_does_not_parse() {
        let dir = std::env::temp_dir().join(format!("dozed-annotate2-{}", std::process::id()));
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "mode = \"live\"\n[[rule]]\nname = \"x\"\nvisibilty = \"hidden\"\n")
            .unwrap();

        assert!(annotate_in_place(&path).is_err());
        assert!(
            std::fs::read_to_string(&path).unwrap().contains("visibilty"),
            "the unparseable file was modified anyway"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_config_is_an_error_not_a_silent_default() {
        // Falling back to defaults on a typo would run rules the user did not
        // write, which is exactly the surprise we are avoiding elsewhere.
        let dir = std::env::temp_dir().join(format!("dozed-cfg2-{}", std::process::id()));
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            "[[rule]]\nname = \"x\"\nwhen = { visibilty = \"hidden\" }\naction = \"freeze\"\n",
        )
        .unwrap();

        let err = load(Some(&path)).unwrap_err();
        assert!(format!("{err:#}").contains("config.toml"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
