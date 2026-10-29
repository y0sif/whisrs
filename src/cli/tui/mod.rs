//! The TUI behind `whisrs config`: a full-screen editor for config.toml,
//! with Home as the hub, built on ratatui.
//!
//! It lives in the CLI binary, so the daemon never links ratatui. Everything
//! it decides is in [`app::App::update`], which is pure: keys and worker
//! answers in, an [`app::Effect`] out. This module runs the effects (save,
//! restart, `$EDITOR`) and owns the terminal. The daemon is only ever asked
//! from [`worker`] threads, so a daemon that hangs cannot freeze the screen.

mod app;
mod complete;
mod form;
mod glyphs;
mod keys;
mod options;
#[cfg(test)]
mod render_tests;
mod schema;
mod screens;
#[cfg(test)]
mod testing;
mod theme;
mod widgets;
mod worker;

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};
use whisrs::service::ServiceManager;

use app::{App, Effect, Msg};
use glyphs::Glyphs;
use theme::{Depth, Ground, Theme};
use widgets::Ui;

/// Which screen opens first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    Config,
}

impl Entry {
    fn command(self) -> &'static str {
        match self {
            Entry::Config => "whisrs config",
        }
    }
}

/// `whisrs config` refused because it is not on an interactive terminal.
/// Its text is the whole message, printed without a prefix.
#[derive(Debug)]
pub struct NotATerminal(String);

impl std::fmt::Display for NotATerminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotATerminal {}

/// The environment variables the TUI reads, taken once at start: nothing
/// below reads the process environment, so tests pass their own and never
/// mutate it.
#[derive(Debug, Clone, Default)]
pub struct Env(HashMap<String, String>);

impl Env {
    fn capture() -> Env {
        Env(std::env::vars_os()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
            .collect())
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    #[cfg(test)]
    pub fn from_pairs(pairs: &[(&str, &str)]) -> Env {
        Env(pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect())
    }
}

/// The files and socket the TUI works on.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config: PathBuf,
    pub vocabulary: PathBuf,
    pub socket: PathBuf,
    /// Where `whisrs setup` puts whisper models.
    pub models: PathBuf,
}

impl Paths {
    fn detect() -> Paths {
        Paths {
            config: whisrs::config_path(),
            vocabulary: whisrs::config::vocabulary::vocabulary_path(),
            socket: whisrs::socket_path(),
            models: dirs::data_dir()
                .unwrap_or_else(|| PathBuf::from("~/.local/share"))
                .join("whisrs/models"),
        }
    }
}

/// Open the TUI at `entry`. Refuses, touching nothing, when stdin or stdout
/// is not a terminal or `TERM` is `dumb`, and before the TUI opens when the
/// config cannot be used (#134).
pub fn run(entry: Entry) -> anyhow::Result<()> {
    let env = Env::capture();
    let paths = Paths::detect();
    if let Some(refusal) = terminal_refusal(
        entry,
        &env,
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        &paths.config,
    ) {
        return Err(NotATerminal(refusal).into());
    }
    let service = ServiceManager::detect();
    let mut app = App::prepare(entry, paths, env, service).map_err(anyhow::Error::msg)?;

    let glyphs = Glyphs::detect(&app.env);
    let depth = Depth::detect(&app.env);
    // Asked before raw mode and before anything reads keys, so the answer
    // is not taken for keystrokes (`Ground::probe` skips it over SSH, where
    // it can come late). NO_COLOR needs no ground.
    let ground = match depth {
        Depth::NoColor => Ground::default(),
        _ => Ground::probe(&app.env),
    };

    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(e) => {
            // Raw mode may be on already, with the alternate screen not.
            ratatui::restore();
            return Err(e).context("cannot open the terminal");
        }
    };
    let (tx, rx) = mpsc::channel();
    let workers = worker::Workers::start(
        tx,
        &app.paths.socket,
        app.service,
        worker::Lister {
            devices: worker::list_devices_quietly,
            models: app.paths.models.clone(),
            home: app.env.get("HOME").map(str::to_string),
        },
    );
    let result = restoring(ratatui::restore, || {
        drain_early_input()?;
        event_loop(
            &mut terminal,
            &mut app,
            &workers,
            &rx,
            glyphs,
            depth,
            ground,
        )
    });
    for line in &app.farewell {
        println!("{line}");
    }
    result
}

/// Run `body` in the TUI, then `restore` the terminal, whatever `body`
/// returned: an error from anywhere in it leaves a usable shell.
fn restoring<T>(
    restore: impl FnOnce(),
    body: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let result = body();
    restore();
    result
}

/// Read away whatever arrived before the first frame: it is not a key for
/// it. A colour reply that came late would read as Alt+], r, g, b and hex
/// digits.
fn drain_early_input() -> std::io::Result<()> {
    while event::poll(Duration::ZERO)? {
        event::read()?;
    }
    Ok(())
}

/// The refusal for a session that is not on an interactive terminal, or
/// `None` when it is.
fn terminal_refusal(entry: Entry, env: &Env, tty: bool, config: &Path) -> Option<String> {
    (!tty || env.get("TERM") == Some("dumb")).then(|| {
        format!(
            "{} needs an interactive terminal. Edit {} (reference: docs/configuration.md), then \
             run whisrs restart.",
            entry.command(),
            tilde(config, env.get("HOME")).display()
        )
    })
}

