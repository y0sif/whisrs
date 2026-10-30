//! Setup: the onboarding wizard behind `whisrs setup`. One step at a time:
//! the backend, its key (or model, or server), the language, the microphone,
//! the extras, the typing backend, the LLM for command mode, a review that
//! writes config.toml, and the checks of the machine (uinput, the service,
//! a keybinding, the GNOME extension).
//!
//! Every answer is a Config row, edited with Config's own editors: the
//! wizard pins a step's rows on a [`ConfigScreen`] of its own, over a form
//! that starts from the defaults. `Enter` goes on, `Backspace` back, `Space`
//! opens a row's picker or input (`h`/`l` change it in place), `t` tests a
//! key, `s` skips an optional step. Writing goes through Config's save, so
//! its checks and refusals hold here too.

use std::path::{Path, PathBuf};

use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use toml::Value;
use whisrs::config::catalog::{self, WhisperModel};
use whisrs::config::keys::{self, KeySummary};
use whisrs::config::setup::key_test::{Target, Tested};
use whisrs::config::setup::keybind::{self, Binding, Compositor};
use whisrs::config::setup::model::{self, DownloadError};
use whisrs::config::setup::uinput::Uinput;
use whisrs::config::setup::unit::UnitFile;
use whisrs::service::ServiceManager;

use crate::tui::app::{App, Dialog, Effect};
use crate::tui::keys::{Action, Mode};
use crate::tui::options::{self, Listed, Loading, Options, Pick};
use crate::tui::schema::{self, Field, Gate, Kind, ListKind};
use crate::tui::screens::config::{self, ConfigScreen, Loaded, Outcome, Pane, Test};
use crate::tui::widgets::frame::{self, Header};
use crate::tui::widgets::picker::{self, Picked, Picker};
use crate::tui::widgets::row::{Row, Value as Shown};
use crate::tui::widgets::{fit, progress, wrap, Notice, Tone, Ui};
use crate::tui::worker::Download;
use crate::tui::{tilde, Env};

/// The numbered steps.
pub const STEPS: usize = 9;

/// The label column of setup's own rows.
const LABEL_WIDTH: usize = 12;

/// A step of the wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// A config is on disk: start fresh, or only check the machine.
    Existing,
    Backend,
    /// The backend's key and model, its model download, or its server.
    Key,
    Language,
    Mic,
    Extras,
    Typing,
    Llm,
    Review,
    Checks,
}

impl Step {
    /// Its number of [`STEPS`]; the first question has none.
    pub fn number(self) -> Option<usize> {
        Some(match self {
            Step::Existing => return None,
            Step::Backend => 1,
            Step::Key => 2,
            Step::Language => 3,
            Step::Mic => 4,
            Step::Extras => 5,
            Step::Typing => 6,
            Step::Llm => 7,
            Step::Review => 8,
            Step::Checks => 9,
        })
    }

    fn after(self) -> Option<Step> {
        Some(match self {
            Step::Existing | Step::Checks => return None,
            Step::Backend => Step::Key,
            Step::Key => Step::Language,
            Step::Language => Step::Mic,
            Step::Mic => Step::Extras,
            Step::Extras => Step::Typing,
            Step::Typing => Step::Llm,
            Step::Llm => Step::Review,
            Step::Review => Step::Checks,
        })
    }

    /// A step whose answers go into config.toml.
    fn answers(self) -> bool {
        matches!(
            self,
            Step::Backend
                | Step::Key
                | Step::Language
                | Step::Mic
                | Step::Extras
                | Step::Typing
                | Step::Llm
        )
    }
}

/// The config on disk when setup opened (or last wrote it).
#[derive(Debug, Clone)]
pub struct Existing {
    pub backend: String,
    /// What the review compares the answers with.
    pub config: whisrs::Config,
}

/// A model download on its way.
#[derive(Debug, Clone, PartialEq)]
pub struct Downloading {
    /// Its id, for its `.part` (`model::part_path`) and its news.
    pub id: u64,
    pub name: String,
    pub done: u64,
    pub total: Option<u64>,
    /// `[local-whisper] model_path` and the model picked before it, put
    /// back when the download does not finish.
    before: (Option<Value>, usize),
}

/// What the machine looks like for the last step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checks {
    pub uinput: Uinput,
    pub manager: ServiceManager,
    pub unit: UnitFile,
    pub unit_path: Option<PathBuf>,
    /// The service is enabled (installed for its manager).
    pub enabled: bool,
    pub active: bool,
    /// The whisrsd and whisrs on PATH.
    pub whisrsd: String,
    pub whisrs: String,
    pub compositor: Option<Compositor>,
    pub binding: Option<Binding>,
    /// On GNOME with the overlay on: whether its extension is installed.
    pub gnome: Option<bool>,
}

/// One of setup's changes to the machine, run on a worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemAction {
    InstallService,
    EnableService,
    Repoint,
    Bind,
    InstallGnome,
}

/// What a check row's `Space` does.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Fix {
    /// The sudo commands, in the plain terminal.
    Uinput,
    System(SystemAction),
    /// Ask before appending `line` to `path`.
    Bind {
        line: String,
        path: PathBuf,
    },
}

/// A row of the last step.
struct CheckRow {
    label: &'static str,
    /// Done, to do, or something to look at.
    mark: Mark,
    text: String,
    fix: Option<Fix>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    Done,
    ToDo,
    Look,
}

/// What a key on Setup asks of the app.
#[derive(Debug, Default)]
pub struct Reply {
    pub effects: Vec<Effect>,
    pub status: Option<Notice>,
    pub flash: Option<String>,
    pub dialog: Option<Dialog>,
    /// Start the microphone on this device (`Some`), or stop it (`None`).
    pub mic: Option<Option<String>>,
    /// Back to Home, asking first about unwritten answers.
    pub leave: bool,
    /// The last step is done.
    pub finish: bool,
    /// A download stopped on screen whose worker may still be writing its
    /// `.part`: its id and that `.part`, for the way out.
    pub orphan: Option<(u64, PathBuf)>,
}

impl Reply {
    fn say(tone: Tone, text: impl Into<String>) -> Reply {
        Reply {
            status: Some(Notice::new(tone, text)),
            ..Reply::default()
        }
    }

    fn effect(effect: Effect) -> Reply {
        Reply {
            effects: vec![effect],
            ..Reply::default()
        }
    }

    /// Take what the row editor asked for.
    fn from_outcome(outcome: Outcome) -> Reply {
        Reply {
            effects: outcome.load.map(Effect::List).into_iter().collect(),
            status: outcome.status,
            flash: outcome.flash,
            ..Reply::default()
        }
    }
}

pub struct Setup {
    /// The answers, as a config, edited with Config's row editors.
    pub editor: ConfigScreen,
    pub step: Step,
    /// The steps that led here, for `Backspace`.
    back: Vec<Step>,
    pub existing: Option<Existing>,
    /// The file there does not load: the write keeps it as `.bak`.
    pub broken: Option<String>,
    /// The cursor on setup's own rows (the choice, the checks).
    cursor: usize,
    /// config.toml holds the answers.
    pub written: bool,
    /// The answers started fresh (not only the checks).
    pub fresh: bool,
    /// The LLM step was skipped: its keys stay as the file has them.
    skipped_llm: bool,
    /// The key test of the step.
    pub test: Option<Test>,
    pub spin: usize,
    pub download: Option<Downloading>,
    /// Into [`catalog::WHISPER_MODELS`].
    model: usize,
    /// The whisper model picker.
    picker: Option<Picker>,
    pub checks: Option<Box<Checks>>,
    /// The uinput fix ran in this session.
    fixed: bool,
    env: Env,
    models: PathBuf,
    config_path: PathBuf,
}

