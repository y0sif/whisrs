//! The GNOME Shell extension that draws the overlay on GNOME, which has no
//! wlroots layer-shell: whether it is installed, and installing it.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::Run;

/// The extension's UUID, and its folder name.
pub const UUID: &str = "whisrs-overlay@eresende.github";

/// Where the extension goes under the data folder (`~/.local/share`).
pub fn target(data_dir: &Path) -> PathBuf {
    data_dir.join("gnome-shell/extensions").join(UUID)
}

/// Whether the extension is in the data folder.
pub fn installed(data_dir: &Path) -> bool {
    target(data_dir).exists()
}

/// The extension in `contrib/`, if this install has it.
pub fn source() -> Option<PathBuf> {
    super::find_contrib_file(&format!("gnome-shell-extension/{UUID}"))
}

/// What [`install`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub path: PathBuf,
    /// `gnome-extensions enable` worked; else why not, for the user to run
    /// it (`gnome-extensions enable <UUID>`).
    pub enabled: Result<(), String>,
}

/// Copy the extension at `src` into the data folder and enable it with
/// `run`. GNOME may only show it after the next login.
pub fn install(src: Option<&Path>, data_dir: &Path, run: Run) -> Result<Installed> {
    let src = src.context(
        "the extension is not in contrib/ here: install whisrs from a clone of \
         https://github.com/y0sif/whisrs, or copy contrib/gnome-shell-extension/ by hand",
    )?;
    let path = target(data_dir);
    copy_dir(src, &path)
        .with_context(|| format!("failed to copy the extension to {}", path.display()))?;
    let enabled = run("gnome-extensions", &["enable", UUID]);
    Ok(Installed { path, enabled })
}

/// Copy the folder `from` to `to`, recursively.
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extension_is_copied_and_enabled() {
        let src = source().expect("contrib/gnome-shell-extension is on disk");
        let data = tempfile::tempdir().unwrap();
        assert!(!installed(data.path()));
        let asked = std::cell::RefCell::new(Vec::new());
        let run = |program: &str, args: &[&str]| {
            asked
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            Err("not on GNOME".to_string())
        };
        let done = install(Some(&src), data.path(), &run).unwrap();
        assert!(installed(data.path()));
        assert!(done.path.join("metadata.json").is_file());
        assert_eq!(done.enabled, Err("not on GNOME".to_string()));
        assert_eq!(*asked.borrow(), [format!("gnome-extensions enable {UUID}")]);

        let why = install(None, data.path(), &run).unwrap_err().to_string();
        assert!(why.contains("contrib/"), "{why}");
    }
}
