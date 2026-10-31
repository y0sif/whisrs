//! A compositor keybinding for `whisrs toggle`: which compositor runs, whether
//! its config binds whisrs already, and appending a binding to it. Hyprland and
//! Sway are written to; any other compositor is named, for the user to bind.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The compositor or desktop the session runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compositor {
    Hyprland,
    Sway,
    Gnome,
    /// Another one, as `XDG_CURRENT_DESKTOP` names it (lower case): `kde`,
    /// `i3`, `niri`.
    Other(String),
}

impl Compositor {
    pub fn name(&self) -> &str {
        match self {
            Compositor::Hyprland => "Hyprland",
            Compositor::Sway => "Sway",
            Compositor::Gnome => "GNOME",
            Compositor::Other(name) => name,
        }
    }
}

/// Which compositor runs, from the process environment.
pub fn detect() -> Option<Compositor> {
    detect_from(|key| std::env::var(key).ok())
}

/// [`detect`] from `env`.
pub fn detect_from(env: impl Fn(&str) -> Option<String>) -> Option<Compositor> {
    // HYPRLAND_INSTANCE_SIGNATURE first: the most specific.
    if env("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return Some(Compositor::Hyprland);
    }
    if env("SWAYSOCK").is_some() {
        return Some(Compositor::Sway);
    }
    let lower = env("XDG_CURRENT_DESKTOP")?.to_lowercase();
    Some(if lower.contains("hyprland") {
        Compositor::Hyprland
    } else if lower.contains("sway") {
        Compositor::Sway
    } else if lower.contains("gnome") {
        Compositor::Gnome
    } else if lower.contains("kde") || lower.contains("plasma") {
        Compositor::Other("kde".to_string())
    } else if lower.contains("i3") {
        Compositor::Other("i3".to_string())
    } else {
        Compositor::Other(lower)
    })
}

/// Whether the compositor's config binds `whisrs toggle`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    /// The file at the path has a `whisrs toggle` binding.
    Bound(PathBuf),
    /// The file at the path has none: [`append`] adds one.
    Unbound(PathBuf),
    /// There is no config file at the path.
    NoFile(PathBuf),
}

/// The config file whisrs writes a binding to, under `config_dir`
/// (`~/.config`); `None` for a compositor it does not write to.
pub fn config_file(compositor: &Compositor, config_dir: &Path) -> Option<PathBuf> {
    match compositor {
        Compositor::Hyprland => Some(config_dir.join("hypr/hyprland.conf")),
        Compositor::Sway => Some(config_dir.join("sway/config")),
        _ => None,
    }
}

/// Whether `compositor`'s config under `config_dir` binds whisrs; `None` for
/// a compositor whisrs does not write to.
pub fn status(compositor: &Compositor, config_dir: &Path) -> Option<Binding> {
    let path = config_file(compositor, config_dir)?;
    Some(match std::fs::read_to_string(&path) {
        Ok(contents) if contents.contains("whisrs toggle") => Binding::Bound(path),
        Ok(_) => Binding::Unbound(path),
        // Unreadable counts as missing: there is nothing to append to.
        Err(_) => Binding::NoFile(path),
    })
}

/// The keys the binding uses, as people say them.
pub fn keys(compositor: &Compositor) -> &'static str {
    match compositor {
        Compositor::Sway => "Mod+W",
        _ => "Super+W",
    }
}

/// The binding line for `compositor`, running `whisrs`.
pub fn line(compositor: &Compositor, whisrs: &str) -> Option<String> {
    match compositor {
        Compositor::Hyprland => Some(format!("bind = $mainMod, W, exec, {whisrs} toggle")),
        Compositor::Sway => Some(format!("bindsym $mod+w exec {whisrs} toggle")),
        _ => None,
    }
}