impl Setup {
    /// Setup over `loaded` (the answers' starting point, the defaults):
    /// with a config on disk it asks first whether to start fresh.
    pub fn new(
        loaded: Loaded,
        existing: Option<Existing>,
        broken: Option<String>,
        env: Env,
        models: PathBuf,
        config_path: PathBuf,
    ) -> Setup {
        let model = catalog::WHISPER_MODELS
            .iter()
            .position(|m| m.name == catalog::DEFAULT_WHISPER_MODEL)
            .unwrap_or(0);
        let mut setup = Setup {
            editor: ConfigScreen::new(loaded, env.clone()),
            step: Step::Existing,
            back: Vec::new(),
            existing,
            broken,
            cursor: 0,
            written: false,
            fresh: false,
            skipped_llm: false,
            test: None,
            spin: 0,
            download: None,
            model,
            picker: None,
            checks: None,
            fixed: false,
            env,
            models,
            config_path,
        };
        if setup.existing.is_none() {
            setup.fresh = true;
            setup.step = Step::Backend;
            setup.editor.pin(setup.fields());
        }
        setup
    }

    /// The answers a fresh setup starts from: the defaults, with filler
    /// removal and the tones on, as setup always offered them.
    pub fn defaults() -> whisrs::Config {
        let mut config: whisrs::Config = toml::from_str("").expect("an empty config parses");
        config.general.remove_filler_words = true;
        config.general.audio_feedback = true;
        config
    }

    /// The effects of the first step, once the app shows it.
    pub fn opened(&mut self) -> Reply {
        self.enter()
    }

    /// Who has the keys.
    pub fn mode(&self) -> Mode {
        if self.picker.is_some() {
            Mode::Picker
        } else {
            self.editor.mode()
        }
    }

    /// Answers that would be lost: given, and not written yet. Once
    /// written, the form holds the file, so any change since is unwritten;
    /// before, a changed answer or a step answered is, wherever the cursor
    /// is now (the first step, reached back, too).
    pub fn unwritten(&self) -> bool {
        if self.written {
            return self.editor.form.is_dirty();
        }
        self.editor.form.is_dirty() || self.back.iter().any(|s| s.answers())
    }

    /// Whether something is moving on screen: a test or the checks.
    pub fn spinning(&self) -> bool {
        self.test == Some(Test::Running)
            || (self.step == Step::Checks && self.checks.is_none())
            || self.editor.tests.values().any(|t| *t == Test::Running)
    }

    fn backend(&self) -> String {
        self.editor.form.config().general.backend.clone()
    }

