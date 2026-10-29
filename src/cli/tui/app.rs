//! The TUI's state and its one update function: a message in (a key, a
//! worker's answer), the state changed, and at most one [`Effect`] out for
//! the loop to run. Nothing here touches the terminal, the disk or the
//! daemon, so key sequences are tested without any of them.

use ratatui::crossterm::event::KeyEvent;
use ratatui::text::{Line, Span};
use ratatui::Frame;
use whisrs::service::ServiceManager;
use whisrs::OverlayConfig;

use super::keys::{self, Action, Mode};
use super::screens::config::{ConfigScreen, Loaded, Saved};
use super::screens::{config, home};
use super::widgets::{dialog, frame, Notice, Tone, Ui};
use super::worker::{DaemonView, RestartReport};
use super::{tilde, Entry, Env, Paths};

/// A screen of the stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Home,
    Config,
}

pub enum Msg {
    Key(KeyEvent),
    Daemon(DaemonView),
    /// The answer to [`Effect::Save`].
    Saved(Result<Saved, String>),
    Restarted(RestartReport),
    /// config.toml read again after `$EDITOR`, or why it cannot be used.
    Reloaded(Result<Loaded, String>),
    /// `$EDITOR` did not start.
    EditorFailed(String),
}

/// What the loop runs for the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Validate and write the config; answered with [`Msg::Saved`].
    Save,
    /// Restart the daemon (on a worker); answered with [`Msg::Restarted`].
    Restart,
    /// Leave the TUI for `$EDITOR` on config.toml; answered with
    /// [`Msg::Reloaded`].
    OpenEditor,
    Quit,
}

/// What the unsaved-changes dialog was opened for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum After {
    Quit,
    Editor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    Unsaved(After),
    Help,
    /// Delete entry `index` of the current section's list.
    DeleteEntry {
        index: usize,
        name: String,
    },
}

pub struct App {
    pub env: Env,
    pub paths: Paths,
    pub service: ServiceManager,
    stack: Vec<Screen>,
    pub home: home::Home,
    pub config: ConfigScreen,
    pub daemon: DaemonView,
    pub dialog: Option<Dialog>,
    /// What the load found: unknown keys, warnings, a fresh config.
    pub notices: Vec<Notice>,
    /// The answer to the last action.
    pub status: Option<Notice>,
    /// What a save asked for by the unsaved-changes dialog leads to.
    pending: Option<After>,
    /// Saved on the way out: quit once the restart answered.
    quitting: bool,
    /// The row whose value just changed, shown reversed for a moment.
    pub flash: Option<String>,
    pub quit: bool,
    /// Printed after the TUI closed: what a save on the way out did.
    pub farewell: Vec<String>,
}

impl App {
    /// Load the config and build the app, before the terminal is taken: a
    /// config that cannot be used is refused here, with the text to print.
    pub fn prepare(
        entry: Entry,
        paths: Paths,
        env: Env,
        service: ServiceManager,
    ) -> Result<App, String> {
        let loaded = config::load(&paths)?;
        Ok(App::new(entry, loaded, paths, env, service))
    }

    pub fn new(
        entry: Entry,
        loaded: Loaded,
        paths: Paths,
        env: Env,
        service: ServiceManager,
    ) -> App {
        let stack = match entry {
            Entry::Config => vec![Screen::Home, Screen::Config],
        };
        let notices = loaded.notices.clone();
        App {
            env,
            paths,
            service,
            stack,
            home: home::Home::default(),
            config: ConfigScreen::new(loaded),
            daemon: DaemonView::Checking,
            dialog: None,
            notices,
            status: None,
            pending: None,
            quitting: false,
            flash: None,
            quit: false,
            farewell: Vec::new(),
        }
    }

    pub fn screen(&self) -> Screen {
        *self.stack.last().expect("the stack always holds Home")
    }

    /// The overlay theme as edited, which colours the TUI too.
    pub fn overlay(&self) -> OverlayConfig {
        self.config
            .form
            .config()
            .overlay
            .clone()
            .unwrap_or_default()
    }

