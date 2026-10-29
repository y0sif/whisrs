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
use super::options::Listing;
use super::schema::ListKind;
use super::screens::config::{ConfigScreen, Loaded, SaveError, Saved};
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
    /// The answer to [`Effect::Save`] and [`Effect::SaveOver`].
    Saved(Result<Saved, SaveError>),
    Restarted(RestartReport),
    /// config.toml read again after `$EDITOR`, or why it cannot be used.
    Reloaded(Result<Loaded, String>),
    /// `$EDITOR` did not start.
    EditorFailed(String),
    /// A worker's list: devices, model files.
    Listed(Listing),
    /// A worker panicked, and the panic hook gave the terminal back.
    Reclaim,
}

/// What the loop runs for the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Fetch a list on a worker; answered with [`Msg::Listed`].
    List(ListKind),
    /// Take the terminal back: raw mode and the alternate screen again.
    Reclaim,
    /// Validate and write the config, unless config.toml changed on disk
    /// since it was read; answered with [`Msg::Saved`].
    Save,
    /// [`Effect::Save`] over a config.toml that changed on disk.
    SaveOver,
    /// Restart the daemon (on a worker) if it is idle; answered with
    /// [`Msg::Restarted`].
    Restart,
    /// Restart it whatever it is doing.
    RestartAnyway,
    /// Leave the TUI for `$EDITOR` on config.toml; answered with
    /// [`Msg::Reloaded`].
    OpenEditor,
    /// Read config.toml again, dropping the edits; answered with
    /// [`Msg::Reloaded`].
    Reload,
    Quit,
}

/// What the unsaved-changes dialog was opened for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum After {
    Quit,
    Editor,
}