    /// The rows of the step, from the schema.
    fn fields(&self) -> Vec<&'static Field> {
        self.fields_of(self.step)
    }

    /// The rows of `step`, for the backend as answered.
    fn fields_of(&self, step: Step) -> Vec<&'static Field> {
        let f = schema::row;
        match step {
            Step::Backend => vec![f("general.backend")],
            Step::Key => match self.backend().as_str() {
                "groq" => vec![f("groq.api_key"), f("groq.model")],
                "deepgram" | "deepgram-streaming" => {
                    vec![f("deepgram.api_key"), f("deepgram.model")]
                }
                "openai" | "openai-realtime" => vec![f("openai.api_key"), f("openai.model")],
                "openai-compatible-realtime" => vec![
                    f("openai-compatible-realtime.url"),
                    f("openai-compatible-realtime.model"),
                    f("openai-compatible-realtime.profile"),
                    f("openai-compatible-realtime.turn_detection"),
                    f("openai-compatible-realtime.api_key"),
                ],
                "asr-sidecar" | "asr" | "vibevoice" => vec![
                    f("asr-sidecar.url"),
                    f("asr-sidecar.model"),
                    f("asr-sidecar.api_key"),
                ],
                // local-whisper: the model row of setup's own.
                _ => Vec::new(),
            },
            Step::Language => vec![f("general.language")],
            Step::Mic => vec![f("audio.device")],
            Step::Extras => {
                let mut rows = vec![
                    f("general.remove_filler_words"),
                    f("general.audio_feedback"),
                ];
                if Gate::Overlay.built() {
                    rows.push(f("general.overlay"));
                }
                rows.push(f("overlay.theme"));
                rows
            }
            Step::Typing => vec![f("input.backend")],
            Step::Llm => vec![f("llm.api_url"), f("llm.model"), f("llm.api_key")],
            Step::Existing | Step::Review | Step::Checks => Vec::new(),
        }
    }

    /// The step is local whisper's model.
    fn model_step(&self) -> bool {
        self.step == Step::Key && matches!(self.backend().as_str(), "local-whisper" | "local")
    }

    /// The key the step tests with `t`.
    fn target(&self) -> Option<Target> {
        match self.step {
            Step::Key => Some(Target::Backend(self.backend())),
            Step::Llm => Some(Target::Llm),
            _ => None,
        }
    }

    pub fn key(&mut self, action: Action) -> Reply {
        if self.picker.is_some() {
            return self.picker_key(action);
        }
        if self.editor.mode() != Mode::Browse {
            return self.editor_key(action);
        }
        if self.download.is_some() {
            return match action {
                Action::Back => self.cancel_download(),
                _ => Reply::say(
                    Tone::Muted,
                    "the model is still downloading: esc cancels it",
                ),
            };
        }
        match action {
            Action::Enter => self.next(),
            Action::Previous => self.previous(),
            Action::Skip => self.skip(),
            Action::Test => self.test(),
            Action::Back => Reply {
                leave: true,
                ..Reply::default()
            },
            _ => match self.step {
                Step::Existing => self.choice_key(action),
                Step::Checks => self.checks_key(action),
                Step::Review => Reply::default(),
                _ if self.model_step() => self.model_key(action),
                _ => self.row_key(action),
            },
        }
    }

    /// A key for the row editor: `Space` opens the row's picker, input or
    /// dialog (and flips a toggle); the rest are Config's.
    fn row_key(&mut self, action: Action) -> Reply {
        let action = match (action, self.editor.focused()) {
            (Action::Toggle, Some((field, _))) if !matches!(field.kind, Kind::Bool) => {
                Action::Enter
            }
            (Action::Toggle, _) => Action::Toggle,
            // Enter is the next step here; Space opens.
            (Action::Enter, _) => return Reply::default(),
            (action, _) => action,
        };
        self.editor_key(action)
    }

    /// A key the row editor takes, and what follows from a changed answer.
    fn editor_key(&mut self, action: Action) -> Reply {
        let device = self.editor.form.config().audio.device.clone();
        let outcome = self.editor.key(action);
        let changed = outcome.flash.is_some();
        let mut reply = Reply::from_outcome(outcome);
        if changed {
            if matches!(self.step, Step::Key | Step::Llm) {
                self.test = None;
            }
            let now = self.editor.form.config().audio.device.clone();
            if self.step == Step::Mic && now != device {
                reply.mic = Some(Some(now));
            }
        }
        reply
    }

    fn next(&mut self) -> Reply {
        match self.step {
            Step::Existing => {
                if self.cursor == 0 {
                    self.fresh = true;
                    self.go(Step::Backend)
                } else {
                    self.go(Step::Checks)
                }
            }
            Step::Llm => {
                self.skipped_llm = false;
                self.go(Step::Review)
            }
            Step::Review => Reply::effect(Effect::WriteSetup { overwrite: false }),
            Step::Checks => Reply {
                finish: true,
                ..Reply::default()
            },
            step => match step.after() {
                Some(to) => self.go(to),
                None => Reply::default(),
            },
        }
    }

    fn previous(&mut self) -> Reply {
        let Some(to) = self.back.pop() else {
            return Reply::say(Tone::Muted, "this is the first step");
        };
        let mut reply = self.leave();
        self.step = to;
        self.cursor = 0;
        reply.absorb(self.enter());
        reply
    }

    /// Go on to `to`.
    fn go(&mut self, to: Step) -> Reply {
        let mut reply = self.leave();
        self.back.push(self.step);
        self.step = to;
        self.cursor = 0;
        reply.absorb(self.enter());
        reply
    }

    /// What leaving the step stops.
    fn leave(&mut self) -> Reply {
        self.test = None;
        self.picker = None;
        match self.step {
            Step::Mic => Reply {
                mic: Some(None),
                ..Reply::default()
            },
            _ => Reply::default(),
        }
    }

    /// What arriving at the step starts.
    fn enter(&mut self) -> Reply {
        self.editor.pin(self.fields());
        let mut reply = Reply::default();
        match self.step {
            Step::Key if self.model_step() => {
                self.set_model();
                reply.effects.push(Effect::List(ListKind::Models));
            }
            Step::Key if self.backend() == "openai-compatible-realtime" => {
                // Where Lemonade listens, as setup always offered it.
                let path = "openai-compatible-realtime.url";
                let empty = self
                    .editor
                    .form
                    .effective(path)
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty);
                if empty {
                    let url = Value::String("ws://localhost:12345/realtime".into());
                    let _ = self.editor.form.set(path, Some(url));
                }
            }
            Step::Key => {
                // A key nobody gave yet: type it straight away.
                if let Some((field, path)) = self.editor.focused() {
                    let config = self.editor.form.config();
                    let none = matches!(field.kind, Kind::Secret)
                        && keys::key_summary_from(config, |var| {
                            self.env.get(var).map(str::to_string)
                        }) == KeySummary::NotSet;
                    if none {
                        reply.absorb(Reply::from_outcome(self.editor.edit(&path)));
                    }
                }
            }
            Step::Mic => {
                reply.effects.push(Effect::List(ListKind::Devices));
                reply.mic = Some(Some(self.editor.form.config().audio.device.clone()));
            }
            Step::Checks => {
                self.checks = None;
                reply.effects.push(Effect::Checks);
            }
            _ => {}
        }
        reply
    }

    fn skip(&mut self) -> Reply {
        match self.step {
            Step::Llm => {
                // Its keys are not written: [llm] stays as the file has it.
                self.skipped_llm = true;
                let mut reply = self.go(Step::Review);
                reply.status = Some(Notice::new(
                    Tone::Muted,
                    "skipped: [llm] stays as it is; whisrs config sets it later",
                ));
                reply
            }
            Step::Checks => self.next(),
            _ => Reply::say(Tone::Muted, "this step is not optional"),
        }
    }

    fn test(&mut self) -> Reply {
        let Some(target) = self.target() else {
            return Reply::say(Tone::Muted, "t tests a key: on the key and LLM steps");
        };
        self.test = Some(Test::Running);
        Reply::effect(Effect::TestKey {
            origin: crate::tui::app::Origin::Setup,
            path: String::new(),
            target,
        })
    }

    /// A key test of the step came back.
    pub fn tested(&mut self, tested: Tested) -> Notice {
        let label = match self.step {
            Step::Llm => "LLM key".to_string(),
            _ => format!("{} test", self.backend()),
        };
        let notice = config::test_notice(&label, &tested);
        self.test = Some(Test::Done(tested));
        notice
    }

    /// The first step's choice: start fresh, or only the checks.
    fn choice_key(&mut self, action: Action) -> Reply {
        match action {
            Action::Up | Action::First => self.cursor = 0,
            Action::Down | Action::Last => self.cursor = 1,
            Action::Toggle => return self.next(),
            _ => {}
        }
        Reply::default()
    }

    fn whisper_model(&self) -> &'static WhisperModel {
        &catalog::WHISPER_MODELS[self.model]
    }

    /// Whether model `name` is in the models folder, as last listed.
    fn has_model(&self, name: &str) -> bool {
        match &self.editor.lists.models {
            Listed::Ready(files) => files.iter().any(|file| {
                file.path.file_name().and_then(|n| n.to_str()) == Some(&model::file_name(name))
            }),
            _ => false,
        }
    }

    /// Point `[local-whisper] model_path` at the model picked.
    fn set_model(&mut self) {
        let path = model::path(&self.models, self.whisper_model().name);
        let _ = self.editor.form.set(
            "local-whisper.model_path",
            Some(Value::String(path.to_string_lossy().into_owned())),
        );
    }

    /// The whisper model row: `h`/`l` change it, `Space` opens the picker.
    fn model_key(&mut self, action: Action) -> Reply {
        let len = catalog::WHISPER_MODELS.len();
        match action {
            Action::Left => self.model = (self.model + len - 1) % len,
            Action::Right => self.model = (self.model + 1) % len,
            Action::Toggle => {
                self.picker = Some(Picker::new(
                    "whisper model",
                    self.model_options(),
                    Some(self.whisper_model().name),
                    false,
                ));
                return Reply::default();
            }
            _ => return Reply::default(),
        }
        self.set_model();
        self.test = None;
        Reply {
            flash: Some("model".to_string()),
            ..Reply::default()
        }
    }

    fn model_options(&self) -> Options {
        Options {
            picks: catalog::WHISPER_MODELS
                .iter()
                .map(|m| {
                    let have = if self.has_model(m.name) {
                        ", downloaded"
                    } else {
                        ""
                    };
                    Pick {
                        value: m.name.to_string(),
                        label: m.name.to_string(),
                        about: format!("{}{have}, {}", options::size(m.bytes), m.about),
                        note: None,
                        refused: None,
                    }
                })
                .collect(),
            unset: None,
            open: false,
            loading: Loading::Done,
        }
    }

    fn picker_key(&mut self, action: Action) -> Reply {
        let picker = self.picker.as_mut().expect("a picker");
        match picker.key(action) {
            picker::Answer::Picked(Picked::Value(name)) => {
                self.picker = None;
                self.pick_model(&name)
            }
            picker::Answer::Closed => {
                self.picker = None;
                Reply::default()
            }
            picker::Answer::Refused(why) => Reply::say(Tone::Warn, why),
            _ => Reply::default(),
        }
    }

    /// Use model `name`: downloaded, it is set; else it downloads.
    fn pick_model(&mut self, name: &str) -> Reply {
        let before = (
            self.editor
                .form
                .effective("local-whisper.model_path")
                .cloned(),
            self.model,
        );
        if let Some(at) = catalog::WHISPER_MODELS.iter().position(|m| m.name == name) {
            self.model = at;
        }
        self.set_model();
        self.test = None;
        if self.has_model(name) {
            return Reply::say(Tone::Ok, format!("using {}", model::file_name(name)));
        }
        let id = model::next_id();
        self.download = Some(Downloading {
            id,
            name: name.to_string(),
            done: 0,
            total: None,
            before,
        });
        Reply {
            effects: vec![Effect::Download {
                name: name.to_string(),
                id,
            }],
            status: Some(Notice::new(
                Tone::Muted,
                format!("downloading {} from Hugging Face", model::file_name(name)),
            )),
            ..Reply::default()
        }
    }

    /// `Esc` on a download: it ends on screen at once, the model picked
    /// before it comes back, and its worker stops at its next read, removing
    /// its own `.part` (the way out removes it too, if the TUI goes first).
    fn cancel_download(&mut self) -> Reply {
        let Some(download) = self.download.take() else {
            return Reply::default();
        };
        let part = model::part_path(&self.models, &download.name, download.id);
        let name = model::file_name(&download.name);
        self.put_back(download.before);
        Reply {
            effects: vec![Effect::CancelDownload(download.id)],
            status: Some(Notice::new(
                Tone::Muted,
                format!("cancelled {name}: nothing is left behind"),
            )),
            orphan: Some((download.id, part)),
            ..Reply::default()
        }
    }

    /// Put back the model path and pick a download replaced.
    fn put_back(&mut self, before: (Option<Value>, usize)) {
        let (path, model) = before;
        self.model = model;
        let _ = self.editor.form.set("local-whisper.model_path", path);
    }

    /// News of a download: only the running one's counts.
    pub fn downloaded(&mut self, news: Download) -> Reply {
        if self.download.as_ref().map(|d| d.id) != Some(news.id()) {
            return Reply::default();
        }
        match news {
            Download::Progress { done, total, .. } => {
                if let Some(download) = &mut self.download {
                    download.done = done;
                    download.total = total;
                }
                Reply::default()
            }
            Download::Done { result, .. } => {
                let download = self.download.take().expect("the running download");
                let name = download.name.clone();
                if result.is_err() {
                    self.put_back(download.before);
                }
                let mut reply = Reply::effect(Effect::List(ListKind::Models));
                reply.status = Some(match result {
                    Ok(path) => Notice::new(
                        Tone::Ok,
                        format!(
                            "downloaded {}",
                            tilde(&path, self.env.get("HOME")).display()
                        ),
                    ),
                    Err(DownloadError::Cancelled) => Notice::new(
                        Tone::Muted,
                        format!(
                            "cancelled {}: nothing was left behind",
                            model::file_name(&name)
                        ),
                    ),
                    Err(DownloadError::Failed(why)) => {
                        Notice::new(Tone::Error, format!("the download failed: {why}"))
                    }
                });
                reply
            }
        }
    }

    /// The running download's id and `.part`: what to remove if the TUI
    /// goes before the worker does.
    pub fn partial(&self) -> Option<(u64, PathBuf)> {
        self.download
            .as_ref()
            .map(|d| (d.id, model::part_path(&self.models, &d.name, d.id)))
    }

    /// The answers were written; `again` is the file as it reads now, to
    /// edit on from (another write then sees no change on disk, and the
    /// review compares with it).
    pub fn written(&mut self, again: Option<Loaded>) -> Reply {
        self.written = true;
        if let Some(again) = again {
            let config = again.form.config().clone();
            self.existing = Some(Existing {
                backend: config.general.backend.clone(),
                config,
            });
            self.broken = None;
            self.editor.reloaded(again);
        }
        self.go(Step::Checks)
    }

    /// The keys a write sets, from the answers as they are now: each answer
    /// step's rows for the backend answered, the whisper model path on its
    /// step, the LLM step's only when it was not skipped.
    pub fn asked(&self) -> Vec<String> {
        let mut keys = Vec::new();
        for step in [
            Step::Backend,
            Step::Key,
            Step::Language,
            Step::Mic,
            Step::Extras,
            Step::Typing,
            Step::Llm,
        ] {
            if step == Step::Llm && self.skipped_llm {
                continue;
            }
            keys.extend(self.fields_of(step).iter().map(|f| f.path.to_string()));
            if step == Step::Key && matches!(self.backend().as_str(), "local-whisper" | "local") {
                keys.push("local-whisper.model_path".to_string());
            }
        }
        keys
    }

    /// What the write changes in the file there: each asked key whose value
    /// differs, its label, and the old and new value as shown (a key masked,
    /// never in full). `None` with no file to compare with.
    pub fn changes(&self) -> Option<Vec<(String, String, String)>> {
        let before = crate::tui::form::Form::new(self.existing.as_ref()?.config.clone()).ok()?;
        let after = &self.editor.form;
        Some(
            self.asked()
                .into_iter()
                .filter_map(|key| {
                    let (old, new) = (before.effective(&key), after.effective(&key));
                    if old == new {
                        return None;
                    }
                    // As config.toml spells it: two rows can share a label.
                    let label = match key.split_once('.') {
                        Some((section, rest)) => format!("[{section}] {rest}"),
                        None => key.clone(),
                    };
                    let secret = key.ends_with("api_key");
                    Some((label, shown_value(old, secret), shown_value(new, secret)))
                })
                .collect(),
        )
    }

    /// The machine was looked at.
    pub fn checked(&mut self, checks: Checks) {
        self.checks = Some(Box::new(checks));
        self.cursor = self
            .cursor
            .min(self.check_rows(false).len().saturating_sub(1));
    }

    /// The uinput fix ran.
    pub fn fixed(&mut self) {
        self.fixed = true;
    }

    fn checks_key(&mut self, action: Action) -> Reply {
        let rows = self.check_rows(false);
        let last = rows.len().saturating_sub(1);
        match action {
            Action::Up => self.cursor = self.cursor.saturating_sub(1),
            Action::Down => self.cursor = (self.cursor + 1).min(last),
            Action::First => self.cursor = 0,
            Action::Last => self.cursor = last,
            Action::Toggle => {
                return match rows.into_iter().nth(self.cursor).and_then(|r| r.fix) {
                    Some(Fix::Uinput) => Reply::effect(Effect::FixUinput),
                    Some(Fix::System(action)) => Reply::effect(Effect::System(action)),
                    Some(Fix::Bind { line, path }) => Reply {
                        dialog: Some(Dialog::Bind {
                            line,
                            path: tilde(&path, self.env.get("HOME")).display().to_string(),
                        }),
                        ..Reply::default()
                    },
                    None => Reply::say(Tone::Muted, "nothing to do on this row"),
                };
            }
            _ => {}
        }
        Reply::default()
    }

    /// The last step's rows; `daemon_up` notes a daemon started by hand.
    fn check_rows(&self, daemon_up: bool) -> Vec<CheckRow> {
        let Some(checks) = &self.checks else {
            return Vec::new();
        };
        let home = self.env.get("HOME");
        let show = |path: &Path| tilde(path, home).display().to_string();
        let mut rows = Vec::new();
        rows.push(match &checks.uinput {
            Uinput::Ok => CheckRow {
                label: "uinput",
                mark: Mark::Done,
                text: "/dev/uinput can be written: whisrs can type".into(),
                fix: None,
            },
            Uinput::Denied if self.fixed => CheckRow {
                label: "uinput",
                mark: Mark::ToDo,
                text: "the rule is in: log out and back in for the input group".into(),
                fix: None,
            },
            Uinput::Denied => CheckRow {
                label: "uinput",
                mark: Mark::ToDo,
                text: "permission denied: space installs the udev rule and joins the input \
                       group (sudo asks for your password)"
                    .into(),
                fix: Some(Fix::Uinput),
            },
            Uinput::Other(why) => CheckRow {
                label: "uinput",
                mark: Mark::Look,
                text: why.clone(),
                fix: None,
            },
        });
        let service = match (checks.manager, &checks.unit) {
            (ServiceManager::None, _) => CheckRow {
                label: "service",
                mark: Mark::Look,
                text: "neither systemd nor OpenRC: start whisrsd & from your session".into(),
                fix: None,
            },
            (manager, UnitFile::Missing) => CheckRow {
                label: "service",
                mark: Mark::ToDo,
                text: format!(
                    "not installed: space installs the {} service and starts it",
                    manager.name()
                ),
                fix: Some(Fix::System(SystemAction::InstallService)),
            },
            (_, UnitFile::Stale { binary, new }) => CheckRow {
                label: "service",
                mark: Mark::ToDo,
                text: format!(
                    "runs {binary}, not {}: space repoints it",
                    show(Path::new(new))
                ),
                fix: Some(Fix::System(SystemAction::Repoint)),
            },
            (_, UnitFile::Foreign) if checks.active => CheckRow {
                label: "service",
                mark: Mark::Done,
                text: "running; setup cannot read what its unit runs, so it leaves the unit \
                       as it is"
                    .into(),
                fix: None,
            },
            (_, _) if checks.active => CheckRow {
                label: "service",
                mark: Mark::Done,
                text: format!("running {}", show(Path::new(&checks.whisrsd))),
                fix: None,
            },
            (_, unit) => {
                let mut text = if checks.enabled {
                    "installed, not running: space starts it".to_string()
                } else {
                    "installed, not enabled: space enables and starts it".to_string()
                };
                if *unit == UnitFile::Foreign {
                    text.push_str(" (its ExecStart is left as it is)");
                }
                if daemon_up {
                    text.push_str(
                        "; a whisrsd started by hand runs now, so stop it first (pkill whisrsd)",
                    );
                }
                CheckRow {
                    label: "service",
                    mark: Mark::ToDo,
                    text,
                    fix: Some(Fix::System(SystemAction::EnableService)),
                }
            }
        };
        rows.push(service);
        let bind = match (&checks.compositor, &checks.binding) {
            (None, _) => CheckRow {
                label: "keybinding",
                mark: Mark::Look,
                text: "no compositor found: bind whisrs toggle in yours".into(),
                fix: None,
            },
            (Some(_), Some(Binding::Bound(path))) => CheckRow {
                label: "keybinding",
                mark: Mark::Done,
                text: format!("{} binds whisrs toggle", show(path)),
                fix: None,
            },
            (Some(c), Some(Binding::Unbound(path))) => CheckRow {
                label: "keybinding",
                mark: Mark::ToDo,
                text: format!(
                    "{} binds no whisrs toggle: space adds {}",
                    show(path),
                    keybind::keys(c)
                ),
                fix: keybind::line(c, &checks.whisrs).map(|line| Fix::Bind {
                    line,
                    path: path.clone(),
                }),
            },
            (Some(c), Some(Binding::NoFile(path))) => CheckRow {
                label: "keybinding",
                mark: Mark::Look,
                text: format!(
                    "no {}: add {}",
                    show(path),
                    keybind::line(c, "whisrs").unwrap_or_default()
                ),
                fix: None,
            },
            (Some(c), None) => CheckRow {
                label: "keybinding",
                mark: Mark::Look,
                text: format!("bind whisrs toggle in {}'s keyboard shortcuts", c.name()),
                fix: None,
            },
        };
        rows.push(bind);
        if let Some(installed) = checks.gnome {
            rows.push(if installed {
                CheckRow {
                    label: "GNOME ext.",
                    mark: Mark::Done,
                    text: "the overlay's GNOME Shell extension is installed".into(),
                    fix: None,
                }
            } else {
                CheckRow {
                    label: "GNOME ext.",
                    mark: Mark::ToDo,
                    text: "GNOME draws the overlay with an extension: space installs it".into(),
                    fix: Some(Fix::System(SystemAction::InstallGnome)),
                }
            });
        }
        rows
    }

    /// The step's title and what to know first.
    fn intro(&self) -> (String, String) {
        let env_note = |var: &str| match self.env.get(var).filter(|v| !v.is_empty()) {
            Some(_) => format!(
                " {var} is set in this shell and wins over a key here; the daemon must see it in \
                 its own environment too."
            ),
            None => format!(" Leave it blank to use {var}."),
        };
        let (title, text) = match self.step {
            Step::Existing => {
                let backend = self
                    .existing
                    .as_ref()
                    .map_or("", |e| e.backend.as_str())
                    .to_string();
                (
                    "A config is there".to_string(),
                    format!(
                        "{} has backend {backend}. Start fresh asks every question again and \
                         writes the answers over it; System checks only looks at the machine.",
                        self.shown_path()
                    ),
                )
            }
            Step::Backend => (
                "Transcription backend".to_string(),
                "Where your speech becomes text. h/l changes it; space lists them, with what each \
                 one is."
                    .to_string(),
            ),
            Step::Key => match self.backend().as_str() {
                "groq" => (
                    "Groq API key".to_string(),
                    format!(
                        "Get one free at console.groq.com/keys. It is saved in config.toml \
                         (0600).{}",
                        env_note("WHISRS_GROQ_API_KEY")
                    ),
                ),
                "deepgram" | "deepgram-streaming" => (
                    "Deepgram API key".to_string(),
                    format!(
                        "Get one, with free credit, at console.deepgram.com/signup. It is saved \
                         in config.toml (0600).{}",
                        env_note("WHISRS_DEEPGRAM_API_KEY")
                    ),
                ),
                "openai" | "openai-realtime" => (
                    "OpenAI API key".to_string(),
                    format!(
                        "Get one at platform.openai.com/api-keys. It is saved in config.toml \
                         (0600).{}",
                        env_note("WHISRS_OPENAI_API_KEY")
                    ),
                ),
                "openai-compatible-realtime" => (
                    "Realtime server".to_string(),
                    "Your OpenAI-compatible realtime server (Lemonade): its WebSocket URL, the \
                     model it loads, and a bearer token if it needs one."
                        .to_string(),
                ),
                "asr-sidecar" | "asr" | "vibevoice" => (
                    "ASR sidecar".to_string(),
                    "Your ASR sidecar's HTTP endpoint, its model, and a bearer token if it \
                     needs one. t sends it half a second of silence."
                        .to_string(),
                ),
                _ => (
                    "Whisper model".to_string(),
                    "whisper.cpp runs on this machine, offline. Space lists the models with \
                     their sizes; picking one that is not downloaded yet downloads it. The .en \
                     ones know English only, and know it better."
                        .to_string(),
                ),
            },
            Step::Language => (
                "Language".to_string(),
                "The language you dictate in, or auto to detect it. Space lists them all: type \
                 to find one."
                    .to_string(),
            ),
            Step::Mic => (
                "Microphone".to_string(),
                "Say a sentence: the bar passes the gate while you talk and drops under it when \
                 you stop."
                    .to_string(),
            ),
            Step::Extras => (
                "Extras".to_string(),
                "Filler words, tones, and the on-screen overlay. The theme colours the overlay, \
                 and this screen."
                    .to_string(),
            ),
            Step::Typing => (
                "Typing".to_string(),
                "How whisrs types the text. auto suits most setups.".to_string(),
            ),
            Step::Llm => (
                "Command mode (optional)".to_string(),
                "Select text, press a hotkey and say what to do: an LLM rewrites it. s skips \
                 this and keeps [llm] as config.toml has it; whisrs config sets it later."
                    .to_string(),
            ),
            Step::Review => (
                "Review".to_string(),
                format!(
                    "Enter writes {} (0600). Backspace goes back to change an answer.",
                    self.shown_path()
                ),
            ),
            Step::Checks => (
                "System checks".to_string(),
                "What dictation needs on this machine. Space does a row; enter finishes."
                    .to_string(),
            ),
        };
        (title, text)
    }

    fn shown_path(&self) -> String {
        tilde(&self.config_path, self.env.get("HOME"))
            .display()
            .to_string()
    }

    /// The review: each answer, and what the write will do.
    fn review(&self, ui: &Ui) -> Vec<(String, Shown)> {
        let config = self.editor.form.config();
        let sep = ui.glyphs.separator;
        let on = |b: bool| if b { "on" } else { "off" };
        let mut rows = vec![("backend".to_string(), Shown::plain(&config.general.backend))];
        let key = match keys::key_summary_from(config, |var| self.env.get(var).map(str::to_string))
        {
            KeySummary::File { masked } => Some(Shown::plain(format!("set {masked}"))),
            KeySummary::Env { var, .. } => {
                Some(Shown::plain(var).then("(this shell)", Tone::Muted))
            }
            KeySummary::NotSet => Some(Shown::of("not set", Tone::Error)),
            KeySummary::Optional(what) => Some(Shown::muted(format!("no {what}"))),
            KeySummary::NotNeeded => None,
        };
        if let Some(key) = key {
            rows.push(("key".to_string(), key));
        }
        match config.general.backend.as_str() {
            "local-whisper" | "local" => {
                let name = self.whisper_model().name;
                let have = if self.has_model(name) {
                    Shown::plain(name).then("downloaded", Tone::Ok)
                } else {
                    Shown::plain(name).then("not downloaded", Tone::Warn)
                };
                rows.push(("model".to_string(), have));
            }
            "openai-compatible-realtime" => {
                let url = config
                    .openai_compatible_realtime
                    .as_ref()
                    .map(|r| r.url.clone())
                    .unwrap_or_default();
                rows.push(("server".to_string(), Shown::plain(url)));
            }
            "asr-sidecar" | "asr" | "vibevoice" => {
                let url = config.asr_sidecar.as_ref().map_or_else(
                    || "http://127.0.0.1:8765/transcribe".into(),
                    |s| s.url.clone(),
                );
                rows.push(("sidecar".to_string(), Shown::plain(url)));
            }
            backend => {
                let model = whisrs::transcription::factory::model_for_backend(backend, config);
                rows.push(("model".to_string(), Shown::plain(model)));
            }
        }
        rows.push((
            "language".to_string(),
            Shown::plain(&config.general.language),
        ));
        rows.push(("mic".to_string(), Shown::plain(&config.audio.device)));
        let theme = config.overlay.clone().unwrap_or_default().theme;
        rows.push((
            "extras".to_string(),
            Shown::plain(format!(
                "fillers {} {sep} tones {} {sep} overlay {} {sep} theme {theme}",
                on(config.general.remove_filler_words),
                on(config.general.audio_feedback),
                on(config.general.overlay),
            )),
        ));
        let typing = self
            .editor
            .form
            .effective("input.backend")
            .and_then(Value::as_str)
            .unwrap_or("auto")
            .to_string();
        rows.push(("typing".to_string(), Shown::plain(typing)));
        rows.push((
            "LLM".to_string(),
            match &config.llm {
                _ if self.skipped_llm && self.existing.is_some() => {
                    Shown::muted("skipped: as config.toml has it")
                }
                _ if self.skipped_llm => Shown::muted("skipped"),
                Some(llm) => Shown::plain(format!("{} {sep} {}", llm.model, llm.api_url)),
                None => Shown::muted("not set"),
            },
        ));
        rows
    }

    /// What the write will do, and what it would refuse.
    fn review_notes(&self) -> Vec<Notice> {
        let mut notes = Vec::new();
        // As the write judges it: a missing key is a warning there.
        match self.editor.form.config().validate_to_write() {
            Ok(warnings) => notes.extend(
                warnings
                    .into_iter()
                    .map(|w| Notice::new(Tone::Warn, w.message.replace('\n', " "))),
            ),
            Err(e) => notes.push(Notice::new(
                Tone::Error,
                format!(
                    "not written yet: {}",
                    e.to_string().lines().next().unwrap_or_default()
                ),
            )),
        }
        let bak = whisrs::config::write::config_backup_path(&self.config_path);
        let bak = tilde(&bak, self.env.get("HOME")).display().to_string();
        if self.broken.is_some() {
            notes.push(Notice::new(
                Tone::Warn,
                format!(
                    "the file there does not load: it is kept as {bak} and written anew from \
                     these answers"
                ),
            ));
        } else if self.existing.is_some() {
            notes.push(Notice::new(
                Tone::Muted,
                format!(
                    "the file is kept as {bak} first; only the keys above change, and the rest \
                     of it, comments too, stays as it is"
                ),
            ));
        }
        notes
    }
}