    /// The daemon dot: up, down, or not known yet.
    pub fn daemon_up(&self) -> Option<bool> {
        match self.daemon {
            DaemonView::Checking => None,
            DaemonView::Up(_) => Some(true),
            DaemonView::Down | DaemonView::NoAnswer(_) => Some(false),
        }
    }

    /// The lines at the foot of a screen, `max` columns wide.
    pub fn messages(&self, ui: &Ui, max: usize) -> Vec<Line<'static>> {
        frame::messages(&self.notices, self.status.as_ref(), ui, max)
    }

    pub fn update(&mut self, msg: Msg) -> Option<Effect> {
        match msg {
            Msg::Key(key) => self.key(key),
            Msg::Daemon(view) => {
                self.daemon = view;
                None
            }
            Msg::Saved(Ok(saved)) => self.saved(saved),
            Msg::Saved(Err(why)) => {
                self.pending = None;
                self.status = Some(Notice::new(Tone::Error, format!("not saved: {why}")));
                None
            }
            Msg::Restarted(report) => self.restarted(report),
            Msg::Reloaded(reload) => {
                self.reloaded(reload);
                None
            }
            Msg::EditorFailed(why) => {
                self.status = Some(Notice::new(Tone::Error, why));
                None
            }
        }
    }

    fn key(&mut self, key: KeyEvent) -> Option<Effect> {
        self.flash = None;
        let mode = if self.dialog.is_none() && self.config.typing() {
            Mode::Text
        } else {
            Mode::Browse
        };
        let action = keys::action(key, mode)?;
        if let Some(dialog) = self.dialog.take() {
            return self.dialog_key(dialog, action);
        }
        if action == Action::Quit {
            return self.ask_quit();
        }
        if mode == Mode::Browse {
            match action {
                Action::Help => {
                    self.dialog = Some(Dialog::Help);
                    return None;
                }
                Action::Save => return self.save(),
                Action::Editor => return self.editor(),
                Action::Restart => {
                    self.status = Some(Notice::new(Tone::Muted, "restarting the daemon"));
                    return Some(Effect::Restart);
                }
                _ => {}
            }
        }
        match self.screen() {
            Screen::Home => {
                if self.home.key(action) {
                    self.stack.push(Screen::Config);
                }
                None
            }
            Screen::Config => {
                let outcome = self.config.key(action);
                if outcome.leave {
                    self.stack.pop();
                }
                if let Some(status) = outcome.status {
                    self.status = Some(status);
                }
                self.flash = outcome.flash;
                self.dialog = outcome.dialog;
                None
            }
        }
    }

    fn dialog_key(&mut self, dialog: Dialog, action: Action) -> Option<Effect> {
        match (&dialog, action) {
            (Dialog::Help, _) => None,
            (Dialog::Unsaved(after), Action::Save) => {
                self.pending = Some(*after);
                self.save()
            }
            (Dialog::Unsaved(after), Action::Delete) => {
                self.config.discard();
                Some(match after {
                    After::Quit => Effect::Quit,
                    After::Editor => Effect::OpenEditor,
                })
            }
            (Dialog::DeleteEntry { index, name }, Action::Delete) => {
                self.status = Some(self.config.delete_entry(*index, name));
                None
            }
            (_, Action::Back) => None,
            _ => {
                self.dialog = Some(dialog);
                None
            }
        }
    }

    fn ask_quit(&mut self) -> Option<Effect> {
        if self.config.form.is_dirty() && !self.quitting {
            self.dialog = Some(Dialog::Unsaved(After::Quit));
            None
        } else {
            Some(Effect::Quit)
        }
    }

    fn save(&mut self) -> Option<Effect> {
        if let Some(why) = &self.config.broken {
            self.pending = None;
            self.status = Some(Notice::new(
                Tone::Error,
                format!("not saved: config.toml does not load ({why}). e opens it to fix it."),
            ));
            return None;
        }
        if !self.config.form.is_dirty() && !self.config.fresh {
            let after = self.pending.take();
            self.status = Some(Notice::new(Tone::Muted, "no changes to save"));
            return match after {
                Some(After::Quit) => Some(Effect::Quit),
                Some(After::Editor) => Some(Effect::OpenEditor),
                None => None,
            };
        }
        Some(Effect::Save)
    }

    fn editor(&mut self) -> Option<Effect> {
        if self.config.form.is_dirty() {
            self.dialog = Some(Dialog::Unsaved(After::Editor));
            None
        } else {
            Some(Effect::OpenEditor)
        }
    }

    fn saved(&mut self, saved: Saved) -> Option<Effect> {
        let was_fresh = self.config.fresh;
        let file = tilde(&saved.path, self.env.get("HOME"))
            .display()
            .to_string();
        self.notices = saved.loaded.notices.clone();
        self.notices.extend(
            saved
                .warnings
                .iter()
                .map(|w| Notice::new(Tone::Warn, format!("warning: {w}"))),
        );
        if was_fresh {
            self.notices.push(Notice::new(
                Tone::Muted,
                "A new config: whisrs setup once installs the uinput rule, the service and a \
                 keybinding.",
            ));
        }
        self.config.saved(saved.loaded);
        match self.pending.take() {
            Some(After::Editor) => {
                self.status = Some(Notice::new(Tone::Ok, format!("saved {file}")));
                Some(Effect::OpenEditor)
            }
            Some(After::Quit) => {
                self.quitting = true;
                self.farewell.push(format!("Saved {file}."));
                self.status = Some(Notice::new(
                    Tone::Muted,
                    format!("saved {file}, restarting the daemon"),
                ));
                Some(Effect::Restart)
            }
            None => {
                self.status = Some(Notice::new(
                    Tone::Muted,
                    format!("saved {file}, restarting the daemon"),
                ));
                Some(Effect::Restart)
            }
        }
    }

    fn restarted(&mut self, report: RestartReport) -> Option<Effect> {
        let notice = match report {
            RestartReport::Restarted => Notice::new(Tone::Ok, "daemon restarted"),
            RestartReport::Busy(doing) => Notice::new(
                Tone::Warn,
                format!("restart skipped: daemon is {doing}, press r later"),
            ),
            RestartReport::Failed(manager) => {
                let hint = manager.restart_hint().unwrap_or("restart");
                let check = match manager {
                    ServiceManager::OpenRc => {
                        "the daemon log under $XDG_STATE_HOME/whisrs/whisrsd.log"
                    }
                    _ => "journalctl --user -u whisrs -e",
                };
                Notice::new(Tone::Error, format!("{hint} failed: see {check}"))
            }
            RestartReport::NoService => Notice::new(
                Tone::Warn,
                "no whisrs service installed: restart the daemon for the new config to take \
                 effect (pkill whisrsd; sleep 0.2; whisrsd &)",
            ),
        };
        if self.quitting {
            self.farewell.push(notice.text.clone());
            self.status = Some(notice);
            return Some(Effect::Quit);
        }
        self.status = Some(notice);
        None
    }

    fn reloaded(&mut self, reload: Result<Loaded, String>) {
        match reload {
            Ok(loaded) => {
                self.notices = loaded.notices.clone();
                self.config.reloaded(loaded);
                self.status = Some(Notice::new(
                    Tone::Muted,
                    "read config.toml again; r restarts the daemon with it",
                ));
            }
            Err(why) => {
                self.status = Some(Notice::new(
                    Tone::Error,
                    format!("config.toml no longer loads, so w is off until e fixes it: {why}"),
                ));
                self.config.broken = Some(why);
            }
        }
    }

    /// Draw the current screen, and the dialog over it.
    pub fn view(&self, f: &mut Frame, ui: &Ui) {
        let area = f.area();
        let Some(_) = frame::place(area, None) else {
            return frame::too_small(f, ui);
        };
        match self.screen() {
            Screen::Home => home::view(self, f, ui),
            Screen::Config => config::view(self, f, ui),
        }
        if let Some(dialog) = &self.dialog {
            dialog::draw(f, area, ui, dialog_lines(dialog, ui));
        }
    }
}

