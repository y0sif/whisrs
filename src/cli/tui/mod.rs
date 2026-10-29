//! The TUI behind `whisrs config`: a full-screen editor for config.toml,
//! with Home as the hub, built on ratatui.
//!
//! It lives in the CLI binary, so the daemon never links ratatui. Everything
//! it decides is in [`app::App::update`], which is pure: keys and worker
//! answers in, an [`app::Effect`] out. This module runs the effects (save,
//! restart, `$EDITOR`) and owns the terminal. The daemon is only ever asked
//! from [`worker`] threads, so a daemon that hangs cannot freeze the screen.

mod app;
mod form;
mod glyphs;
mod keys;
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
}

impl Paths {
    fn detect() -> Paths {
        Paths {
            config: whisrs::config_path(),
            vocabulary: whisrs::config::vocabulary::vocabulary_path(),
            socket: whisrs::socket_path(),
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
    let workers = worker::Workers::start(tx, &app.paths.socket, app.service);
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

/// Leave the TUI for `$VISUAL`, else `$EDITOR`, else `vi`, on `path`, and
/// come back to it. The editor command may carry arguments (`code -w`), so
/// it runs through `sh`. The inner `Err` is an editor that did not start;
/// the outer one a terminal that could not be taken back.
fn suspend_for_editor(
    terminal: &mut ratatui::DefaultTerminal,
    env: &Env,
    path: &Path,
) -> anyhow::Result<Result<(), String>> {
    let editor = ["VISUAL", "EDITOR"]
        .into_iter()
        .find_map(|key| env.get(key).filter(|value| !value.trim().is_empty()))
        .unwrap_or("vi");
    ratatui::restore();
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(path)
        .status();
    // Back in, without `ratatui::init` again: that would stack a second
    // panic hook.
    enable_raw_mode().context("cannot reopen the terminal")?;
    execute!(std::io::stdout(), EnterAlternateScreen).context("cannot reopen the terminal")?;
    terminal.clear()?;
    Ok(status
        .map(|_| ())
        .map_err(|e| format!("cannot run {editor}: {e}")))
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