/// `path` with the home directory written `~`.
fn tilde(path: &Path, home: Option<&str>) -> PathBuf {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) => Path::new("~").join(rest),
        None => path.to_path_buf(),
    }
}

/// How often the loop wakes with nothing to do, to take worker answers.
const TICK: Duration = Duration::from_millis(200);

/// How long a changed value shows reversed.
const FLASH: Duration = Duration::from_millis(150);

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    workers: &worker::Workers,
    rx: &mpsc::Receiver<Msg>,
    glyphs: Glyphs,
    depth: Depth,
    ground: Ground,
) -> anyhow::Result<()> {
    let mut redraw = true;
    let mut flash_until: Option<Instant> = None;
    loop {
        if redraw {
            // From the config as edited, so a new `[overlay] theme` shows at
            // once.
            let theme = Theme::new(&app.overlay(), depth, ground);
            let ui = Ui {
                theme: &theme,
                glyphs: &glyphs,
            };
            terminal.draw(|frame| app.view(frame, &ui))?;
            redraw = false;
        }
        if app.quit {
            return Ok(());
        }

        let wait = flash_until
            .map(|until| until.saturating_duration_since(Instant::now()))
            .map_or(TICK, |left| left.min(TICK));
        if event::poll(wait)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let effect = app.update(Msg::Key(key));
                    perform(terminal, app, workers, effect)?;
                    redraw = true;
                }
                Event::Resize(..) => redraw = true,
                _ => {}
            }
        }
        while let Ok(msg) = rx.try_recv() {
            let before = app.daemon.clone();
            let is_daemon = matches!(msg, Msg::Daemon(_));
            let effect = app.update(msg);
            perform(terminal, app, workers, effect)?;
            // The poll answers every 2 s; only a change is worth a frame.
            redraw |= !is_daemon || app.daemon != before;
        }

        match (app.flash.is_some(), flash_until) {
            (true, None) => flash_until = Some(Instant::now() + FLASH),
            (true, Some(until)) if Instant::now() >= until => {
                app.flash = None;
                flash_until = None;
                redraw = true;
            }
            (false, Some(_)) => flash_until = None,
            _ => {}
        }
    }
}

/// Run `effect`, and whatever the answer to it asks for next.
fn perform(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    workers: &worker::Workers,
    mut effect: Option<Effect>,
) -> anyhow::Result<()> {
    while let Some(next) = effect.take() {
        effect = match next {
            Effect::Save | Effect::SaveOver => {
                let overwrite = next == Effect::SaveOver;
                let saved = screens::config::save(&app.config, &app.paths, overwrite);
                app.update(Msg::Saved(saved))
            }
            Effect::Restart | Effect::RestartAnyway => {
                workers.restart(next == Effect::RestartAnyway);
                None
            }
            Effect::List(kind) => {
                workers.list(kind);
                None
            }
            Effect::Reclaim => {
                reclaim(terminal)?;
                None
            }
            Effect::Reload => {
                let reload = screens::config::reload(&app.paths);
                app.update(Msg::Reloaded(reload))
            }
            Effect::OpenEditor => {
                match suspend_for_editor(terminal, &app.env, &app.paths.config)? {
                    Ok(()) => {
                        let reload = screens::config::reload(&app.paths);
                        app.update(Msg::Reloaded(reload))
                    }
                    Err(why) => app.update(Msg::EditorFailed(why)),
                }
            }
            Effect::Quit => {
                app.quit = true;
                None
            }
        };
    }
    Ok(())
}

/// The editors tried, in order, when neither `$VISUAL` nor `$EDITOR` is
/// set. Arch installs no `vi` by default.
const EDITORS: [&str; 4] = ["nvim", "vim", "vi", "nano"];

/// What `e` runs: `$VISUAL`, else `$EDITOR`, else the first of [`EDITORS`]
/// on `PATH`; `None` when there is none.
fn editor_command(env: &Env) -> Option<String> {
    ["VISUAL", "EDITOR"]
        .into_iter()
        .find_map(|key| env.get(key).filter(|value| !value.trim().is_empty()))
        .map(str::to_string)
        .or_else(|| {
            EDITORS
                .into_iter()
                .find(|name| find_on_path(env, name).is_some())
                .map(str::to_string)
        })
}