/// Whose keys the help lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpFor {
    Rows,
    Picker,
    Hotkey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    Unsaved(After),
    Help(HelpFor),
    /// Delete entry `index` of the current section's list.
    DeleteEntry {
        index: usize,
        name: String,
    },
    /// `r` found the daemon not answering: restart it anyway (default no)?
    RestartAnyway,
    /// A save found this file changed on disk since it was read.
    Changed(&'static str),
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
    /// The restart running was asked for with `r`, not by a save.
    restart_by_key: bool,
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
            config: ConfigScreen::new(loaded, env.clone()),
            env,
            paths,
            service,
            stack,
            home: home::Home::default(),
            daemon: DaemonView::Checking,
            dialog: None,
            notices,
            status: None,
            pending: None,
            quitting: false,
            restart_by_key: false,
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
            Msg::Saved(Err(SaveError::Failed(why))) => {
                self.pending = None;
                self.status = Some(Notice::new(Tone::Error, format!("not saved: {why}")));
                None
            }
            Msg::Saved(Err(SaveError::Changed(file))) => {
                self.dialog = Some(Dialog::Changed(file));
                None
            }
            Msg::Saved(Err(SaveError::Reread(why))) => {
                // It is on disk; what is not known is what it says now, so
                // nothing restarts and `w` waits for `e`.
                self.config.form.mark_saved();
                self.config.broken = Some(why.clone());
                let text = format!("saved, but re-reading it failed: {why}");
                self.status = Some(Notice::new(Tone::Warn, text.clone()));
                if self.pending.take() == Some(After::Quit) {
                    self.farewell.push(text);
                    return Some(Effect::Quit);
                }
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
            Msg::Listed(listing) => {
                self.config.listed(listing);
                // It came: a row that was looking for it is not any more.
                if self.status.as_ref().is_some_and(|s| s.text == "looking...") {
                    self.status = None;
                }
                None
            }
            Msg::Reclaim => Some(Effect::Reclaim),
        }
    }

    fn key(&mut self, key: KeyEvent) -> Option<Effect> {
        self.flash = None;
        let mode = if self.dialog.is_some() {
            Mode::Dialog
        } else if self.screen() == Screen::Config {
            self.config.mode()
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
        // A picker and the hotkey dialog have keys of their own; the help
        // closes back onto them.
        let help = match mode {
            Mode::Picker => Some(HelpFor::Picker),
            Mode::Hotkey => Some(HelpFor::Hotkey),
            _ => None,
        };
        if let (Action::Help, Some(help)) = (action, help) {
            self.dialog = Some(Dialog::Help(help));
            return None;
        }
        if mode == Mode::Browse {
            match action {
                Action::Help => {
                    self.dialog = Some(Dialog::Help(HelpFor::Rows));
                    return None;
                }
                Action::Save => return self.save(),
                Action::Editor => return self.editor(),
                Action::Restart => return self.restart(),
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
                outcome.load.map(Effect::List)
            }
        }
    }

    fn dialog_key(&mut self, dialog: Dialog, action: Action) -> Option<Effect> {
        match (&dialog, action) {
            (Dialog::Help(_), _) => None,
            (Dialog::Unsaved(after), Action::Type('w')) => {
                self.pending = Some(*after);
                self.save()
            }
            (Dialog::Unsaved(after), Action::Type('d')) => {
                self.config.discard();
                Some(match after {
                    After::Quit => Effect::Quit,
                    After::Editor => Effect::OpenEditor,
                })
            }
            (Dialog::DeleteEntry { index, name }, Action::Type('d')) => {
                self.status = Some(self.config.delete_entry(*index, name));
                None
            }
            (Dialog::RestartAnyway, Action::Type('y')) => {
                // The file may have stopped loading while the daemon was asked.
                if self.config.broken.is_some() {
                    self.status = Some(not_restarted_while_broken());
                    return None;
                }
                self.status = Some(Notice::new(Tone::Muted, "restarting the daemon"));
                Some(Effect::RestartAnyway)
            }
            // No is the default: anything else.
            (Dialog::RestartAnyway, _) => {
                self.status = Some(Notice::new(
                    Tone::Warn,
                    "restart skipped: daemon is not answering",
                ));
                None
            }
            (Dialog::Changed(_), Action::Type('o')) => Some(Effect::SaveOver),
            (Dialog::Changed(_), Action::Type('l')) => {
                self.pending = None;
                Some(Effect::Reload)
            }
            (Dialog::Changed(_), Action::Back) => {
                self.pending = None;
                self.status = Some(Notice::new(Tone::Muted, "not saved"));
                None
            }
            (_, Action::Back) => None,
            _ => {
                self.dialog = Some(dialog);
                None
            }
        }
    }

    /// `r`: restart the daemon if it is idle, once config.toml loads.
    fn restart(&mut self) -> Option<Effect> {
        if self.config.broken.is_some() {
            self.status = Some(not_restarted_while_broken());
            return None;
        }
        self.restart_by_key = true;
        self.status = Some(Notice::new(Tone::Muted, "restarting the daemon"));
        Some(Effect::Restart)
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
        if let Some(regenerated) = &saved.regenerated {
            self.notices.push(Notice::new(
                Tone::Warn,
                format!(
                    "config.toml {}, so it was written from scratch; the old file is {}",
                    regenerated.reason,
                    tilde(&regenerated.backup, self.env.get("HOME")).display()
                ),
            ));
        }
        self.config.saved(saved.loaded);
        self.restart_by_key = false;
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
        let by_key = std::mem::take(&mut self.restart_by_key);
        let notice = match report {
            // Another dialog is up (unsaved changes, a change on disk): it
            // stays, and the question is a line instead.
            RestartReport::NotAnswering if by_key && !self.quitting && self.dialog.is_some() => {
                Notice::new(
                    Tone::Warn,
                    "restart skipped: daemon is not answering; r asks to restart it anyway",
                )
            }
            RestartReport::NotAnswering if by_key && !self.quitting => {
                self.dialog = Some(Dialog::RestartAnyway);
                self.status = None;
                return None;
            }
            RestartReport::Restarted => Notice::new(Tone::Ok, "daemon restarted"),
            // After a save, silence may be a dictation starting.
            RestartReport::Busy(_) | RestartReport::NotAnswering => {
                let doing = match report {
                    RestartReport::Busy(doing) => doing,
                    _ => "not answering",
                };
                if self.quitting {
                    Notice::new(
                        Tone::Warn,
                        format!(
                            "Restart skipped: the daemon is {doing}. Run whisrs restart when \
                             it is done."
                        ),
                    )
                } else {
                    Notice::new(
                        Tone::Warn,
                        format!("restart skipped: daemon is {doing}, press r later"),
                    )
                }
            }
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
            // The box's border and padding off.
            let width = usize::from(area.width.saturating_sub(4));
            let title = match dialog {
                Dialog::Help(HelpFor::Rows) => "keys",
                Dialog::Help(HelpFor::Picker) => "picker keys",
                Dialog::Help(HelpFor::Hotkey) => "hotkey keys",
                _ => "",
            };
            dialog::draw_titled(f, area, ui, title, dialog_lines(dialog, ui, width));
        }
    }
}