impl Reply {
    /// Add what `other` asks for to this.
    fn absorb(&mut self, other: Reply) {
        self.effects.extend(other.effects);
        if other.status.is_some() {
            self.status = other.status;
        }
        if other.flash.is_some() {
            self.flash = other.flash;
        }
        if other.dialog.is_some() {
            self.dialog = other.dialog;
        }
        if other.mic.is_some() {
            self.mic = other.mic;
        }
        self.leave |= other.leave;
        self.finish |= other.finish;
    }
}

/// A config value as the review shows it: a key masked to its last four
/// characters (never in full), a toggle as on or off, unset as `not set`.
fn shown_value(value: Option<&Value>, secret: bool) -> String {
    match value {
        None => "not set".to_string(),
        Some(Value::String(s)) if s.is_empty() => {
            if secret {
                "not set".to_string()
            } else {
                "empty".to_string()
            }
        }
        Some(Value::String(s)) if secret => keys::mask_api_key(s),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Boolean(on)) => if *on { "on" } else { "off" }.to_string(),
        Some(other) if secret => keys::mask_api_key(&other.to_string()),
        Some(other) => other.to_string(),
    }
}

pub fn view(app: &App, f: &mut Frame, ui: &Ui) {
    let Some(setup) = &app.setup else {
        return;
    };
    let area = f.area();
    let width = usize::from(frame::body_width(area.width).saturating_sub(2));
    let messages = app.messages(ui, width);
    let (title, intro) = setup.intro();
    let intro = wrap(&intro, width.max(1));
    let mut body_lines: Vec<Line<'static>> = Vec::new();
    // Built first, drawn after the rows the editor draws itself.
    let editor_rows = setup.editor_rows();
    let after = setup.lines_after(app, width, ui);
    let help_room = width.saturating_sub(2).max(1);
    let help = if editor_rows > 0 && setup.mode() == Mode::Browse {
        wrap(&config::row_help(&setup.editor), help_room)
    } else if setup.step == Step::Checks {
        // The focused check in full: its row is one line.
        setup
            .check_rows(app.daemon_up() == Some(true))
            .into_iter()
            .nth(setup.cursor)
            .map(|check| wrap(&check.text, help_room))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    body_lines.extend(after);
    let rows = 1
        + intro.len()
        + 1
        + editor_rows
        + body_lines.len()
        + if help.is_empty() { 0 } else { help.len() + 1 }
        + if messages.is_empty() {
            0
        } else {
            messages.len() + 1
        };
    let Some(placed) = frame::place(area, Some(rows as u16)) else {
        return frame::too_small(f, ui);
    };
    let name = match setup.step.number() {
        Some(n) => format!("setup {n}/{STEPS}"),
        None => "setup".to_string(),
    };
    let body = frame::draw(
        f,
        placed.frame,
        ui,
        Header {
            screen: &name,
            daemon: app.daemon_up(),
            live: (setup.step == Step::Mic).then(|| app.mic.level()).flatten(),
        },
    );
    let mut y = body.y;
    let put = |f: &mut Frame, line: Line<'static>, y: &mut u16| {
        if *y < body.bottom() {
            f.render_widget(Paragraph::new(line), Rect::new(body.x, *y, body.width, 1));
        }
        *y += 1;
    };
    put(
        f,
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                fit(&title, width.saturating_sub(2), ui.glyphs.ellipsis),
                ratatui::style::Style::new().add_modifier(Modifier::BOLD),
            ),
        ]),
        &mut y,
    );
    for part in intro {
        put(
            f,
            Line::from(vec![Span::raw("  "), Span::styled(part, ui.theme.muted)]),
            &mut y,
        );
    }
    y += 1;
    if editor_rows > 0 {
        let height = (editor_rows as u16).min(body.bottom().saturating_sub(y));
        config::draw_rows(
            &setup.editor,
            &app.env,
            app.flash.as_deref(),
            f,
            Rect::new(body.x, y, body.width, height),
            ui,
            Pane {
                crumb: false,
                help: false,
            },
        );
        y += editor_rows as u16;
    }
    for line in body_lines {
        put(f, line, &mut y);
    }
    if !help.is_empty() {
        y += 1;
        for part in help {
            put(
                f,
                Line::from(vec![Span::raw("  "), Span::styled(part, ui.theme.muted)]),
                &mut y,
            );
        }
    }
    if !messages.is_empty() {
        y += 1;
        for line in messages {
            if y < body.bottom() {
                f.render_widget(
                    line,
                    Rect::new(body.x + 2, y, body.width.saturating_sub(2), 1),
                );
            }
            y += 1;
        }
    }
    setup.footer(f, placed.footer, ui);
    config::draw_open(&setup.editor, f, placed.frame, body, ui);
    if let Some(picker) = &setup.picker {
        crate::tui::widgets::dialog::dim(f.buffer_mut(), placed.frame, ui);
        picker.draw(f, body, ui);
    }
}

