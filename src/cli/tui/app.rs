//! The TUI's state and its one update function: a message in (a key, a
//! worker's answer), the state changed, and at most one [`Effect`] out for
//! the loop to run. Nothing here touches the terminal, the disk or the
//! daemon, so key sequences are tested without any of them.

use std::time::Instant;

use ratatui::crossterm::event::KeyEvent;
use ratatui::text::{Line, Span};
use ratatui::Frame;
use whisrs::config::setup::key_test::{Target, Tested};
use whisrs::config::setup::mic::{Probe, ProbeError};
use whisrs::service::ServiceManager;
use whisrs::OverlayConfig;

use super::keys::{self, Action, Mode};
use super::options::Listing;
use super::schema::{self, ListKind};
use super::screens::config::{ConfigScreen, Found, Loaded, SaveError, Saved, Test};
use super::screens::history::{Dictation, History};
use super::screens::mic::analysis::MicReading;
use super::screens::mic::{self, Meter};
use super::screens::recordings::{self, Act, Detail, Item, Recordings, Wanted};
use super::screens::setup::{self, Checks, Existing, Reply, Setup, SystemAction};
use super::screens::{config, history, home};
use super::widgets::{dialog, frame, Notice, Tone, Ui};
use super::worker::{DaemonView, Download, RestartReport};
use super::{tilde, Entry, Env, Paths};

/// A screen of the stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Home,
    Config,
    Setup,
    Mic,
    Recordings,
    History,
}

/// Which screen asked for a key test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Config,
    Setup,
}

/// A capture's news, from the Mic worker.
#[derive(Debug, Clone, PartialEq)]
pub enum MicEvent {
    /// What opening the device for a look found.
    Probed(Result<Probe, ProbeError>),
    Reading(MicReading, Instant),
    /// The capture did not start, or stopped, and why.
    Failed(String),
}

/// What [`Effect::OpenSetup`] read: the config on disk, and the answers'
/// starting point.
pub struct SetupLoad {
    pub found: Found,
    pub disk: Loaded,
    pub answers: Loaded,
}

/// Read what setup opens on: an unreadable config is refused (#134).
pub fn setup_load(paths: &Paths) -> Result<SetupLoad, String> {
    let (disk, found) = config::load_for_setup(paths)?;
    let answers = config::loaded_from(paths, Setup::defaults())?;
    Ok(SetupLoad {
        found,
        disk,
        answers,
    })
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
    /// A key test came back.
    KeyTested {
        origin: Origin,
        path: String,
        tested: Tested,
    },
    Download(Download),
    Mic {
        session: u64,
        event: MicEvent,
    },
    /// Setup's look at the machine.
    Checked(Box<Checks>),
    /// One of setup's system steps ran: what to say, or why it failed.
    SystemDone(SystemAction, Result<Vec<String>, String>),
    /// The uinput fix ran: the commands that failed, and why.
    Fixed(Vec<String>),
    /// Setup can open, or why not.
    SetupReady(Result<SetupLoad, String>),
    /// The answer to [`Effect::SaveDevice`].
    DeviceSaved(Result<Saved, SaveError>),
    /// The answer to [`Effect::WriteSetup`], and the file read once more
    /// for setup to edit on from.
    SetupWritten(Result<Saved, SaveError>, Option<Loaded>),
    /// Read aloud's sample `id` ended: said, or why not.
    Spoke {
        id: u64,
        said: Result<(), String>,
    },
    /// A spinner's next frame.
    Tick,
    /// The recordings folder, read.
    Recordings(Result<Vec<Item>, String>),
    /// One recording read whole, at the stamp it was asked at.
    RecordingRead {
        id: String,
        stamp: Option<std::time::SystemTime>,
        detail: Result<Detail, String>,
    },
    /// The daemon's answer to [`Effect::Recording`]: done, or why not.
    Acted {
        act: Act,
        answer: Result<(), String>,
    },
    /// The dictation history, read.
    HistoryRead(Result<Vec<Dictation>, String>),
    /// The daemon's answer to [`Effect::ClearHistory`]: done, or why not.
    HistoryCleared(Result<(), String>),
}

/// What the loop runs for the app.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Test a key on a worker, in the config of `origin`; answered with
    /// [`Msg::KeyTested`].
    TestKey {
        origin: Origin,
        path: String,
        target: Target,
    },
    /// Download a whisper model; answered with [`Msg::Download`].
    Download {
        name: String,
        id: u64,
    },
    /// Stop download `id`.
    CancelDownload(u64),
    /// Write `[audio] device` alone onto config.toml (Mic's `w`); answered
    /// with [`Msg::DeviceSaved`].
    SaveDevice {
        overwrite: bool,
    },
    /// Capture from a microphone for the meter; answered with [`Msg::Mic`].
    MicStart {
        session: u64,
        device: String,
    },
    MicStop,
    /// Read the config for setup; answered with [`Msg::SetupReady`].
    OpenSetup,
    /// Validate and write setup's answers; answered with
    /// [`Msg::SetupWritten`].
    WriteSetup {
        overwrite: bool,
    },
    /// Look at the machine for setup; answered with [`Msg::Checked`].
    Checks,
    /// Run one of setup's system steps; answered with [`Msg::SystemDone`].
    System(SystemAction),
    /// Run the uinput fix with sudo, in the plain terminal; answered with
    /// [`Msg::Fixed`].
    FixUinput,
    /// Say read aloud's sample with setup's `[tts]` as sample `id`;
    /// answered with [`Msg::Spoke`].
    Speak {
        id: u64,
    },
    /// Stop the sample being said.
    StopSpeaking,
    /// Read the recordings folder now and every 2 s while Recordings is
    /// open; answered with [`Msg::Recordings`] when it changed.
    WatchRecordings,
    /// Read it again now: an action was answered.
    RefreshRecordings,
    StopRecordings,
    /// Read one recording whole; answered with [`Msg::RecordingRead`].
    ReadRecording(Wanted),
    /// Ask the daemon to act on a recording; answered with [`Msg::Acted`].
    Recording(Act),
    /// Read the dictation history; answered with [`Msg::HistoryRead`].
    ReadHistory,
    /// Ask the daemon to clear it; answered with [`Msg::HistoryCleared`].
    ClearHistory,
    /// Each in turn.
    All(Vec<Effect>),
}

/// What the unsaved-changes dialog was opened for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum After {
    Quit,
    Editor,
    /// Open Setup, which writes config.toml itself.
    Setup,
}