/// What a dialog says.
fn dialog_lines(dialog: &Dialog, ui: &Ui) -> Vec<Line<'static>> {
    let keys = |pairs: &[(&'static str, &'static str)]| {
        let sep = format!(" {} ", ui.glyphs.separator);
        let mut spans = Vec::new();
        for (i, (key, what)) in pairs.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(sep.clone(), ui.theme.faint));
            }
            spans.push(Span::styled(*key, ui.theme.accent));
            spans.push(Span::styled(format!(" {what}"), ui.theme.muted));
        }
        Line::from(spans)
    };
    match dialog {
        Dialog::Unsaved(_) => vec![
            Line::from("Unsaved changes to config.toml."),
            Line::default(),
            keys(&[("w", "save"), ("d", "discard"), ("esc", "stay")]),
        ],
        Dialog::DeleteEntry { name, .. } => vec![
            Line::from(format!("Delete {name}? It goes when you save.")),
            Line::default(),
            keys(&[("d", "delete"), ("esc", "keep")]),
        ],
        Dialog::Help => {
            let row = |key: &'static str, what: &'static str| {
                Line::from(vec![
                    Span::styled(format!("{key:<10}"), ui.theme.accent),
                    Span::raw(what),
                ])
            };
            vec![
                row("j k", "move (or the arrows), g G first and last"),
                row("h l", "change a choice or number, or move between panes"),
                row("tab", "the other pane"),
                row("enter", "open, edit, or commit what you typed"),
                row("space", "flip a toggle"),
                row("x", "back to the default"),
                row("a d", "add or delete, in a list"),
                row("/", "filter the rows; esc clears it"),
                row("w", "save, then restart the daemon"),
                row("e", "edit config.toml in $EDITOR"),
                row("r", "restart the daemon"),
                row("esc", "back one level, or cancel typing"),
                row("q", "quit, asking first about unsaved changes"),
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    use super::*;
    use crate::tui::screens::config::{load, save};
    use crate::tui::testing::{app, chord, paths, press, Fixture};

    /// Turn notifications off, an edit to be unsaved.
    fn edited() -> Fixture {
        let mut fixture = app("[general]\nbackend = \"groq\"\n[groq]\napi_key = \"k\"\n");
        press(&mut fixture.app, "\njj ");
        assert!(!fixture.app.config.form.config().general.notify);
        assert!(fixture.app.config.form.is_dirty());
        fixture
    }

    #[test]
    fn quitting_with_nothing_unsaved_just_quits() {
        let mut fixture = app("");
        assert_eq!(press(&mut fixture.app, "q"), [Effect::Quit]);
        assert_eq!(
            chord(&mut fixture.app, KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(Effect::Quit)
        );
    }

    /// q asks first about unsaved edits: Esc stays, d discards and quits.
    #[test]
    fn the_unsaved_dialog_stays_or_discards() {
        let mut fixture = edited();
        assert!(press(&mut fixture.app, "q").is_empty());
        assert_eq!(fixture.app.dialog, Some(Dialog::Unsaved(After::Quit)));
        // Other keys leave it up.
        assert!(press(&mut fixture.app, "jq").is_empty());
        assert_eq!(fixture.app.dialog, Some(Dialog::Unsaved(After::Quit)));
        press(&mut fixture.app, "\x1b");
        assert_eq!(fixture.app.dialog, None);
        assert!(fixture.app.config.form.is_dirty());

        assert!(chord(&mut fixture.app, KeyCode::Char('c'), KeyModifiers::CONTROL).is_none());
        assert_eq!(press(&mut fixture.app, "d"), [Effect::Quit]);
        assert!(!fixture.app.config.form.is_dirty());
    }

    /// w in the dialog saves, restarts, then quits, and says what it did
    /// once the TUI is gone.
    #[test]
    fn save_and_quit_restarts_then_quits() {
        let mut fixture = edited();
        press(&mut fixture.app, "q");
        assert_eq!(press(&mut fixture.app, "w"), [Effect::Save]);
        let paths = paths(fixture.dir.path());
        let saved = save(&fixture.app.config, &paths);
        assert_eq!(fixture.app.update(Msg::Saved(saved)), Some(Effect::Restart));
        assert_eq!(
            fixture.app.update(Msg::Restarted(RestartReport::Restarted)),
            Some(Effect::Quit)
        );
        assert_eq!(fixture.app.farewell.len(), 2);
        assert!(fixture.app.farewell[0].starts_with("Saved "));
        assert_eq!(fixture.app.farewell[1], "daemon restarted");
        // A second q while it waits for the restart does not ask again.
        assert_eq!(press(&mut fixture.app, "q"), [Effect::Quit]);
    }

    /// A failed save keeps the dialog's quit from happening.
    #[test]
    fn a_refused_save_does_not_quit() {
        let mut fixture = edited();
        press(&mut fixture.app, "q");
        press(&mut fixture.app, "w");
        assert_eq!(fixture.app.update(Msg::Saved(Err("invalid".into()))), None);
        assert!(!fixture.app.quit);
        assert!(fixture.app.config.form.is_dirty());
        // The next save is a plain one.
        assert_eq!(press(&mut fixture.app, "w"), [Effect::Save]);
        let saved = save(&fixture.app.config, &paths(fixture.dir.path()));
        assert_eq!(fixture.app.update(Msg::Saved(saved)), Some(Effect::Restart));
        assert_eq!(
            fixture.app.update(Msg::Restarted(RestartReport::Restarted)),
            None
        );
    }

    /// e with unsaved edits asks first; d discards them and opens $EDITOR.
    #[test]
    fn the_editor_asks_about_unsaved_edits() {
        let mut fixture = edited();
        assert!(press(&mut fixture.app, "e").is_empty());
        assert_eq!(fixture.app.dialog, Some(Dialog::Unsaved(After::Editor)));
        assert_eq!(press(&mut fixture.app, "d"), [Effect::OpenEditor]);
        assert!(!fixture.app.config.form.is_dirty());
        // Clean, it opens straight away.
        assert_eq!(press(&mut fixture.app, "e"), [Effect::OpenEditor]);

        // w saves first, then opens it, without a restart.
        let mut fixture = edited();
        press(&mut fixture.app, "e");
        assert_eq!(press(&mut fixture.app, "w"), [Effect::Save]);
        let saved = save(&fixture.app.config, &paths(fixture.dir.path()));
        assert_eq!(
            fixture.app.update(Msg::Saved(saved)),
            Some(Effect::OpenEditor)
        );
    }

    #[test]
    fn a_busy_daemon_is_left_alone_and_r_tries_again() {
        let mut fixture = app("");
        fixture
            .app
            .update(Msg::Restarted(RestartReport::Busy("recording")));
        let status = fixture.app.status.clone().unwrap();
        assert_eq!(status.tone, Tone::Warn);
        assert_eq!(
            status.text,
            "restart skipped: daemon is recording, press r later"
        );
        assert_eq!(press(&mut fixture.app, "r"), [Effect::Restart]);

        fixture.app.update(Msg::Restarted(RestartReport::Failed(
            ServiceManager::Systemd,
        )));
        let status = fixture.app.status.clone().unwrap();
        assert_eq!(status.tone, Tone::Error);
        assert!(status.text.contains("journalctl --user -u whisrs -e"));

        fixture.app.update(Msg::Restarted(RestartReport::NoService));
        let status = fixture.app.status.clone().unwrap();
        assert!(status.text.contains("pkill whisrsd; sleep 0.2; whisrsd &"));
    }

    /// Esc on Config goes up to Home; enter on Home opens Config again,
    /// edits kept.
    #[test]
    fn esc_goes_home_and_back() {
        let mut fixture = edited();
        press(&mut fixture.app, "\t\x1b");
        assert_eq!(fixture.app.screen(), Screen::Home);
        press(&mut fixture.app, "\n");
        assert_eq!(fixture.app.screen(), Screen::Config);
        assert!(fixture.app.config.form.is_dirty());
    }

    /// A save of a config that had no file writes one, and points to setup.
    #[test]
    fn saving_a_fresh_config_points_to_setup() {
        let toml = "[general]\nbackend = \"groq\"\n[groq]\napi_key = \"k\"\n";
        let mut fixture = app(toml);
        let paths = paths(fixture.dir.path());
        std::fs::remove_file(&paths.config).unwrap();
        fixture.app.config.fresh = true;
        let saved = save(&fixture.app.config, &paths);
        assert_eq!(fixture.app.update(Msg::Saved(saved)), Some(Effect::Restart));
        assert!(paths.config.is_file());
        assert!(fixture
            .app
            .notices
            .iter()
            .any(|n| n.text.contains("whisrs setup once")));
        assert!(!load(&paths).unwrap().fresh);
    }
}