/// Append the binding for `compositor`, running `whisrs`, to `path`; the
/// file. A file that binds `whisrs toggle` already is left as it is, so a
/// second press, or a binding added meanwhile, writes nothing. Hyprland
/// reads it on `hyprctl reload`, Sway on its own reload: neither picks it
/// up by itself.
pub fn append(compositor: &Compositor, path: &Path, whisrs: &str) -> Result<PathBuf> {
    let line = line(compositor, whisrs).with_context(|| {
        format!(
            "whisrs does not write {}'s config: bind whisrs toggle there yourself",
            compositor.name()
        )
    })?;
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    if contents.contains("whisrs toggle") {
        return Ok(path.to_path_buf());
    }
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.write_all(format!("\n# whisrs: voice-to-text dictation\n{line}\n").as_bytes())
        .with_context(|| format!("failed to write to {}", path.display()))?;
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(pairs: &[(&str, &str)]) -> Option<Compositor> {
        detect_from(|key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        })
    }

    #[test]
    fn the_compositor_comes_from_the_session() {
        assert_eq!(
            detect(&[("HYPRLAND_INSTANCE_SIGNATURE", "x"), ("SWAYSOCK", "y")]),
            Some(Compositor::Hyprland)
        );
        assert_eq!(detect(&[("SWAYSOCK", "/run/sway")]), Some(Compositor::Sway));
        assert_eq!(
            detect(&[("XDG_CURRENT_DESKTOP", "ubuntu:GNOME")]),
            Some(Compositor::Gnome)
        );
        assert_eq!(
            detect(&[("XDG_CURRENT_DESKTOP", "KDE")]),
            Some(Compositor::Other("kde".into()))
        );
        assert_eq!(
            detect(&[("XDG_CURRENT_DESKTOP", "niri")]),
            Some(Compositor::Other("niri".into()))
        );
        assert_eq!(detect(&[]), None);
    }

    #[test]
    fn a_binding_is_found_appended_once_and_named() {
        let dir = tempfile::tempdir().unwrap();
        let hypr = Compositor::Hyprland;
        let conf = dir.path().join("hypr/hyprland.conf");
        assert_eq!(
            status(&hypr, dir.path()),
            Some(Binding::NoFile(conf.clone()))
        );
        std::fs::create_dir_all(conf.parent().unwrap()).unwrap();
        std::fs::write(&conf, "$mainMod = SUPER\n").unwrap();
        assert_eq!(
            status(&hypr, dir.path()),
            Some(Binding::Unbound(conf.clone()))
        );
        assert_eq!(append(&hypr, &conf, "/usr/bin/whisrs").unwrap(), conf);
        let text = std::fs::read_to_string(&conf).unwrap();
        assert!(text.starts_with("$mainMod = SUPER\n"));
        assert!(text.ends_with("bind = $mainMod, W, exec, /usr/bin/whisrs toggle\n"));
        assert!(!text.contains(['\u{2013}', '\u{2014}']));
        assert_eq!(
            status(&hypr, dir.path()),
            Some(Binding::Bound(conf.clone()))
        );
        // Bound already: a second append writes nothing.
        append(&hypr, &conf, "/usr/bin/whisrs").unwrap();
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), text);

        let sway = dir.path().join("sway/config");
        std::fs::create_dir_all(sway.parent().unwrap()).unwrap();
        std::fs::write(&sway, "").unwrap();
        append(&Compositor::Sway, &sway, "whisrs").unwrap();
        assert!(std::fs::read_to_string(&sway)
            .unwrap()
            .contains("bindsym $mod+w exec whisrs toggle"));

        // Nothing to write for another compositor, and no file is made.
        let kde = Compositor::Other("kde".into());
        assert_eq!(status(&kde, dir.path()), None);
        assert!(append(&kde, &dir.path().join("kde"), "whisrs").is_err());
        assert!(append(&hypr, &dir.path().join("none.conf"), "whisrs").is_err());
        assert!(!dir.path().join("none.conf").exists());
    }
}