/// Whose keys the help lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpFor {
    Rows,
    Picker,
    Hotkey,
    Setup,
    Mic,
    Recordings,
    History,
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
    /// Setup's answers are not written: leave anyway (and quit)?
    Unwritten {
        quit: bool,
    },
    /// Append this keybinding to this file?
    Bind {
        line: String,
        path: String,
    },
    /// Setup's write found this file changed since setup opened.
    SetupChanged(&'static str),
    /// Mic's write found this file changed since it was read.
    DeviceChanged(&'static str),
    /// Delete this recording for good? Keep by default.
    DeleteRecording {
        act: Act,
        question: String,
    },
    /// Transcribe this transcribed recording again? Keep by default.
    RetryRecording {
        act: Act,
        lines: Vec<String>,
    },
    /// Clear the whole dictation history, of this many dictations (`None`:
    /// the file did not read)? Keep by default.
    ClearHistory {
        count: Option<usize>,
    },
}

pub struct App {
    pub env: Env,
    pub paths: Paths,
    pub service: ServiceManager,
    entry: Entry,
    stack: Vec<Screen>,
    pub home: home::Home,
    pub config: ConfigScreen,
    /// The wizard, while it is open.
    pub setup: Option<Setup>,
    /// The microphone meter of the Mic screen and setup's mic step.
    pub mic: Meter,
    pub recordings: Recordings,
    pub history: History,
    /// The last capture started.
    mic_session: u64,
    /// Spinner frames so far.
    pub ticks: usize,
    /// Downloads stopped on screen whose worker may still be writing:
    /// their id and `.part`, removed on the way out if still there.
    pub orphans: Vec<(u64, std::path::PathBuf)>,
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
        match entry {
            Entry::Config => {
                let loaded = config::load(&paths)?;
                Ok(App::new(entry, loaded, paths, env, service))
            }
            Entry::Setup => {
                let load = setup_load(&paths)?;
                let mut app = App::new(entry, load.disk, paths, env, service);
                // Its first step, a choice or the backend, starts nothing.
                let _ = app.start_setup(load.found, None, load.answers);
                Ok(app)
            }
        }
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
            Entry::Setup => vec![Screen::Home],
        };
        let notices = loaded.notices.clone();
        let mut home = home::Home::default();
        home.point(Screen::Config);
        App {
            config: ConfigScreen::new(loaded, env.clone()),
            env,
            paths,
            service,
            entry,
            stack,
            home,
            setup: None,
            mic: Meter::default(),
            recordings: Recordings::new(cfg!(feature = "recording")),
            history: History::default(),
            mic_session: 0,
            ticks: 0,
            orphans: Vec::new(),
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

    /// The overlay theme as edited, which colours the TUI too: setup's
    /// answer while setup is open, so the accent follows it live.
    pub fn overlay(&self) -> OverlayConfig {
        let form = match (&self.setup, self.screen()) {
            (Some(setup), Screen::Setup) => &setup.editor.form,
            _ => &self.config.form,
        };
        form.config().overlay.clone().unwrap_or_default()
    }

    /// How often the screen moves on its own: the mic meter about 30 times
    /// a second, a download or a spinner about 10; `None` when it is still.
    pub fn frame(&self) -> Option<std::time::Duration> {
        let mic = match (self.screen(), &self.setup) {
            (Screen::Mic, _) => true,
            (Screen::Setup, Some(setup)) => setup.step == setup::Step::Mic,
            _ => false,
        };
        let busy = self.spinning()
            || self
                .setup
                .as_ref()
                .is_some_and(|setup| setup.download.is_some());
        if mic {
            Some(std::time::Duration::from_millis(33))
        } else if busy {
            Some(std::time::Duration::from_millis(100))
        } else {
            None
        }
    }

    /// Whether a spinner turns: a key test or a look at the machine runs.
    pub fn spinning(&self) -> bool {
        self.config.tests.values().any(|t| *t == Test::Running)
            || self.setup.as_ref().is_some_and(Setup::spinning)
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
                if let Some(setup) = &mut self.setup {
                    setup.editor.listed(listing.clone());
                }
                self.config.listed(listing);
                // It came: a row that was looking for it is not any more.
                if self.status.as_ref().is_some_and(|s| s.text == "looking...") {
                    self.status = None;
                }
                None
            }
            Msg::Reclaim => Some(Effect::Reclaim),
            Msg::KeyTested {
                origin,
                path,
                tested,
            } => {
                match (origin, &mut self.setup) {
                    (Origin::Setup, Some(setup)) => self.status = Some(setup.tested(tested)),
                    (Origin::Setup, None) => {}
                    (Origin::Config, _) => {
                        let label = schema::find(&path).map_or("key", |field| field.label);
                        self.status = Some(config::test_notice(label, &tested));
                        self.config.tests.insert(path, Test::Done(tested));
                    }
                }
                None
            }
            Msg::Download(news) => {
                // An orphan's worker is done: it removed its `.part`.
                if let Download::Done { id, .. } = &news {
                    self.orphans.retain(|(orphan, _)| orphan != id);
                }
                let reply = self.setup.as_mut()?.downloaded(news);
                self.reply(reply)
            }
            Msg::DeviceSaved(saved) => self.device_saved(saved),
            Msg::Mic { session, event } => {
                self.mic.event(session, event);
                None
            }
            Msg::Checked(checks) => {
                if let Some(setup) = &mut self.setup {
                    setup.checked(*checks);
                }
                None
            }
            Msg::SystemDone(action, done) => {
                let restart = done.is_ok()
                    && action == SystemAction::Repoint
                    && self.daemon_up() == Some(true);
                self.status = Some(match done {
                    Ok(notes) => Notice::new(Tone::Ok, notes.join("; ")),
                    Err(why) => Notice::new(Tone::Error, why),
                });
                self.setup.as_ref()?;
                Some(if restart {
                    Effect::All(vec![Effect::Checks, Effect::Restart])
                } else {
                    Effect::Checks
                })
            }
            Msg::Fixed(failed) => {
                self.status = Some(if failed.is_empty() {
                    Notice::new(
                        Tone::Ok,
                        "installed the udev rule and joined the input group; log out and back \
                         in if uinput still says denied",
                    )
                } else {
                    Notice::new(
                        Tone::Error,
                        format!("not all of it worked: {}", failed.join("; ")),
                    )
                });
                self.setup.as_mut()?.fixed();
                Some(Effect::Checks)
            }
            Msg::SetupReady(Ok(load)) => {
                self.start_setup(load.found, Some(load.disk), load.answers)
            }
            Msg::SetupReady(Err(why)) => {
                self.status = Some(Notice::new(Tone::Error, why));
                None
            }
            Msg::SetupWritten(written, again) => self.setup_written(written, again),
            Msg::Spoke { id, said } => {
                if let Some(notice) = self.setup.as_mut()?.spoke(id, said) {
                    self.status = Some(notice);
                }
                None
            }
            Msg::Tick => {
                self.ticks += 1;
                self.config.spin = self.ticks;
                if let Some(setup) = &mut self.setup {
                    setup.spin = self.ticks;
                    setup.editor.spin = self.ticks;
                }
                None
            }
            Msg::Recordings(folder) => {
                let wanted = self.recordings.read(folder);
                if self.screen() == Screen::Recordings {
                    wanted.map(Effect::ReadRecording)
                } else {
                    None
                }
            }
            Msg::RecordingRead { id, stamp, detail } => {
                self.recordings.detail_read(id, stamp, detail);
                None
            }
            Msg::Acted { act, answer } => {
                self.status = Some(match &answer {
                    Ok(()) => Notice::new(Tone::Ok, act.done()),
                    Err(why) => Notice::new(Tone::Error, why.clone()),
                });
                let wanted = self.recordings.acted(&act, answer.is_ok());
                if self.screen() != Screen::Recordings {
                    return None;
                }
                Some(match wanted {
                    Some(wanted) => Effect::All(vec![
                        Effect::RefreshRecordings,
                        Effect::ReadRecording(wanted),
                    ]),
                    None => Effect::RefreshRecordings,
                })
            }
            Msg::HistoryRead(entries) => {
                self.history.read(entries);
                None
            }
            Msg::HistoryCleared(answer) => {
                self.history.clearing = false;
                let cleared = answer.is_ok();
                self.status = Some(match answer {
                    Ok(()) => Notice::new(Tone::Ok, "history cleared"),
                    Err(why) => Notice::new(Tone::Error, why),
                });
                (cleared && self.screen() == Screen::History).then_some(Effect::ReadHistory)
            }
        }
    }

    /// Open setup on what [`setup_load`] found. `disk` is the config on
    /// disk when the Config screen already holds one (setup opened from
    /// Home); `None` gives it to the Config screen.
    fn start_setup(
        &mut self,
        found: Found,
        disk: Option<Loaded>,
        answers: Loaded,
    ) -> Option<Effect> {
        let disk_form = disk.as_ref().map_or(&self.config.form, |d| &d.form);
        let existing = match &found {
            Found::Usable => Some(Existing {
                backend: disk_form.config().general.backend.clone(),
                config: disk_form.config().clone(),
            }),
            Found::Missing | Found::Broken(_) => None,
        };
        let broken = match found {
            Found::Broken(message) => {
                self.notices.push(Notice::new(
                    Tone::Warn,
                    whisrs::config::write::undeserializable_config_setup_warning(
                        &message,
                        &self.paths.config,
                    )
                    .replace('\n', " "),
                ));
                if disk.is_none() {
                    // Config edits nothing over a file that does not load.
                    self.config.broken =
                        Some(message.lines().next().unwrap_or_default().to_string());
                }
                Some(message)
            }
            _ => None,
        };
        let mut setup = Setup::new(
            answers,
            existing,
            broken,
            self.env.clone(),
            self.paths.models.clone(),
            self.paths.config.clone(),
        );
        let reply = setup.opened();
        self.setup = Some(setup);
        self.stack.push(Screen::Setup);
        self.reply(reply)
    }

    /// The answer to setup's write.
    fn setup_written(
        &mut self,
        written: Result<Saved, SaveError>,
        again: Option<Loaded>,
    ) -> Option<Effect> {
        match written {
            Ok(saved) => {
                let file = tilde(&saved.path, self.env.get("HOME"))
                    .display()
                    .to_string();
                self.notices = saved.loaded.notices.clone();
                self.notices.extend(saved.warnings.iter().map(|w| {
                    Notice::new(Tone::Warn, format!("warning: {}", w.replace('\n', " ")))
                }));
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
                self.config.reloaded(saved.loaded);
                let kept = saved
                    .backup
                    .as_ref()
                    .map(|bak| {
                        format!(
                            "; the old file is {}",
                            tilde(bak, self.env.get("HOME")).display()
                        )
                    })
                    .unwrap_or_default();
                let mut reply = self.setup.as_mut()?.written(again);
                reply.status = Some(Notice::new(Tone::Ok, format!("wrote {file}{kept}")));
                let effect = self.reply(reply);
                if self.daemon_up() == Some(true) {
                    // The daemon runs the old config until it restarts.
                    return Some(Effect::All(
                        effect.into_iter().chain([Effect::Restart]).collect(),
                    ));
                }
                effect
            }
            Err(SaveError::Failed(why)) => {
                self.status = Some(Notice::new(Tone::Error, format!("not written: {why}")));
                None
            }
            Err(SaveError::Changed(file)) => {
                self.dialog = Some(Dialog::SetupChanged(file));
                None
            }
            Err(SaveError::Reread(why)) => {
                let mut reply = self.setup.as_mut()?.written(again);
                reply.status = Some(Notice::new(
                    Tone::Warn,
                    format!("written, but reading it back failed: {why}"),
                ));
                self.reply(reply)
            }
        }
    }

    /// Do what setup's reply asks. A step change clears what the step
    /// before said, unless it says something itself.
    fn reply(&mut self, reply: Reply) -> Option<Effect> {
        if let Some(status) = reply.status {
            self.status = Some(status);
        } else if reply.moved {
            self.status = None;
        }
        if reply.flash.is_some() {
            self.flash = reply.flash;
        }
        if reply.dialog.is_some() {
            self.dialog = reply.dialog;
        }
        if let Some(orphan) = reply.orphan {
            self.orphans.push(orphan);
        }
        let mut effects = reply.effects;
        match reply.mic {
            Some(Some(device)) => effects.push(self.start_mic(device)),
            Some(None) => effects.push(Effect::MicStop),
            None => {}
        }
        if reply.leave {
            if self.setup.as_ref().is_some_and(Setup::unwritten) {
                self.dialog = Some(Dialog::Unwritten { quit: false });
            } else {
                effects.extend(self.leave_setup());
            }
        }
        if reply.finish {
            effects.extend(self.finish_setup());
        }
        match effects.len() {
            0 => None,
            1 => effects.pop(),
            _ => Some(Effect::All(effects)),
        }
    }

    /// A new capture of `device`, for the meter.
    fn start_mic(&mut self, device: String) -> Effect {
        self.mic_session += 1;
        self.mic.start(self.mic_session);
        Effect::MicStart {
            session: self.mic_session,
            device,
        }
    }

    /// Close setup, back to Home, stopping what it started.
    fn leave_setup(&mut self) -> Option<Effect> {
        let setup = self.setup.take()?;
        if self.screen() == Screen::Setup {
            self.stack.pop();
            self.home.point(Screen::Setup);
        }
        let mut effects = Vec::new();
        if setup.step == setup::Step::Mic {
            effects.push(Effect::MicStop);
        }
        if matches!(setup.sample, Some(setup::Sample::Playing { .. })) {
            effects.push(Effect::StopSpeaking);
        }
        if let Some((id, part)) = setup.partial() {
            self.orphans.push((id, part));
            effects.push(Effect::CancelDownload(id));
        }
        match effects.len() {
            0 => None,
            1 => effects.pop(),
            _ => Some(Effect::All(effects)),
        }
    }

    /// The last step is done: `whisrs setup` ends, with what to know; from
    /// Home, Home again.
    fn finish_setup(&mut self) -> Option<Effect> {
        let effect = self.leave_setup();
        if self.entry != Entry::Setup {
            self.status = Some(Notice::new(Tone::Ok, "setup is done"));
            return effect;
        }
        let logs = match self.service {
            ServiceManager::OpenRc => "${XDG_STATE_HOME:-~/.local/state}/whisrs/whisrsd.log",
            _ => "journalctl --user -u whisrs -f",
        };
        self.farewell.extend([
            "You're all set.".to_string(),
            format!(
                "  Config:  {}",
                tilde(&self.paths.config, self.env.get("HOME")).display()
            ),
            format!("  Logs:    {logs}"),
            "  Change a setting with whisrs config; run whisrs setup again for the rest."
                .to_string(),
        ]);
        Some(Effect::Quit)
    }

    /// Open the Mic screen: the device row, and the capture.
    fn open_mic(&mut self) -> Option<Effect> {
        self.config.pin(vec![schema::row("audio.device")]);
        self.stack.push(Screen::Mic);
        let device = self.config.form.config().audio.device.clone();
        let mut effects = Vec::new();
        if self.config.lists.ask(ListKind::Devices) {
            effects.push(Effect::List(ListKind::Devices));
        }
        effects.push(self.start_mic(device));
        Some(Effect::All(effects))
    }

    fn leave_mic(&mut self) -> Option<Effect> {
        self.config.unpin();
        self.stack.pop();
        self.home.point(Screen::Mic);
        Some(Effect::MicStop)
    }

    /// Open Recordings: the folder is read now and then every 2 s, unless
    /// this whisrs has no recordings to read.
    fn open_recordings(&mut self) -> Option<Effect> {
        self.stack.push(Screen::Recordings);
        self.status = None;
        self.recordings.opened();
        self.recordings.built.then_some(Effect::WatchRecordings)
    }

    fn open_history(&mut self) -> Option<Effect> {
        self.stack.push(Screen::History);
        self.status = None;
        self.history.opened();
        Some(Effect::ReadHistory)
    }

    /// `[general] history` as saved, which is what the daemon runs on: is
    /// a dictation added to the history?
    pub fn history_kept(&self) -> bool {
        self.config
            .form
            .saved("general.history")
            .and_then(toml::Value::as_bool)
            .unwrap_or(true)
    }

    /// `C` on History: ask before the daemon clears it all, unless there is
    /// nothing to clear or nothing to clear it.
    fn ask_clear_history(&mut self) -> Option<Effect> {
        let count = match &self.history.entries {
            None => return None,
            Some(Ok(entries)) if entries.is_empty() => {
                self.status = Some(Notice::new(Tone::Muted, "the history is empty"));
                return None;
            }
            Some(Ok(entries)) => Some(entries.len()),
            Some(Err(_)) => None,
        };
        match self.clear_refusal() {
            Some(notice) => self.status = Some(notice),
            None => self.dialog = Some(Dialog::ClearHistory { count }),
        }
        None
    }

    /// Why the history is not cleared now, if it is not: the daemon clears
    /// it, as `whisrs log --clear` has it do.
    fn clear_refusal(&self) -> Option<Notice> {
        if let Some(down) = self.down("clear the history") {
            return Some(down);
        }
        self.history.clearing.then(|| {
            Notice::new(
                Tone::Warn,
                "still clearing the history: wait for whisrsd's answer",
            )
        })
    }

    /// Take what a key on Recordings asked for.
    fn recordings_reply(&mut self, reply: recordings::Reply) -> Option<Effect> {
        if let Some(status) = reply.status {
            self.status = Some(status);
        }
        let mut effects = Vec::new();
        if let Some(wanted) = reply.read {
            effects.push(Effect::ReadRecording(wanted));
        }
        if let Some(dialog) = reply.dialog {
            // Nothing to ask about while the daemon that would do it is
            // down, or still at an action on the same recording.
            let refused = match &dialog {
                Dialog::DeleteRecording { act, .. } | Dialog::RetryRecording { act, .. } => {
                    self.refusal(act)
                }
                _ => None,
            };
            match refused {
                Some(notice) => self.status = Some(notice),
                None => self.dialog = Some(dialog),
            }
        }
        if let Some(act) = reply.act {
            effects.extend(self.act(act));
        }
        if reply.leave {
            self.stack.pop();
            self.home.point(Screen::Recordings);
            effects.push(Effect::StopRecordings);
        }
        match effects.len() {
            0 => None,
            1 => effects.pop(),
            _ => Some(Effect::All(effects)),
        }
    }

    /// Send `act` to the daemon, unless it is known to be down (then say
    /// so, with the command that starts it, as Home does) or has not
    /// answered an action on that recording yet.
    fn act(&mut self, act: Act) -> Option<Effect> {
        if let Some(refusal) = self.refusal(&act) {
            self.status = Some(refusal);
            return None;
        }
        self.status = Some(Notice::new(Tone::Muted, act.asking()));
        self.recordings.sent(&act.id);
        Some(Effect::Recording(act))
    }

    /// Why `act` is not sent now, if it is not.
    fn refusal(&self, act: &Act) -> Option<Notice> {
        if let Some(down) = self.down(act.verb()) {
            return Some(down);
        }
        self.recordings.is_busy(&act.id).then(|| {
            Notice::new(
                Tone::Warn,
                format!("still working on {}: wait for whisrsd's answer", act.label),
            )
        })
    }

    /// That the daemon cannot `verb` since it is known to be down, with the
    /// command that starts it, as Home says.
    fn down(&self, verb: &str) -> Option<Notice> {
        if self.daemon != DaemonView::Down {
            return None;
        }
        let start = self
            .service
            .enable_hint()
            .unwrap_or(self.service.manual_start_hint());
        Some(Notice::new(
            Tone::Error,
            format!("cannot {verb}: whisrsd is not running. Start it with {start}"),
        ))
    }

    fn key(&mut self, key: KeyEvent) -> Option<Effect> {
        self.flash = None;
        // A sample being said stops at any key, which does nothing else.
        if self.dialog.is_none() && self.screen() == Screen::Setup {
            if let Some(reply) = self.setup.as_mut().and_then(Setup::stop_sample) {
                return self.reply(reply);
            }
        }
        let mode = if self.dialog.is_some() {
            Mode::Dialog
        } else {
            match self.screen() {
                Screen::Config | Screen::Mic => self.config.mode(),
                Screen::Setup => self.setup.as_ref().map_or(Mode::Browse, Setup::mode),
                Screen::Home => Mode::Browse,
                Screen::Recordings => self.recordings.mode(),
                Screen::History => self.history.mode(),
            }
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
        if mode == Mode::Read && action == Action::Help {
            self.dialog = Some(Dialog::Help(match self.screen() {
                Screen::History => HelpFor::History,
                _ => HelpFor::Recordings,
            }));
            return None;
        }
        if mode == Mode::Browse {
            let setup = self.screen() == Screen::Setup;
            match action {
                Action::Help => {
                    self.dialog = Some(Dialog::Help(match self.screen() {
                        Screen::Setup => HelpFor::Setup,
                        Screen::Mic => HelpFor::Mic,
                        _ => HelpFor::Rows,
                    }));
                    return None;
                }
                Action::Save if self.screen() == Screen::Mic => return self.save_device(),
                Action::Save if !setup => return self.save(),
                Action::Editor if !setup => return self.editor(),
                Action::Restart if !setup => return self.restart(),
                _ => {}
            }
        }
        match self.screen() {
            Screen::Home => match self.home.key(action)? {
                Screen::Config => {
                    self.stack.push(Screen::Config);
                    None
                }
                Screen::Mic => self.open_mic(),
                Screen::Recordings => self.open_recordings(),
                Screen::History => self.open_history(),
                // Setup writes config.toml itself: Config's edits first.
                Screen::Setup if self.config.form.is_dirty() => {
                    self.dialog = Some(Dialog::Unsaved(After::Setup));
                    None
                }
                Screen::Setup => Some(Effect::OpenSetup),
                Screen::Home => None,
            },
            Screen::Config => {
                let outcome = self.config.key(action);
                if outcome.leave {
                    self.stack.pop();
                    self.home.point(Screen::Config);
                }
                self.outcome(outcome, Origin::Config)
            }
            Screen::Mic => {
                if mode == Mode::Browse && action == Action::Back {
                    return self.leave_mic();
                }
                let device = self.config.form.config().audio.device.clone();
                let outcome = self.config.key(action);
                let effect = self.outcome(outcome, Origin::Config);
                let now = self.config.form.config().audio.device.clone();
                if now == device {
                    return effect;
                }
                let start = self.start_mic(now);
                Some(match effect {
                    Some(effect) => Effect::All(vec![effect, start]),
                    None => start,
                })
            }
            Screen::Setup => {
                let reply = self.setup.as_mut()?.key(action);
                self.reply(reply)
            }
            Screen::Recordings => {
                // The daemon runs on the file as saved, not on unsaved edits.
                let summary_on = self
                    .config
                    .form
                    .saved("recording.summary")
                    .and_then(toml::Value::as_bool)
                    .unwrap_or(false);
                let reply = self.recordings.key(action, summary_on);
                self.recordings_reply(reply)
            }
            Screen::History => match self.history.key(action) {
                history::Asked::Leave => {
                    self.stack.pop();
                    self.home.point(Screen::History);
                    None
                }
                history::Asked::Clear => self.ask_clear_history(),
                history::Asked::Nothing => None,
            },
        }
    }

    /// Take what a key on Config's rows asked for.
    fn outcome(&mut self, outcome: config::Outcome, origin: Origin) -> Option<Effect> {
        if let Some(status) = outcome.status {
            self.status = Some(status);
        }
        self.flash = outcome.flash;
        self.dialog = outcome.dialog;
        if let Some((path, target)) = outcome.test {
            self.config.tests.insert(path.clone(), Test::Running);
            return Some(Effect::TestKey {
                origin,
                path,
                target,
            });
        }
        outcome.load.map(Effect::List)
    }

    fn dialog_key(&mut self, dialog: Dialog, action: Action) -> Option<Effect> {
        match (&dialog, action) {
            (Dialog::Help(_), _) => None,
            (
                Dialog::DeleteRecording { act, .. } | Dialog::RetryRecording { act, .. },
                Action::Type('y'),
            ) => {
                let act = act.clone();
                self.act(act)
            }
            // Keep is the default: anything else.
            (Dialog::DeleteRecording { act, .. }, _) => {
                self.status = Some(Notice::new(Tone::Muted, format!("kept {}", act.label)));
                None
            }
            (Dialog::RetryRecording { act, .. }, _) => {
                self.status = Some(Notice::new(
                    Tone::Muted,
                    format!("{} keeps its transcript", act.label),
                ));
                None
            }
            (Dialog::ClearHistory { .. }, Action::Type('y')) => {
                // The daemon may have stopped while the question was up.
                if let Some(notice) = self.clear_refusal() {
                    self.status = Some(notice);
                    return None;
                }
                self.history.clearing = true;
                self.status = Some(Notice::new(Tone::Muted, "clearing the history..."));
                Some(Effect::ClearHistory)
            }
            (Dialog::ClearHistory { .. }, _) => {
                self.status = Some(Notice::new(Tone::Muted, "kept the history"));
                None
            }
            (Dialog::Unsaved(after), Action::Type('w')) => {
                self.pending = Some(*after);
                self.save()
            }
            (Dialog::Unsaved(after), Action::Type('d')) => {
                self.config.discard();
                Some(match after {
                    After::Quit => Effect::Quit,
                    After::Editor => Effect::OpenEditor,
                    After::Setup => Effect::OpenSetup,
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
            (Dialog::Unwritten { quit }, Action::Type('d')) => {
                let quit = *quit;
                let effect = self.leave_setup();
                if !quit {
                    return effect;
                }
                if self.config.form.is_dirty() {
                    self.dialog = Some(Dialog::Unsaved(After::Quit));
                    return effect;
                }
                Some(match effect {
                    Some(effect) => Effect::All(vec![effect, Effect::Quit]),
                    None => Effect::Quit,
                })
            }
            (Dialog::Bind { .. }, Action::Type('y')) => Some(Effect::System(SystemAction::Bind)),
            (Dialog::Bind { .. }, _) => {
                self.status = Some(Notice::new(Tone::Muted, "not added"));
                None
            }
            (Dialog::SetupChanged(_), Action::Type('o')) => {
                Some(Effect::WriteSetup { overwrite: true })
            }
            (Dialog::DeviceChanged(_), Action::Type('o')) => {
                Some(Effect::SaveDevice { overwrite: true })
            }
            (Dialog::DeviceChanged(_), Action::Back) => {
                self.status = Some(Notice::new(Tone::Muted, "not saved"));
                None
            }
            (Dialog::SetupChanged(_), Action::Back) => {
                self.status = Some(Notice::new(Tone::Muted, "not written"));
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
        let unwritten =
            self.screen() == Screen::Setup && self.setup.as_ref().is_some_and(Setup::unwritten);
        if unwritten && !self.quitting {
            self.dialog = Some(Dialog::Unwritten { quit: true });
            return None;
        }
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
                Some(After::Setup) => Some(Effect::OpenSetup),
                None => None,
            };
        }
        Some(Effect::Save)
    }

    /// Mic's `w`: `[audio] device` alone onto the file as it is on disk;
    /// Config's other edits stay pending.
    fn save_device(&mut self) -> Option<Effect> {
        if let Some(why) = &self.config.broken {
            self.status = Some(Notice::new(
                Tone::Error,
                format!("not saved: config.toml does not load ({why}). e opens it to fix it."),
            ));
            return None;
        }
        if !self.config.form.is_dirty_at("audio.device") {
            self.status = Some(Notice::new(Tone::Muted, "the device is saved already"));
            return None;
        }
        Some(Effect::SaveDevice { overwrite: false })
    }

    /// The answer to Mic's `w`: the device is no edit any more, the rest
    /// still are; then the daemon restarts, if idle.
    fn device_saved(&mut self, saved: Result<Saved, SaveError>) -> Option<Effect> {
        match saved {
            Ok(saved) => {
                let file = tilde(&saved.path, self.env.get("HOME"))
                    .display()
                    .to_string();
                self.notices.extend(
                    saved
                        .warnings
                        .iter()
                        .map(|w| Notice::new(Tone::Warn, format!("warning: {w}"))),
                );
                self.config.settled("audio.device", saved.loaded);
                self.status = Some(Notice::new(
                    Tone::Muted,
                    format!("saved [audio] device to {file}, restarting the daemon"),
                ));
                Some(Effect::Restart)
            }
            Err(SaveError::Failed(why)) => {
                self.status = Some(Notice::new(Tone::Error, format!("not saved: {why}")));
                None
            }
            Err(SaveError::Changed(file)) => {
                self.dialog = Some(Dialog::DeviceChanged(file));
                None
            }
            Err(SaveError::Reread(why)) => {
                self.config.form.settle("audio.device");
                self.status = Some(Notice::new(
                    Tone::Warn,
                    format!("saved, but re-reading it failed: {why}"),
                ));
                None
            }
        }
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
            Some(After::Setup) => {
                self.status = Some(Notice::new(
                    Tone::Muted,
                    format!("saved {file}, restarting the daemon"),
                ));
                Some(Effect::All(vec![Effect::Restart, Effect::OpenSetup]))
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
            Screen::Setup => setup::view(self, f, ui),
            Screen::Mic => mic::view(self, f, ui),
            Screen::Recordings => recordings::view(self, f, ui),
            Screen::History => history::view(self, f, ui),
        }
        if let Some(dialog) = &self.dialog {
            // The box's border and padding off.
            let width = usize::from(area.width.saturating_sub(4));
            let title = match dialog {
                Dialog::Help(HelpFor::Rows) => "keys",
                Dialog::Help(HelpFor::Picker) => "picker keys",
                Dialog::Help(HelpFor::Hotkey) => "hotkey keys",
                Dialog::Help(HelpFor::Setup) => "setup keys",
                Dialog::Help(HelpFor::Mic) => "mic keys",
                Dialog::Help(HelpFor::Recordings) => "recordings keys",
                Dialog::Help(HelpFor::History) => "history keys",
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
    // A question reads best in short lines.
    let question_width = width.clamp(1, 64);
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
        Dialog::Unwritten { .. } => vec![
            Line::from("Your setup answers are not written yet."),
            Line::default(),
            keys(&[("d", "drop them"), ("esc", "stay")]),
        ],
        Dialog::Bind { line, path } => {
            let mut lines = vec![Line::from(format!("Append to {path}:"))];
            for part in crate::tui::widgets::wrap(line, width.saturating_sub(2).max(1)) {
                lines.push(Line::from(Span::styled(
                    format!("  {part}"),
                    ui.theme.accent,
                )));
            }
            lines.push(Line::default());
            lines.push(keys(&[("y", "append"), ("n", "no")]));
            lines
        }
        Dialog::DeviceChanged(file) => vec![
            Line::from(format!("{file} changed on disk since you opened it.")),
            Line::default(),
            keys(&[("o", "write the device over it"), ("esc", "cancel")]),
        ],
        Dialog::SetupChanged(file) => vec![
            Line::from(format!("{file} changed on disk since setup opened.")),
            Line::default(),
            keys(&[("o", "write over it"), ("esc", "cancel")]),
        ],
        Dialog::DeleteRecording { question, .. } => {
            let mut lines: Vec<Line<'static>> = crate::tui::widgets::wrap(question, question_width)
                .into_iter()
                .map(Line::from)
                .collect();
            lines.push(Line::default());
            lines.push(keys(&[("y", "delete"), ("n", "keep")]));
            lines
        }
        Dialog::ClearHistory { count } => {
            let question = match count {
                Some(1) => "Clear the history? Its one dictation goes for good.".to_string(),
                Some(n) => format!("Clear the history? All {n} dictations go for good."),
                None => "Clear the history? Everything in history.jsonl goes for good.".to_string(),
            };
            let mut lines: Vec<Line<'static>> =
                crate::tui::widgets::wrap(&question, question_width)
                    .into_iter()
                    .map(Line::from)
                    .collect();
            lines.push(Line::default());
            lines.push(keys(&[("y", "clear"), ("n", "keep")]));
            lines
        }
        Dialog::RetryRecording { lines: text, .. } => {
            let mut lines = Vec::new();
            for (i, paragraph) in text.iter().enumerate() {
                if i == 1 {
                    lines.push(Line::default());
                }
                lines.extend(
                    crate::tui::widgets::wrap(paragraph, question_width)
                        .into_iter()
                        .map(Line::from),
                );
            }
            lines.push(Line::default());
            lines.push(keys(&[("y", "retry"), ("n", "keep")]));
            lines
        }
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
                    ("t", "test a key row: one request, no audio"),
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
                HelpFor::Setup => &[
                    ("enter", "the next step; on the review, write config.toml"),
                    ("backspace", "the step before"),
                    ("space", "open the row's list or input, or flip a toggle"),
                    ("h l", "change the row in place"),
                    ("j k", "move between the rows"),
                    (
                        "t",
                        "test the key: one request, no audio; on read aloud, say a sample",
                    ),
                    (
                        "s",
                        "skip an optional step, even midway: none of its keys are written",
                    ),
                    ("esc", "leave setup, asking about unwritten answers"),
                    ("q", "quit, asking the same"),
                ],
                HelpFor::Mic => &[
                    ("h l", "the next or the previous device"),
                    ("enter", "pick the device from a list"),
                    (
                        "w",
                        "save it as [audio] device, then restart the daemon if idle",
                    ),
                    ("esc", "back to Home; the capture stops"),
                    ("q", "quit, asking first about unsaved edits"),
                ],
                HelpFor::Recordings => &[
                    ("j k", "move; in a transcript, scroll it"),
                    ("ctrl+d u", "half a page; pgdn pgup a page, g G the ends"),
                    ("enter", "open the transcript; esc back to the list"),
                    (
                        "r",
                        "transcribe again: a failed one at once, a transcribed one asks first",
                    ),
                    ("c", "copy the transcript to the clipboard"),
                    ("n", "name a speaker: pick it, then type the name"),
                    ("S", "summarize it with the [llm]"),
                    ("d", "delete it for good, asking first"),
                    ("/", "filter by title, id, speaker or date"),
                    ("esc", "clear the filter, else back to Home"),
                    ("q", "quit, asking first about unsaved edits"),
                    (
                        "",
                        "Every action is the daemon's: it says why when it cannot.",
                    ),
                ],
                HelpFor::History => &[
                    ("j k", "move; in a dictation, scroll it"),
                    ("ctrl+d u", "half a page; pgdn pgup a page, g G the ends"),
                    ("enter", "the whole text; esc back to the list"),
                    ("/", "filter by the text"),
                    ("C", "clear the whole history, asking first"),
                    ("esc", "clear the filter, else back to Home"),
                    ("q", "quit, asking first about unsaved edits"),
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

    /// `t` on a key row tests it on a worker; the row spins, then says
    /// what came back, and a new key drops the old result.
    #[test]
    fn t_tests_a_key_row() {
        use whisrs::config::setup::key_test::{Outcome, Source, Target, Tested};
        let mut fixture = crate::tui::testing::mockup();
        press(&mut fixture.app, "j\nj");
        assert_eq!(
            press(&mut fixture.app, "t"),
            [Effect::TestKey {
                origin: Origin::Config,
                path: "deepgram.api_key".into(),
                target: Target::Backend("deepgram".into()),
            }]
        );
        assert!(fixture.app.spinning());
        let drawn = crate::tui::testing::rows(&crate::tui::testing::draw(&fixture.app, 90, 30));
        assert!(
            drawn
                .iter()
                .any(|r| r.contains("set ****9f2c \u{00B7} \u{280B}")),
            "the rendered screen differs"
        );
        fixture.app.update(Msg::KeyTested {
            origin: Origin::Config,
            path: "deepgram.api_key".into(),
            tested: Tested {
                outcome: Outcome::Valid { ms: 214 },
                source: Source::Config,
            },
        });
        assert!(!fixture.app.spinning());
        assert_eq!(
            fixture.app.status.as_ref().unwrap().text,
            "deepgram api_key: valid, 214 ms"
        );
        let drawn = crate::tui::testing::rows(&crate::tui::testing::draw(&fixture.app, 90, 30));
        assert!(drawn
            .iter()
            .any(|r| r.contains("set ****9f2c \u{00B7} valid")));

        // The env key wins, and the result says it was that one.
        fixture.app.update(Msg::KeyTested {
            origin: Origin::Config,
            path: "groq.api_key".into(),
            tested: Tested {
                outcome: Outcome::Rejected,
                source: Source::Env("WHISRS_GROQ_API_KEY"),
            },
        });
        let status = fixture.app.status.clone().unwrap();
        assert_eq!(status.tone, Tone::Error);
        assert!(
            status.text.contains("WHISRS_GROQ_API_KEY from this shell"),
            "{}",
            status.text
        );

        // A new key is untested.
        press(&mut fixture.app, "\n");
        crate::tui::testing::type_text(&mut fixture.app, "dg_new_key_1");
        press(&mut fixture.app, "\n");
        assert!(!fixture.app.config.tests.contains_key("deepgram.api_key"));

        // Not a key row: it says where t works.
        press(&mut fixture.app, "k");
        assert!(press(&mut fixture.app, "t").is_empty());
        assert!(fixture
            .app
            .status
            .as_ref()
            .unwrap()
            .text
            .starts_with("t tests a key"));
    }

    /// Mic opens from Home with its device row and a capture; a new device
    /// starts a new capture, w saves as Config does, Esc stops it.
    #[test]
    fn the_mic_screen_captures_and_saves_the_device() {
        use crate::tui::options::{Device, Listing};
        let mut fixture = crate::tui::testing::mockup();
        press(&mut fixture.app, "\x1b");
        assert_eq!(
            press(&mut fixture.app, "j\n"),
            [Effect::All(vec![
                Effect::List(ListKind::Devices),
                Effect::MicStart {
                    session: 1,
                    device: "default".into(),
                },
            ])]
        );
        assert_eq!(fixture.app.screen(), Screen::Mic);
        fixture
            .app
            .update(Msg::Listed(Listing::Devices(Ok(vec![Device {
                id: "alsa_input.usb-yeti".into(),
                description: "Blue Yeti".into(),
                monitor: false,
            }]))));
        assert_eq!(
            press(&mut fixture.app, "l"),
            [Effect::MicStart {
                session: 2,
                device: "alsa_input.usb-yeti".into(),
            }]
        );
        // The picker is Config's, and picks the same way.
        press(&mut fixture.app, "\n");
        assert_eq!(fixture.app.config.mode(), Mode::Picker);
        assert_eq!(
            press(&mut fixture.app, "\x1b"),
            Vec::<Effect>::new(),
            "esc closes the picker, not the screen"
        );
        assert_eq!(fixture.app.screen(), Screen::Mic);
        assert_eq!(
            press(&mut fixture.app, "w"),
            [Effect::SaveDevice { overwrite: false }]
        );
        assert_eq!(press(&mut fixture.app, "\x1b"), [Effect::MicStop]);
        assert_eq!(fixture.app.screen(), Screen::Home);
        // Config is as it was: its section list, the edit kept.
        press(&mut fixture.app, "k\n");
        assert_eq!(fixture.app.screen(), Screen::Config);
        assert_eq!(
            fixture.app.config.form.config().audio.device,
            "alsa_input.usb-yeti"
        );
        let drawn = crate::tui::testing::rows(&crate::tui::testing::draw(&fixture.app, 90, 30));
        assert!(drawn.iter().any(|r| r.contains("Backend & keys")));
    }

    /// Mic's `w` writes `[audio] device` alone onto the file as it is: a
    /// pending Config edit stays pending (and unwritten), the device row is
    /// no edit any more, and Config's own save after it is not taken for a
    /// change on disk.
    #[test]
    fn mic_saves_only_the_device() {
        use crate::tui::options::{Device, Listing};
        let toml = "# mine\n[general]\nnotify = true\n";
        let mut fixture = app(toml);
        let paths = paths(fixture.dir.path());
        // A Config edit, pending: notifications off.
        press(&mut fixture.app, "\njj ");
        assert!(!fixture.app.config.form.config().general.notify);
        // To Home, then Mic, and another device.
        press(&mut fixture.app, "\t\x1bj\n");
        assert_eq!(fixture.app.screen(), Screen::Mic);
        fixture
            .app
            .update(Msg::Listed(Listing::Devices(Ok(vec![Device {
                id: "alsa_input.usb-yeti".into(),
                description: "Blue Yeti".into(),
                monitor: false,
            }]))));
        press(&mut fixture.app, "l");
        assert_eq!(
            press(&mut fixture.app, "w"),
            [Effect::SaveDevice { overwrite: false }]
        );
        let keys = ["audio.device".to_string()];
        let saved = crate::tui::screens::config::save_keys(
            &fixture.app.config,
            &keys,
            false,
            &paths,
            false,
        );
        assert_eq!(
            fixture.app.update(Msg::DeviceSaved(saved)),
            Some(Effect::Restart)
        );
        let text = std::fs::read_to_string(&paths.config).unwrap();
        assert_eq!(
            text,
            format!("{toml}\n[audio]\ndevice = \"alsa_input.usb-yeti\"\n"),
            "the device alone is written"
        );
        let on_disk: whisrs::Config = toml::from_str(&text).unwrap();
        assert_eq!(on_disk.audio.device, "alsa_input.usb-yeti");
        assert!(on_disk.general.notify, "the pending edit is not written");
        assert!(!paths.config.with_file_name("config.toml.bak").exists());
        // Still pending, the device not among the edits.
        assert!(fixture.app.config.form.is_dirty());
        assert!(!fixture.app.config.form.is_dirty_at("audio.device"));
        assert_eq!(
            press(&mut fixture.app, "w"),
            Vec::<Effect>::new(),
            "the device is saved already"
        );
        // Config's save after it writes the rest, with no change-on-disk
        // question.
        let saved = crate::tui::screens::config::save(&fixture.app.config, &paths, false)
            .expect("no change on disk to ask about");
        let on_disk: whisrs::Config =
            toml::from_str(&std::fs::read_to_string(&paths.config).unwrap()).unwrap();
        assert!(!on_disk.general.notify);
        assert_eq!(on_disk.audio.device, "alsa_input.usb-yeti");
        drop(saved);
    }

    /// Setup writes config.toml itself, so Config's unsaved edits are
    /// asked about first: w saves them, d drops them, esc stays on Home.
    #[test]
    fn setup_asks_about_unsaved_config_edits_first() {
        let mut fixture = edited();
        press(&mut fixture.app, "\t\x1b");
        assert_eq!(fixture.app.screen(), Screen::Home);
        assert!(press(&mut fixture.app, "k\n").is_empty());
        assert_eq!(fixture.app.dialog, Some(Dialog::Unsaved(After::Setup)));
        // Esc: nothing opens, the edit stays.
        assert!(press(&mut fixture.app, "\x1b").is_empty());
        assert_eq!(fixture.app.screen(), Screen::Home);
        assert!(fixture.app.config.form.is_dirty());
        // d drops it and opens Setup.
        press(&mut fixture.app, "\n");
        assert_eq!(press(&mut fixture.app, "d"), [Effect::OpenSetup]);
        assert!(!fixture.app.config.form.is_dirty());

        // w saves it, restarts, then opens Setup.
        let mut fixture = edited();
        press(&mut fixture.app, "\t\x1bk\n");
        assert_eq!(press(&mut fixture.app, "w"), [Effect::Save]);
        let saved = save(&fixture.app.config, &paths(fixture.dir.path()), false);
        assert_eq!(
            fixture.app.update(Msg::Saved(saved)),
            Some(Effect::All(vec![Effect::Restart, Effect::OpenSetup]))
        );
        // Clean: it just opens.
        let mut fixture = app("[general]\n");
        press(&mut fixture.app, "\x1bk");
        assert_eq!(press(&mut fixture.app, "\n"), [Effect::OpenSetup]);
    }

    /// Setup opens from Home on what is on disk.
    #[test]
    fn setup_opens_from_home() {
        let mut fixture = crate::tui::testing::mockup();
        press(&mut fixture.app, "\x1bk");
        assert_eq!(press(&mut fixture.app, "\n"), [Effect::OpenSetup]);
        let load = setup_load(&paths(fixture.dir.path()));
        assert_eq!(fixture.app.update(Msg::SetupReady(load)), None);
        assert_eq!(fixture.app.screen(), Screen::Setup);
        let setup = fixture.app.setup.as_ref().unwrap();
        assert_eq!(setup.step, crate::tui::screens::setup::Step::Existing);
        assert_eq!(
            setup.existing.as_ref().unwrap().backend,
            "deepgram-streaming"
        );
        // Finished from Home, it is Home again, not the end.
        press(&mut fixture.app, "j\n");
        fixture
            .app
            .update(Msg::Checked(Box::new(crate::tui::screens::setup::Checks {
                uinput: whisrs::config::setup::uinput::Uinput::Ok,
                manager: ServiceManager::None,
                unit: whisrs::config::setup::unit::UnitFile::Missing,
                unit_path: None,
                enabled: false,
                active: false,
                whisrsd: "whisrsd".into(),
                whisrs: "whisrs".into(),
                compositor: None,
                binding: None,
                gnome: None,
            })));
        assert!(press(&mut fixture.app, "\n").is_empty());
        assert_eq!(fixture.app.screen(), Screen::Home);
        assert!(!fixture.app.quit && fixture.app.farewell.is_empty());
    }
}