/// The executable `name` in `env`'s `PATH`, as a shell would find it.
pub fn find_on_path(env: &Env, name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    env.get("PATH")?
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(name))
        .find(|path| {
            std::fs::metadata(path)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

/// How the editor's run ended: `Err` when it never started (`sh` answers
/// 126 for a command it cannot run, 127 for one it cannot find), so the
/// file is not read again as if it had been edited.
fn editor_ran(
    editor: &str,
    status: std::io::Result<std::process::ExitStatus>,
) -> Result<(), String> {
    match status {
        Ok(status) if matches!(status.code(), Some(126 | 127)) => Err(format!(
            "{editor} did not start (sh exit {}): set $VISUAL or $EDITOR to an editor that is \
             installed",
            status.code().unwrap_or_default()
        )),
        Ok(_) => Ok(()),
        Err(e) => Err(format!("cannot run {editor}: {e}")),
    }
}

/// Leave the TUI for the editor [`editor_command`] names, on `path`, and
/// come back to it. The command may carry arguments (`code -w`), so it runs
/// through `sh`. The inner `Err` is an editor that did not start, or none
/// found, which never leaves the screen; the outer one a terminal that
/// could not be taken back.
fn suspend_for_editor(
    terminal: &mut ratatui::DefaultTerminal,
    env: &Env,
    path: &Path,
) -> anyhow::Result<Result<(), String>> {
    let Some(editor) = editor_command(env) else {
        return Ok(Err("no editor found: set $VISUAL or $EDITOR".to_string()));
    };
    // A device listing may have stderr on /dev/null: the editor waits
    // until it is back, so it never gets /dev/null as its stderr.
    let status = {
        let _stderr = worker::stderr_lock();
        ratatui::restore();
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("sh")
            .arg(path)
            .status()
    };
    reclaim(terminal)?;
    Ok(editor_ran(&editor, status))
}

/// Take the terminal back after it was given away (to `$EDITOR`, or by the
/// panic hook), without `ratatui::init` again: that would stack a second
/// panic hook.
fn reclaim(terminal: &mut ratatui::DefaultTerminal) -> anyhow::Result<()> {
    enable_raw_mode().context("cannot reopen the terminal")?;
    execute!(std::io::stdout(), EnterAlternateScreen).context("cannot reopen the terminal")?;
    terminal.clear()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An error inside the TUI, the early-input drain's first, still
    /// restores the terminal.
    #[test]
    fn an_error_in_the_tui_restores_the_terminal() {
        let mut restored = false;
        let result: anyhow::Result<()> = restoring(
            || restored = true,
            || Err(std::io::Error::other("cannot read the terminal").into()),
        );
        assert!(result.is_err());
        assert!(restored);
    }

    /// A fake `PATH`: a folder holding executables named `names`.
    fn path_with(names: &[&str]) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        for name in names {
            let bin = dir.path().join(name);
            std::fs::write(&bin, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Not executable: not an editor.
        std::fs::write(dir.path().join("nano"), "").unwrap();
        dir
    }

    #[test]
    fn the_editor_is_visual_then_editor_then_the_first_on_path() {
        let bin = path_with(&["vim", "vi"]);
        let path = bin.path().to_str().unwrap();
        let env = |pairs: &[(&str, &str)]| {
            let mut all = vec![("PATH", path)];
            all.extend_from_slice(pairs);
            Env::from_pairs(&all)
        };
        assert_eq!(
            editor_command(&env(&[("VISUAL", "code -w"), ("EDITOR", "nvim")])).as_deref(),
            Some("code -w")
        );
        assert_eq!(
            editor_command(&env(&[("VISUAL", " "), ("EDITOR", "hx")])).as_deref(),
            Some("hx")
        );
        // No nvim here: vim comes next.
        assert_eq!(editor_command(&env(&[])).as_deref(), Some("vim"));
        let bare = path_with(&[]);
        let none = Env::from_pairs(&[("PATH", bare.path().to_str().unwrap())]);
        assert_eq!(editor_command(&none), None);
        assert_eq!(editor_command(&Env::default()), None);
    }

    /// `sh` exits 126 or 127 when the editor never started: that is a
    /// failure naming it, not an edit to read back.
    #[test]
    fn an_editor_that_did_not_start_is_a_failure() {
        use std::os::unix::process::ExitStatusExt;
        let exit = |code: i32| Ok(std::process::ExitStatus::from_raw(code << 8));
        assert_eq!(editor_ran("nvim", exit(0)), Ok(()));
        assert_eq!(editor_ran("nvim", exit(1)), Ok(()));
        for code in [126, 127] {
            let why = editor_ran("vi", exit(code)).unwrap_err();
            assert!(why.starts_with("vi did not start"), "{why}");
        }
        assert!(editor_ran("vi", Err(std::io::Error::other("no sh")))
            .unwrap_err()
            .contains("cannot run vi"));
    }

    #[test]
    fn a_session_off_a_terminal_is_refused_with_the_path() {
        let env = Env::from_pairs(&[("HOME", "/home/ada"), ("TERM", "xterm-kitty")]);
        let config = Path::new("/home/ada/.config/whisrs/config.toml");
        assert_eq!(terminal_refusal(Entry::Config, &env, true, config), None);
        assert_eq!(
            terminal_refusal(Entry::Config, &env, false, config).as_deref(),
            Some(
                "whisrs config needs an interactive terminal. Edit \
                 ~/.config/whisrs/config.toml (reference: docs/configuration.md), then run \
                 whisrs restart."
            )
        );
        let dumb = Env::from_pairs(&[("TERM", "dumb")]);
        let refusal = terminal_refusal(Entry::Config, &dumb, true, config).unwrap();
        assert!(
            refusal.contains("/home/ada/.config/whisrs/config.toml"),
            "{refusal}"
        );
    }
}