impl Setup {
    /// How many rows the editor draws on this step.
    fn editor_rows(&self) -> usize {
        match self.step {
            Step::Existing | Step::Review | Step::Checks => 0,
            _ if self.model_step() => 0,
            _ => self.fields().len(),
        }
    }

    /// The step's lines under the editor's rows: setup's own rows, the
    /// test, the meter, the download.
    fn lines_after(&self, app: &App, width: usize, ui: &Ui) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        // The choice's names are longer than a label.
        let labels = match self.step {
            Step::Existing => "System checks only".len() + 2,
            _ => self.label_width(),
        };
        let row = |label: &str, value: Shown, focused: bool, choice: bool, flash: bool| {
            let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, width as u16, 1));
            Row {
                focused,
                choice,
                flash,
                ..Row::new(label, value)
            }
            .draw(Rect::new(0, 0, width as u16, 1), &mut buf, ui, labels);
            line_of(&buf, width)
        };
        match self.step {
            Step::Existing => {
                for (i, (name, about)) in [
                    ("Start fresh", "answer every question again"),
                    ("System checks only", "keep the config, check the machine"),
                ]
                .into_iter()
                .enumerate()
                {
                    lines.push(row(
                        name,
                        Shown::muted(about),
                        i == self.cursor,
                        false,
                        false,
                    ));
                }
            }
            Step::Key if self.model_step() => {
                let m = self.whisper_model();
                let sep = ui.glyphs.separator;
                let value = Shown::plain(m.name).then(
                    format!("{} {sep} {}", options::size(m.bytes), m.about),
                    Tone::Muted,
                );
                let flash = app.flash.as_deref() == Some("model");
                lines.push(row("model", value, true, true, flash));
                let path = model::path(&self.models, m.name);
                let file = Shown::muted(tilde(&path, self.env.get("HOME")).display().to_string());
                let file = if self.has_model(m.name) {
                    file.then(format!("{sep} downloaded"), Tone::Ok)
                } else {
                    file.then(format!("{sep} not downloaded"), Tone::Warn)
                };
                lines.push(row("file", file, false, false, false));
                if let Some(download) = &self.download {
                    let mut spans = vec![Span::raw("  ")];
                    spans.push(Span::styled(
                        format!("{:<LABEL_WIDTH$}", "download"),
                        ui.theme.muted,
                    ));
                    spans.extend(progress::line(download.done, download.total, ui).spans);
                    lines.push(Line::from(spans));
                }
                lines.push(self.test_line(width, ui));
            }
            Step::Key | Step::Llm => lines.push(self.test_line(width, ui)),
            Step::Mic => {
                lines.push(Line::default());
                lines.extend(app.mic.lines(width, ui));
            }
            Step::Review => {
                for (label, value) in self.review(ui) {
                    lines.push(row(&label, value, false, false, false));
                }
                if let Some(changes) = self.changes() {
                    lines.push(Line::default());
                    let mut spans = vec![Span::raw("  ")];
                    if changes.is_empty() {
                        spans.push(Span::styled(
                            "These answers change nothing in config.toml.",
                            ui.theme.muted,
                        ));
                        lines.push(Line::from(spans));
                    } else {
                        spans.push(Span::styled("What changes in config.toml:", ui.theme.muted));
                        lines.push(Line::from(spans));
                    }
                    let labels = changes
                        .iter()
                        .map(|(label, _, _)| crate::tui::widgets::width(label) + 2)
                        .max()
                        .unwrap_or(labels)
                        .min(width / 2);
                    let room = width.saturating_sub(labels + 2);
                    for (label, old, new) in changes {
                        let label = fit(&label, labels.saturating_sub(2), ui.glyphs.ellipsis);
                        let text = format!("{old} {} {new}", ui.glyphs.to);
                        let mut spans = vec![
                            Span::raw("  "),
                            Span::styled(format!("{label:<labels$}"), ui.theme.muted),
                        ];
                        for (i, part) in wrap(&text, room.max(1)).into_iter().enumerate() {
                            if i > 0 {
                                lines.push(Line::from(std::mem::take(&mut spans)));
                                spans.push(Span::raw(" ".repeat(labels + 2)));
                            }
                            spans.push(Span::raw(part));
                        }
                        lines.push(Line::from(spans));
                    }
                }
                let notes = self.review_notes();
                if !notes.is_empty() {
                    lines.push(Line::default());
                    lines.extend(
                        frame::messages(&notes, None, ui, width.saturating_sub(2))
                            .into_iter()
                            .map(|l| {
                                let mut spans = vec![Span::raw("  ")];
                                spans.extend(l.spans);
                                Line::from(spans)
                            }),
                    );
                }
            }
            Step::Checks => {
                let up = app.daemon_up() == Some(true);
                let rows = self.check_rows(up);
                if rows.is_empty() {
                    let spin = ui.glyphs.spinner[self.spin % ui.glyphs.spinner.len()];
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(format!("{spin} looking..."), ui.theme.muted),
                    ]));
                }
                for (i, check) in rows.into_iter().enumerate() {
                    let (mark, tone) = match check.mark {
                        Mark::Done => (ui.glyphs.up, Tone::Ok),
                        Mark::ToDo => (ui.glyphs.down, Tone::Plain),
                        Mark::Look => (ui.glyphs.warn, Tone::Warn),
                    };
                    let focused = i == self.cursor;
                    let room = width.saturating_sub(LABEL_WIDTH + 6);
                    // One line each; the focused one's whole text is the
                    // help under them, so a cut row still reads in full.
                    let parts = vec![fit(&check.text, room, ui.glyphs.ellipsis)];
                    for (n, part) in parts.into_iter().enumerate() {
                        let mut spans = vec![
                            if focused && n == 0 {
                                Span::styled(ui.glyphs.gutter, ui.theme.accent)
                            } else {
                                Span::raw(" ")
                            },
                            Span::raw(" "),
                        ];
                        if n == 0 {
                            let style = match tone {
                                Tone::Plain => ui.theme.accent,
                                tone => tone.style(ui.theme),
                            };
                            spans.push(Span::styled(mark, style));
                            spans.push(Span::raw(" "));
                            let label = format!("{:<LABEL_WIDTH$}", check.label);
                            spans.push(if focused {
                                Span::styled(
                                    label,
                                    ratatui::style::Style::new().add_modifier(Modifier::BOLD),
                                )
                            } else {
                                Span::styled(label, ui.theme.muted)
                            });
                        } else {
                            spans.push(Span::raw(" ".repeat(LABEL_WIDTH + 2)));
                        }
                        spans.push(Span::raw(part));
                        lines.push(Line::from(spans));
                    }
                }
            }
            _ => {}
        }
        lines
    }

    /// The label column: the editor's, where its rows are on screen, so
    /// setup's own lines line up under them.
    fn label_width(&self) -> usize {
        let fields = if self.editor_rows() > 0 {
            self.fields()
        } else {
            Vec::new()
        };
        fields
            .iter()
            .map(|field| crate::tui::widgets::width(field.label) + 2)
            .max()
            .map_or(LABEL_WIDTH, |w| w.min(24))
    }

    /// What `t` does on this step, before it ran.
    fn test_hint(&self) -> &'static str {
        match (self.step, self.backend().as_str()) {
            (Step::Llm, _) => "t tests the key, sending no audio",
            (_, "asr-sidecar" | "asr" | "vibevoice") => {
                "t sends the sidecar half a second of silence"
            }
            (_, "local-whisper" | "local") => "t checks the model file",
            (_, "openai-compatible-realtime") => "t says what can be tested",
            _ => "t tests the key, sending no audio",
        }
    }

    /// The test line of the key and LLM steps.
    fn test_line(&self, width: usize, ui: &Ui) -> Line<'static> {
        let (text, tone) = match &self.test {
            None => (self.test_hint().to_string(), Tone::Muted),
            Some(Test::Running) => (
                format!(
                    "{} testing...",
                    ui.glyphs.spinner[self.spin % ui.glyphs.spinner.len()]
                ),
                Tone::Muted,
            ),
            Some(test @ Test::Done(tested)) => {
                let (_, tone) = config::test_word(test, 0, ui);
                (tested.outcome.to_string(), tone)
            }
        };
        let labels = self.label_width();
        let room = width.saturating_sub(labels + 2);
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{:<labels$}", "test"), ui.theme.muted),
            Span::styled(fit(&text, room, ui.glyphs.ellipsis), tone.style(ui.theme)),
        ])
    }

    fn footer(&self, f: &mut Frame, rect: Rect, ui: &Ui) {
        if self.picker.is_some() {
            let keys = format!("{}/{}", ui.glyphs.key_up, ui.glyphs.key_down);
            frame::footer(
                f,
                rect,
                ui,
                &[
                    ("type", "to filter"),
                    ("enter", "pick"),
                    ("esc", "cancel"),
                    (&keys, "move"),
                ],
            );
            return;
        }
        if config::draw_open_footer(&self.editor, f, rect, ui) {
            return;
        }
        let pair = |key: &str, what: &str| (key.to_string(), what.to_string());
        let mut hints: Vec<(String, String)> = Vec::new();
        if self.download.is_some() {
            hints.push(pair("esc", "cancel the download"));
        } else {
            match self.step {
                Step::Existing => {
                    hints.extend([pair("j/k", "move"), pair("enter", "choose")]);
                }
                Step::Review => hints.push(pair("enter", "write")),
                Step::Checks => hints.extend([
                    pair("j/k", "move"),
                    pair("space", "do it"),
                    pair("enter", "finish"),
                ]),
                _ if self.model_step() => {
                    hints.extend([pair("h/l", "change"), pair("space", "pick")]);
                    hints.push(pair("enter", "next"));
                }
                _ => {
                    if let Some((field, _)) = self.editor.focused() {
                        hints.extend(self.verbs(field, ui));
                    }
                    hints.push(pair("enter", "next"));
                }
            }
            // The ones to go first when narrow come last but the way out.
            if self.target().is_some() {
                hints.push(pair("t", "test"));
            }
            if matches!(self.step, Step::Llm | Step::Checks) {
                hints.push(pair("s", "skip"));
            }
            if self.step != Step::Existing {
                hints.push(pair("backspace", "back"));
            }
            hints.push(pair("esc", "leave"));
        }
        let refs: Vec<(&str, &str)> = hints
            .iter()
            .map(|(k, w)| (k.as_str(), w.as_str()))
            .collect();
        frame::footer(f, rect, ui, &refs);
    }

    /// What the keys do on a row of `field`, setup's way: `Space` opens.
    fn verbs(&self, field: &Field, ui: &Ui) -> Vec<(String, String)> {
        let pair = |key: &str, what: &str| (key.to_string(), what.to_string());
        match &field.kind {
            Kind::Bool => vec![pair("space", "toggle")],
            Kind::Text { .. } | Kind::Secret => vec![pair("space", "type")],
            _ => config::verbs(field, ui)
                .into_iter()
                .map(|(key, what)| {
                    if key == "enter" {
                        ("space".to_string(), what)
                    } else {
                        (key, what)
                    }
                })
                .collect(),
        }
    }
}

/// The text of a one-line buffer, styles kept.
fn line_of(buf: &ratatui::buffer::Buffer, width: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for x in 0..width as u16 {
        let cell = &buf[(x, 0)];
        spans.push(Span::styled(cell.symbol().to_string(), cell.style()));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests;