/// `r` (or `y` to restart anyway) while config.toml does not load.
fn not_restarted_while_broken() -> Notice {
    Notice::new(
        Tone::Error,
        "not restarted: config.toml does not load, and the daemon would start on defaults. e \
         opens it to fix it.",
    )
}

/// What a dialog says, `width` columns at most: the help wraps its lines,
/// under their text.
fn dialog_lines(dialog: &Dialog, ui: &Ui, width: usize) -> Vec<Line<'static>> {
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
        Dialog::RestartAnyway => vec![
            Line::from("The daemon is not answering, so it may be busy."),
            Line::from("Restart it anyway?"),
            Line::default(),
            keys(&[("y", "restart"), ("n", "no")]),
        ],
        Dialog::Changed(file) => vec![
            Line::from(format!("{file} changed on disk since you opened it.")),
            Line::default(),
            keys(&[
                ("o", "overwrite"),
                ("l", "load it (drops your edits)"),
                ("esc", "cancel"),
            ]),
        ],
        Dialog::Help(help) => {
            const KEY: usize = 10;
            let rows: &[(&str, &str)] = match help {
                HelpFor::Rows => &[
                    ("j k", "move, g G first and last"),
                    ("h l", "change a value, step a number; else panes"),
                    ("H L", "a number by its big step"),
                    ("enter", "pick from a list, or type a value"),
                    ("space", "flip a toggle, or the next value"),
                    ("x", "back to the default"),
                    ("a d", "add or delete, in a list"),
                    ("/", "filter the rows; esc clears it"),
                    ("tab", "the other pane"),
                    ("w", "save, then restart the daemon if idle"),
                    ("r", "restart the daemon"),
                    ("e", "edit config.toml in $EDITOR"),
                    ("esc", "back one level, or cancel"),
                    ("q", "quit, asking first about unsaved edits"),
                    ("?", "in a picker or a hotkey dialog: its keys"),
                ],
                HelpFor::Picker => &[
                    (
                        "type",
                        "filter: each word starts a word of a value or of what it is",
                    ),
                    ("up down", "move; ctrl+p ctrl+n, pgup pgdn, home end too"),
                    ("enter", "pick; a dimmed value says why it cannot be used"),
                    ("use", "a typed value no list has comes last, checked"),
                    ("backspace", "erase; ctrl+u clears the filter"),
                    ("tab", "complete a typed path, on a path row"),
                    ("esc", "leave with nothing changed"),
                ],
                HelpFor::Hotkey => &[
                    ("tab j k", "the next or the previous part"),
                    ("space h l", "flip a modifier, or step the key"),
                    ("enter", "on the key: pick it from a list; else save"),
                    ("t", "type the hotkey instead"),
                    ("x", "unset it"),
                    ("esc", "cancel"),
                    (
                        "",
                        "Only ScrollLock, Pause and F13 to F24 go without a modifier.",
                    ),
                ],
            };
            let mut lines = Vec::new();
            for (key, what) in rows {
                let wrapped = crate::tui::widgets::wrap(what, width.saturating_sub(KEY).max(1));
                for (i, part) in wrapped.into_iter().enumerate() {
                    let key = if i == 0 { *key } else { "" };
                    lines.push(Line::from(vec![
                        Span::styled(format!("{key:<KEY$}"), ui.theme.accent),
                        Span::raw(part),
                    ]));
                }
            }
            lines
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
        let saved = save(&fixture.app.config, &paths, false);
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
        assert_eq!(
            fixture
                .app
                .update(Msg::Saved(Err(SaveError::Failed("invalid".into())))),
            None
        );
        assert!(!fixture.app.quit);
        assert!(fixture.app.config.form.is_dirty());
        // The next save is a plain one.
        assert_eq!(press(&mut fixture.app, "w"), [Effect::Save]);
        let saved = save(&fixture.app.config, &paths(fixture.dir.path()), false);
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
        let saved = save(&fixture.app.config, &paths(fixture.dir.path()), false);
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

    /// `r` on a daemon that does not answer asks before restarting it, no
    /// by default; after a save, silence just skips the restart.
    #[test]
    fn a_daemon_that_does_not_answer_is_restarted_only_when_asked() {
        let mut fixture = app("");
        assert_eq!(press(&mut fixture.app, "r"), [Effect::Restart]);
        fixture
            .app
            .update(Msg::Restarted(RestartReport::NotAnswering));
        assert_eq!(fixture.app.dialog, Some(Dialog::RestartAnyway));
        let drawn = crate::tui::testing::rows(&crate::tui::testing::draw(&fixture.app, 90, 30));
        assert!(drawn.iter().any(|r| r.contains("Restart it anyway?")));
        // Anything but y is no.
        assert!(press(&mut fixture.app, "\n").is_empty());
        assert_eq!(fixture.app.dialog, None);
        assert!(fixture
            .app
            .status
            .as_ref()
            .unwrap()
            .text
            .contains("not answering"));

        press(&mut fixture.app, "r");
        fixture
            .app
            .update(Msg::Restarted(RestartReport::NotAnswering));
        assert_eq!(press(&mut fixture.app, "y"), [Effect::RestartAnyway]);

        // A save's restart does not ask.
        let mut fixture = edited();
        press(&mut fixture.app, "w");
        let saved = save(&fixture.app.config, &paths(fixture.dir.path()), false);
        assert_eq!(fixture.app.update(Msg::Saved(saved)), Some(Effect::Restart));
        fixture
            .app
            .update(Msg::Restarted(RestartReport::NotAnswering));
        assert_eq!(fixture.app.dialog, None);
        assert_eq!(
            fixture.app.status.as_ref().unwrap().text,
            "restart skipped: daemon is not answering, press r later"
        );
    }

    /// A not-answering that comes back after another dialog opened leaves
    /// that dialog up, and says it as a line.
    #[test]
    fn a_late_not_answering_does_not_replace_a_dialog() {
        let mut fixture = edited();
        assert_eq!(press(&mut fixture.app, "r"), [Effect::Restart]);
        press(&mut fixture.app, "q");
        assert_eq!(fixture.app.dialog, Some(Dialog::Unsaved(After::Quit)));
        fixture
            .app
            .update(Msg::Restarted(RestartReport::NotAnswering));
        assert_eq!(fixture.app.dialog, Some(Dialog::Unsaved(After::Quit)));
        assert!(fixture
            .app
            .status
            .as_ref()
            .unwrap()
            .text
            .contains("r asks to restart it anyway"));
    }

    /// `y` checks again that config.toml loads: it may have broken while
    /// the daemon was asked.
    #[test]
    fn restart_anyway_is_refused_while_the_file_does_not_load() {
        let mut fixture = app("");
        press(&mut fixture.app, "r");
        fixture
            .app
            .update(Msg::Restarted(RestartReport::NotAnswering));
        fixture.app.config.broken = Some("TOML parse error".into());
        assert!(press(&mut fixture.app, "y").is_empty());
        assert!(fixture
            .app
            .status
            .as_ref()
            .unwrap()
            .text
            .starts_with("not restarted"));
    }

    /// Saved on the way out with the daemon busy, the farewell says what to
    /// run: there is no r once the TUI is gone.
    #[test]
    fn a_skipped_restart_on_the_way_out_says_to_run_whisrs_restart() {
        let mut fixture = edited();
        press(&mut fixture.app, "q");
        press(&mut fixture.app, "w");
        let saved = save(&fixture.app.config, &paths(fixture.dir.path()), false);
        fixture.app.update(Msg::Saved(saved));
        assert_eq!(
            fixture
                .app
                .update(Msg::Restarted(RestartReport::Busy("recording"))),
            Some(Effect::Quit)
        );
        assert_eq!(
            fixture.app.farewell[1],
            "Restart skipped: the daemon is recording. Run whisrs restart when it is done."
        );
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
        let mut fixture = crate::tui::testing::app_on(None, &[]);
        let paths = paths(fixture.dir.path());
        assert!(fixture.app.config.fresh);
        // groq, the default backend, needs its key to validate.
        press(&mut fixture.app, "j\n");
        press(&mut fixture.app, "jjj\n");
        crate::tui::testing::type_text(&mut fixture.app, "gsk_1");
        press(&mut fixture.app, "\n");
        let saved = save(&fixture.app.config, &paths, false);
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
