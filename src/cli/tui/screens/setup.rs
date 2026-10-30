//! Setup: the onboarding wizard behind `whisrs setup`. One step at a time:
//! the backend, its key (or model, or server), the language, the microphone,
//! the extras, the typing backend, then the optional modes (recording,
//! command mode, read aloud, Jarvis), a review that writes config.toml, and
//! the checks of the machine (uinput, the service, a keybinding, the GNOME
//! extension).
//!
//! Every answer is a Config row, edited with Config's own editors: the
//! wizard pins a step's rows on a [`ConfigScreen`] of its own, over a form
//! that starts from the defaults. `Enter` goes on, `Backspace` back, `Space`
//! opens a row's picker or input (`h`/`l` change it in place), `t` tests a
//! key, `s` skips an optional step. An optional step asks first whether to
//! set its mode up; skipped, none of its keys are written. Writing goes
//! through Config's save, so its checks and refusals hold here too.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use toml::Value;
use whisrs::config::catalog::{self, WhisperModel};
use whisrs::config::keys::{self, KeySummary};
use whisrs::config::setup::key_test::{Provider, Target, Tested};
use whisrs::config::setup::keybind::{self, Binding, Compositor};
use whisrs::config::setup::model::{self, DownloadError};
use whisrs::config::setup::uinput::Uinput;
use whisrs::config::setup::unit::UnitFile;
use whisrs::service::ServiceManager;

use crate::tui::app::{App, Dialog, Effect};
use crate::tui::keys::{Action, Mode};
use crate::tui::options::{self, Listed, Loading, ModelFile, Options, Pick};
use crate::tui::schema::{self, Field, Gate, Kind, ListKind};
use crate::tui::screens::config::{self, ConfigScreen, Loaded, Outcome, Pane, Test};
use crate::tui::widgets::frame::{self, Header};
use crate::tui::widgets::picker::{self, Picked, Picker};
use crate::tui::widgets::row::{Row, Value as Shown};
use crate::tui::widgets::{fit, progress, wrap, Notice, Tone, Ui};
use crate::tui::worker::Download;
use crate::tui::{complete, tilde, Env};

/// The label column of setup's own rows.
const LABEL_WIDTH: usize = 12;

/// What `t` on read aloud says, with the voice picked.
pub const SAMPLE: &str = "This is how whisrs reads your text aloud.";

/// A step of the wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
    /// Recording (optional, in a build with it): `[recording]`.
    Recording,
    /// Command mode (optional): `[llm]` and its hotkey.
    Command,
    /// Read aloud (optional): `[tts]` and its hotkey.
    ReadAloud,
    /// Jarvis (optional): one `[[agents]]` entry.
    Jarvis,
    Review,
    Checks,
}

impl Step {
    /// The numbered steps, in order: recording only in a build with it.
    pub fn all() -> Vec<Step> {
        let mut steps = vec![
            Step::Backend,
            Step::Key,
            Step::Language,
            Step::Mic,
            Step::Extras,
            Step::Typing,
        ];
        if Gate::Recording.built() {
            steps.push(Step::Recording);
        }
        steps.extend([
            Step::Command,
            Step::ReadAloud,
            Step::Jarvis,
            Step::Review,
            Step::Checks,
        ]);
        steps
    }

    /// How many numbered steps there are.
    pub fn count() -> usize {
        Step::all().len()
    }

    /// Its number in [`Step::all`]; the first question has none.
    pub fn number(self) -> Option<usize> {
        Step::all().iter().position(|s| *s == self).map(|at| at + 1)
    }

    fn after(self) -> Option<Step> {
        let all = Step::all();
        let at = all.iter().position(|s| *s == self)?;
        all.get(at + 1).copied()
    }

    /// A step whose answers go into config.toml, whatever is chosen there.
    fn answers(self) -> bool {
        matches!(
            self,
            Step::Backend | Step::Key | Step::Language | Step::Mic | Step::Extras | Step::Typing
        )
    }

    /// An optional step: it asks first whether to set its mode up.
    pub fn optional(self) -> bool {
        matches!(
            self,
            Step::Recording | Step::Command | Step::ReadAloud | Step::Jarvis
        )
    }

    /// What config.toml calls the optional step's keys.
    fn section(self) -> &'static str {
        match self {
            Step::Recording => "[recording]",
            Step::Command => "[llm]",
            Step::ReadAloud => "[tts]",
            Step::Jarvis => "[[agents]]",
            _ => "",
        }
    }

    /// The optional step's mode, as messages name it.
    fn mode(self) -> &'static str {
        match self {
            Step::Recording => "recording",
            Step::Command => "command mode",
            Step::ReadAloud => "read aloud",
            Step::Jarvis => "Jarvis",
            _ => "",
        }
    }

    /// Every answer the optional step can change: what Set it up keeps, to
    /// put back on a skip.
    fn paths(self) -> &'static [&'static str] {
        match self {
            Step::Recording => &[
                "recording.user_name",
                "recording.backend",
                "hotkeys.recording",
                "groq.api_key",
                "openai.api_key",
                "deepgram.api_key",
            ],
            Step::Command => &["llm.api_url", "llm.model", "llm.api_key", "hotkeys.command"],
            Step::ReadAloud => &[
                "tts.enabled",
                "tts.backend",
                "tts.model",
                "tts.voice",
                "tts.url",
                "tts.api_key",
                "hotkeys.speak",
            ],
            Step::Jarvis => &["agents"],
            _ => &[],
        }
    }
}

/// The answers an optional step's Set it up may change, as they were
/// before it: what a skip puts back.
type Before = Vec<(String, Option<Value>)>;

/// The read-aloud sample: playing, or how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sample {
    /// Being said, as sample `id`.
    Playing {
        id: u64,
    },
    Said,
    Stopped,
    Failed(String),
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
    /// Started by `Enter`: the wizard goes on once it is done.
    go_on: bool,
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
    /// The step (or an optional step's phase) changed: a status the
    /// change did not set itself is cleared.
    pub moved: bool,
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
    /// The optional steps set up, each with its answers as they were
    /// before: a skip puts them back. A step not here is skipped, and its
    /// keys stay as the file has them.
    set_up: BTreeMap<Step, Before>,
    /// The optional step shows its rows, not its question.
    filling: bool,
    /// The key test of the step.
    pub test: Option<Test>,
    /// The read-aloud sample.
    pub sample: Option<Sample>,
    /// The id of the last sample started.
    samples: u64,
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
            set_up: BTreeMap::new(),
            filling: false,
            test: None,
            sample: None,
            samples: 0,
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
            let rows = setup.rows();
            setup.editor.pin_at(rows);
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
        self.editor.form.is_dirty()
            || self.back.iter().any(|s| s.answers())
            || !self.set_up.is_empty()
    }

    /// Whether something is moving on screen: a test, the sample or the
    /// checks.
    pub fn spinning(&self) -> bool {
        self.test == Some(Test::Running)
            || matches!(self.sample, Some(Sample::Playing { .. }))
            || (self.step == Step::Checks && self.checks.is_none())
            || self.editor.tests.values().any(|t| *t == Test::Running)
    }

    fn backend(&self) -> String {
        self.editor.form.config().general.backend.clone()
    }

    /// The rows on screen now: none while an optional step asks its
    /// question.
    fn rows(&self) -> Vec<(&'static Field, String)> {
        if self.step.optional() && !self.filling {
            return Vec::new();
        }
        self.rows_of(self.step)
    }

    /// The rows of `step`, each at its path, for the answers as they are.
    fn rows_of(&self, step: Step) -> Vec<(&'static Field, String)> {
        let f = |path: &str| (schema::row(path), path.to_string());
        let hotkey = |path: &str| (schema::mode_hotkey(path), path.to_string());
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
            Step::Recording => {
                let mut rows = vec![
                    f("recording.user_name"),
                    f("recording.backend"),
                    hotkey("hotkeys.recording"),
                ];
                rows.extend(self.recording_key().map(f));
                rows
            }
            Step::Command => vec![
                f("llm.api_url"),
                f("llm.model"),
                f("llm.api_key"),
                hotkey("hotkeys.command"),
            ],
            Step::ReadAloud => {
                let mut rows = vec![f("tts.backend"), f("tts.model"), f("tts.voice")];
                if matches!(self.tts_backend().as_str(), "tts-sidecar" | "openai-compat") {
                    rows.push(f("tts.url"));
                }
                rows.extend([f("tts.api_key"), hotkey("hotkeys.speak")]);
                rows
            }
            Step::Jarvis => match self.jarvis() {
                Some(n) => [
                    "name",
                    "harness",
                    "dir",
                    "permissions",
                    "feedback",
                    "hotkey",
                    "new_session_hotkey",
                ]
                .into_iter()
                .map(|key| {
                    (
                        schema::entry_row("agents", key),
                        format!("agents.{n}.{key}"),
                    )
                })
                .collect(),
                None => Vec::new(),
            },
            Step::Existing | Step::Review | Step::Checks => Vec::new(),
        }
    }

    /// The key row recording needs beside the dictation one: its backend's,
    /// when that is another provider's.
    fn recording_key(&self) -> Option<&'static str> {
        let name = recording_backend(self.editor.form.config()).ok()?;
        let provider = Provider::of_backend(&name)?;
        (Provider::of_backend(&self.backend()) != Some(provider)).then_some(match provider {
            Provider::Groq => "groq.api_key",
            Provider::OpenAi => "openai.api_key",
            Provider::Deepgram => "deepgram.api_key",
        })
    }

    /// Whether `[tts]` has a key of its own.
    fn tts_own_key(&self) -> bool {
        self.editor
            .form
            .config()
            .tts
            .as_ref()
            .and_then(|t| t.api_key.as_deref())
            .is_some_and(|key| !key.is_empty())
    }

    /// The provider key read aloud relies on: its backend's, while `[tts]
    /// api_key` is empty and the backend is a cloud one.
    fn tts_key(&self) -> Option<&'static str> {
        if self.tts_own_key() {
            return None;
        }
        Some(match Provider::of_backend(&self.tts_backend())? {
            Provider::Groq => "groq.api_key",
            Provider::OpenAi => "openai.api_key",
            Provider::Deepgram => "deepgram.api_key",
        })
    }

    fn tts_backend(&self) -> String {
        self.editor
            .form
            .config()
            .tts
            .as_ref()
            .map_or("groq", |t| t.backend.as_str())
            .to_string()
    }

    /// The entry Jarvis set up: the last of `[[agents]]`.
    fn jarvis(&self) -> Option<usize> {
        if !self.set_up.contains_key(&Step::Jarvis) {
            return None;
        }
        self.editor.form.entries("agents").len().checked_sub(1)
    }

    /// The step is local whisper's model.
    fn model_step(&self) -> bool {
        self.step == Step::Key && matches!(self.backend().as_str(), "local-whisper" | "local")
    }

    /// The key the step tests with `t`.
    fn target(&self) -> Option<Target> {
        match self.step {
            Step::Key => Some(Target::Backend(self.backend())),
            Step::Command if self.filling => Some(Target::Llm),
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
                step if step.optional() && !self.filling => self.choice_key(action),
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

    /// A key the row editor takes, and what follows from a changed answer:
    /// the test or the sample is moot, the mic follows the device, and a
    /// row that comes or goes with a value (the sidecar's URL, recording's
    /// key) does.
    fn editor_key(&mut self, action: Action) -> Reply {
        let device = self.editor.form.config().audio.device.clone();
        let outcome = self.editor.key(action);
        let changed = outcome.flash.is_some();
        let mut reply = Reply::from_outcome(outcome);
        if changed {
            if matches!(self.step, Step::Key | Step::Command) {
                self.test = None;
            }
            if self.step == Step::ReadAloud {
                self.sample = None;
            }
            let now = self.editor.form.config().audio.device.clone();
            if self.step == Step::Mic && now != device {
                reply.mic = Some(Some(now));
            }
            if matches!(self.step, Step::Recording | Step::ReadAloud)
                && self.editor.mode() == Mode::Browse
            {
                let rows = self.rows();
                self.editor.pin_at(rows);
            }
        }
        reply
    }

    fn next(&mut self) -> Reply {
        match self.step {
            Step::Existing => {
                if self.cursor == 0 {
                    if !self.fresh {
                        self.keep_keys();
                    }
                    self.fresh = true;
                    self.go(Step::Backend)
                } else {
                    self.go(Step::Checks)
                }
            }
            step if step.optional() && !self.filling => {
                if self.cursor == 0 {
                    self.start_mode()
                } else {
                    self.skip()
                }
            }
            step if step.optional() => {
                if let Some(why) = self.missing() {
                    return Reply::say(Tone::Warn, why);
                }
                if step == Step::Jarvis {
                    self.absolute_dir();
                }
                let note = self.harness_note();
                let mut reply = self.go_after();
                if let Some(note) = note {
                    reply.status = Some(Notice::new(Tone::Warn, note));
                }
                reply
            }
            Step::Key if self.model_step() => {
                let name = self.whisper_model().name;
                match &self.editor.lists.models {
                    Listed::NotAsked | Listed::Looking => Reply::say(
                        Tone::Muted,
                        "looking for the models already downloaded: enter again in a moment",
                    ),
                    _ if self.has_model(name) => self.go_after(),
                    _ => self.download_model(name, true),
                }
            }
            Step::Review => Reply::effect(Effect::WriteSetup { overwrite: false }),
            Step::Checks => Reply {
                finish: true,
                ..Reply::default()
            },
            _ => self.go_after(),
        }
    }

    /// On to the step after this one.
    fn go_after(&mut self) -> Reply {
        match self.step.after() {
            Some(to) => self.go(to),
            None => Reply::default(),
        }
    }

    fn previous(&mut self) -> Reply {
        // From an optional step's rows, its question.
        if self.step.optional() && self.filling {
            let mut reply = self.leave();
            self.filling = false;
            self.cursor = 0;
            self.editor.pin_at(Vec::new());
            reply.moved = true;
            return reply;
        }
        let Some(to) = self.back.pop() else {
            return Reply::say(Tone::Muted, "this is the first step");
        };
        let mut reply = self.leave();
        self.step = to;
        self.cursor = 0;
        reply.absorb(self.enter());
        reply.moved = true;
        reply
    }

    /// Go on to `to`.
    fn go(&mut self, to: Step) -> Reply {
        let mut reply = self.leave();
        self.back.push(self.step);
        self.step = to;
        self.cursor = 0;
        reply.absorb(self.enter());
        reply.moved = true;
        reply
    }

    /// What leaving the step stops.
    fn leave(&mut self) -> Reply {
        self.test = None;
        self.picker = None;
        let mut reply = Reply::default();
        if let Some(stop) = self.stop_sample() {
            reply.absorb(stop);
        }
        self.sample = None;
        if self.step == Step::Mic {
            reply.mic = Some(None);
        }
        reply
    }

    /// What arriving at the step starts. An optional step shows its rows
    /// if it was set up, else its question.
    fn enter(&mut self) -> Reply {
        self.filling = self.step.optional() && self.set_up.contains_key(&self.step);
        let rows = self.rows();
        self.editor.pin_at(rows);
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
            step if step.optional() => {
                // None of its keys are written: they stay as the file has
                // them.
                self.put_back();
                let said = match self.existing {
                    Some(_) => format!(
                        "skipped {}: {} stays as config.toml has it; whisrs config sets it later",
                        step.mode(),
                        step.section()
                    ),
                    None => format!("skipped {}: whisrs config sets it later", step.mode()),
                };
                let mut reply = self.go_after();
                reply.status = Some(Notice::new(Tone::Muted, said));
                reply
            }
            Step::Checks => self.next(),
            _ => Reply::say(Tone::Muted, "this step is not optional"),
        }
    }

    /// Set it up: keep the answers as they are, to put back on a skip,
    /// start the mode's own (read aloud on, Jarvis's entry), and show its
    /// rows. Set up before, it goes on from its answers as they are.
    fn start_mode(&mut self) -> Reply {
        let step = self.step;
        if !self.set_up.contains_key(&step) {
            let before = step
                .paths()
                .iter()
                .map(|path| (path.to_string(), self.editor.form.value(path).cloned()))
                .collect();
            self.set_up.insert(step, before);
            match step {
                Step::ReadAloud => self.start_read_aloud(),
                Step::Jarvis => self.start_jarvis(),
                _ => {}
            }
        }
        self.filling = true;
        self.cursor = 0;
        let rows = self.rows();
        self.editor.pin_at(rows);
        Reply {
            moved: true,
            ..Reply::default()
        }
    }

    /// Read aloud on, its backend the dictation backend's provider when
    /// that has a voice: its key is then there already. A `[tts] api_key`
    /// kept from the file keeps the backend it is for.
    fn start_read_aloud(&mut self) {
        let _ = self
            .editor
            .form
            .set("tts.enabled", Some(Value::Boolean(true)));
        if self.tts_own_key() {
            return;
        }
        let provider = match self.backend().as_str() {
            "groq" => Some("groq"),
            "openai" | "openai-realtime" => Some("openai"),
            "deepgram" | "deepgram-streaming" => Some("deepgram"),
            _ => None,
        };
        if let Some(provider) = provider {
            let _ = self
                .editor
                .form
                .set("tts.backend", Some(Value::from(provider)));
        }
    }

    /// Jarvis's entry, after the file's own agents, so the write appends
    /// it and changes none of theirs: named `jarvis` unless one is, on the
    /// first harness `PATH` has.
    fn start_jarvis(&mut self) {
        let mut agents: Vec<Value> = self
            .existing
            .as_ref()
            .and_then(|e| Value::try_from(&e.config.agents).ok())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        let taken = agents
            .iter()
            .any(|a| a.get("name").and_then(Value::as_str) == Some("jarvis"));
        let mut entry = toml::Table::new();
        entry.insert(
            "name".into(),
            Value::from(if taken { "" } else { "jarvis" }),
        );
        let on_path = options::harnesses_on_path(&self.env);
        let found = catalog::HARNESSES
            .iter()
            .map(|(name, _)| *name)
            .find(|name| {
                whisrs::agent::Harness::parse(name)
                    .is_some_and(|harness| on_path.contains(harness.default_command()))
            });
        if let Some(harness) = found {
            entry.insert("harness".into(), Value::from(harness));
        }
        agents.push(Value::Table(entry));
        let _ = self.editor.form.set("agents", Some(Value::Array(agents)));
    }

    /// The optional step skipped: its answers back as they were before Set
    /// it up. A key the key step asks now is that step's, and stays.
    fn put_back(&mut self) {
        let Some(before) = self.set_up.remove(&self.step) else {
            return;
        };
        let owned: Vec<String> = self
            .rows_of(Step::Key)
            .into_iter()
            .map(|(_, path)| path)
            .collect();
        for (path, value) in before {
            if !owned.contains(&path) {
                let _ = self.editor.form.set(&path, value);
            }
        }
    }

    /// The file's API keys, into answers that start fresh: a key is not a
    /// default to start over from. Each comes with what says where it goes
    /// (`[llm]`'s URL and model, `[tts]`'s backend and URL, a server's URL),
    /// so a key is never sent somewhere else. Copied once, as the file has
    /// them; the answers are no less clean for them.
    fn keep_keys(&mut self) {
        let Some(file) = self
            .existing
            .as_ref()
            .and_then(|e| Value::try_from(&e.config).ok())
        else {
            return;
        };
        let clean = !self.editor.form.is_dirty();
        for (section, table) in file.as_table().into_iter().flatten() {
            let has_key = table
                .get("api_key")
                .and_then(Value::as_str)
                .is_some_and(|key| !key.is_empty());
            if !has_key {
                continue;
            }
            let with: &[&str] = match section.as_str() {
                "llm" => &["api_url", "model"],
                "tts" => &["backend", "url"],
                "openai-compatible-realtime" | "asr-sidecar" => &["url"],
                _ => &[],
            };
            for key in std::iter::once(&"api_key").chain(with) {
                if let Some(value) = table.get(*key) {
                    let path = format!("{section}.{key}");
                    let _ = self.editor.form.set(&path, Some(value.clone()));
                }
            }
        }
        if clean {
            self.editor.form.mark_saved();
        }
    }

    /// What the mode of this step still needs before `Enter` goes on, with
    /// the way out; `None` when it is complete.
    fn missing(&self) -> Option<String> {
        let config = self.editor.form.config();
        match self.step {
            Step::Recording => {
                let name = match recording_backend(config) {
                    Ok(name) => name,
                    Err(_) => {
                        let backend = config
                            .recording
                            .as_ref()
                            .and_then(|r| r.backend.as_deref())
                            .filter(|b| !b.trim().is_empty())
                            .unwrap_or(&config.general.backend);
                        return Some(format!(
                            "recording needs a backend that takes a file, not {backend}: space \
                             picks one, or s skips recording"
                        ));
                    }
                };
                (!self.key_resolves(&name)).then(|| {
                    format!(
                        "recording needs a key for {name}: space types it, or s skips recording"
                    )
                })
            }
            Step::Command => (!self.llm_key_resolves()).then(|| {
                "command mode needs an LLM key: space types it on api_key, or s skips command \
                 mode"
                    .to_string()
            }),
            Step::ReadAloud => {
                let backend = self.tts_backend();
                let sidecar = matches!(backend.as_str(), "tts-sidecar" | "openai-compat");
                (!sidecar && !self.tts_own_key() && !self.key_resolves(&backend)).then(|| {
                    format!(
                        "read aloud needs a key for {backend}: space types it on api_key, or \
                         s skips read aloud"
                    )
                })
            }
            Step::Jarvis => self.jarvis_missing(),
            _ => None,
        }
    }

    /// Whether backend `name`'s key resolves as the daemon resolves it
    /// (its env var, then config.toml); one without a key needs none.
    fn key_resolves(&self, name: &str) -> bool {
        let mut config = self.editor.form.config().clone();
        config.general.backend = name.to_string();
        keys::key_summary_from(&config, |var| self.env.get(var).map(str::to_string))
            != KeySummary::NotSet
    }

    /// Whether the LLM key resolves as `llm::resolve_api_key` does: `[llm]
    /// api_key`, else `WHISRS_OPENAI_API_KEY` when set (even empty), else
    /// `WHISRS_GROQ_API_KEY`. A server on this machine needs none.
    fn llm_key_resolves(&self) -> bool {
        let llm = self.editor.form.config().llm.clone().unwrap_or_default();
        if local_url(&llm.api_url) || !llm.api_key.is_empty() {
            return true;
        }
        let key = self
            .env
            .get("WHISRS_OPENAI_API_KEY")
            .or_else(|| self.env.get("WHISRS_GROQ_API_KEY"))
            .unwrap_or_default();
        !key.is_empty()
    }

    /// What Jarvis's entry still needs: a valid name no other agent has,
    /// and a dir that exists.
    fn jarvis_missing(&self) -> Option<String> {
        let n = self.jarvis()?;
        let text = |key: &str| {
            self.editor
                .form
                .value(&format!("agents.{n}.{key}"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        let name = text("name");
        if name.is_empty() {
            return Some("Jarvis needs a name: space types one, or s skips Jarvis".to_string());
        }
        if !whisrs::agent::valid_agent_name(&name) {
            return Some(format!(
                "{name} cannot name an agent (a-z, 0-9, _ and -): space types another, or s \
                 skips Jarvis"
            ));
        }
        let taken = (0..n).any(|i| {
            self.editor
                .form
                .value(&format!("agents.{i}.name"))
                .and_then(Value::as_str)
                .is_some_and(|other| other.trim() == name)
        });
        if taken {
            return Some(format!(
                "an agent is named {name} already: space types another name, or s skips Jarvis"
            ));
        }
        let dir = text("dir");
        if dir.is_empty() {
            return Some(
                "Jarvis needs a dir to work in: space types one, or s skips Jarvis".to_string(),
            );
        }
        let full = complete::expand_home(&dir, self.env.get("HOME"));
        (!Path::new(&full).is_dir())
            .then(|| format!("{dir} is not a folder: space types another, or s skips Jarvis"))
    }

    /// Jarvis's dir, accepted relative to this terminal's folder, stored
    /// absolute: the daemon would take it from its own (`$HOME` under
    /// systemd). `~/...` stays as typed; the daemon expands it.
    fn absolute_dir(&mut self) {
        let Some(n) = self.jarvis() else {
            return;
        };
        let path = format!("agents.{n}.dir");
        let Some(dir) = self.editor.form.value(&path).and_then(Value::as_str) else {
            return;
        };
        let dir = dir.trim();
        if dir.starts_with('/') || dir == "~" || dir.starts_with("~/") {
            return;
        }
        let Ok(cwd) = std::env::current_dir() else {
            return;
        };
        let full = whisrs::agent::absolute_dir(dir, Some(&cwd));
        let full = Value::String(full.to_string_lossy().into_owned());
        let _ = self.editor.form.set(&path, Some(full));
    }

    /// Jarvis's harness when `PATH` does not have it: a warning, not a
    /// refusal.
    fn harness_note(&self) -> Option<String> {
        if self.step != Step::Jarvis {
            return None;
        }
        let path = format!("agents.{}.harness", self.jarvis()?);
        let harness = self.editor.form.effective(&path)?.as_str()?.to_string();
        let field = schema::entry_row("agents", "harness");
        let note = options::options(field, &path, &self.editor.form, &self.editor.lists)
            .find(&harness)?
            .note
            .clone()?;
        Some(format!(
            "{harness} is {note}: Jarvis cannot start it until it is"
        ))
    }

    fn test(&mut self) -> Reply {
        if self.step == Step::ReadAloud && self.filling {
            return self.say();
        }
        let Some(target) = self.target() else {
            return Reply::say(
                Tone::Muted,
                "t tests a key on the key and command mode steps, and says a sample on read \
                 aloud",
            );
        };
        self.test = Some(Test::Running);
        Reply::effect(Effect::TestKey {
            origin: crate::tui::app::Origin::Setup,
            path: String::new(),
            target,
        })
    }

    /// `t` on read aloud: say [`SAMPLE`] with the voice picked.
    fn say(&mut self) -> Reply {
        if let Some(why) = self.missing() {
            return Reply::say(Tone::Warn, why);
        }
        self.samples += 1;
        let id = self.samples;
        self.sample = Some(Sample::Playing { id });
        Reply::effect(Effect::Speak { id })
    }

    /// Stop the sample being said, if one is: any key does.
    pub fn stop_sample(&mut self) -> Option<Reply> {
        if !matches!(self.sample, Some(Sample::Playing { .. })) {
            return None;
        }
        self.sample = Some(Sample::Stopped);
        Some(Reply {
            effects: vec![Effect::StopSpeaking],
            status: Some(Notice::new(Tone::Muted, "stopped")),
            ..Reply::default()
        })
    }

    /// Sample `id` ended: said, or why not. One stopped or replaced since
    /// says nothing.
    pub fn spoke(&mut self, id: u64, said: Result<(), String>) -> Option<Notice> {
        if self.sample != Some(Sample::Playing { id }) {
            return None;
        }
        match said {
            Ok(()) => {
                self.sample = Some(Sample::Said);
                None
            }
            Err(why) => {
                self.sample = Some(Sample::Failed(why.clone()));
                Some(Notice::new(
                    Tone::Error,
                    format!("the sample failed: {why}"),
                ))
            }
        }
    }

    /// A key test of the step came back.
    pub fn tested(&mut self, tested: Tested) -> Notice {
        let label = match self.step {
            Step::Command => "LLM key".to_string(),
            _ => format!("{} test", self.backend()),
        };
        let notice = config::test_notice(&label, &tested);
        self.test = Some(Test::Done(tested));
        notice
    }

    /// The first step's choice, or an optional step's question: the first
    /// row or the second.
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

    /// Use model `name`: downloaded, it is set; else it downloads, and
    /// the step stays.
    fn pick_model(&mut self, name: &str) -> Reply {
        if self.has_model(name) {
            if let Some(at) = catalog::WHISPER_MODELS.iter().position(|m| m.name == name) {
                self.model = at;
            }
            self.set_model();
            self.test = None;
            return Reply::say(Tone::Ok, format!("using {}", model::file_name(name)));
        }
        self.download_model(name, false)
    }

    /// Download model `name`, set as the model; with `go_on`, the wizard
    /// goes on once it is done.
    fn download_model(&mut self, name: &str, go_on: bool) -> Reply {
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
        let id = model::next_id();
        self.download = Some(Downloading {
            id,
            name: name.to_string(),
            done: 0,
            total: None,
            before,
            go_on,
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
        self.put_back_model(download.before);
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
    fn put_back_model(&mut self, before: (Option<Value>, usize)) {
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
                // A failed one stays the choice, for Enter to try again;
                // only a cancelled one gives way to the model before it.
                if matches!(result, Err(DownloadError::Cancelled)) {
                    self.put_back_model(download.before);
                }
                let mut reply = Reply::effect(Effect::List(ListKind::Models));
                let status = match result {
                    Ok(path) => {
                        // On disk now, before the listing says so.
                        let file = ModelFile {
                            path: path.clone(),
                            size: download.total.unwrap_or(download.done),
                        };
                        match &mut self.editor.lists.models {
                            Listed::Ready(files) => files.push(file),
                            other => *other = Listed::Ready(vec![file]),
                        }
                        if download.go_on {
                            reply.absorb(self.go_after());
                        }
                        Notice::new(
                            Tone::Ok,
                            format!(
                                "downloaded {}",
                                tilde(&path, self.env.get("HOME")).display()
                            ),
                        )
                    }
                    Err(DownloadError::Cancelled) => Notice::new(
                        Tone::Muted,
                        format!(
                            "cancelled {}: nothing was left behind",
                            model::file_name(&name)
                        ),
                    ),
                    Err(DownloadError::Failed(why)) => Notice::new(
                        Tone::Error,
                        format!("the download failed: {why}; enter tries again"),
                    ),
                };
                reply.status = Some(status);
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
    /// step, and an optional step's only when it was set up: its rows, read
    /// aloud's `enabled`, Jarvis's `[[agents]]` (the file's own entries and
    /// the new one, so only that one is added), and the provider key a mode
    /// counts on (read aloud's, with no `[tts] api_key`), so a mode judged
    /// complete on a key has it written. Each key once.
    pub fn asked(&self) -> Vec<String> {
        let mut keys: Vec<String> = Vec::new();
        for step in Step::all() {
            if !step.answers() && !self.set_up.contains_key(&step) {
                continue;
            }
            let mut add: Vec<String> = match step {
                Step::Jarvis => vec!["agents".to_string()],
                _ => self
                    .rows_of(step)
                    .into_iter()
                    .map(|(_, path)| path)
                    .collect(),
            };
            match step {
                Step::Key if matches!(self.backend().as_str(), "local-whisper" | "local") => {
                    add.push("local-whisper.model_path".to_string());
                }
                Step::Recording => add.extend(self.recording_relies_on().map(str::to_string)),
                Step::ReadAloud => {
                    add.push("tts.enabled".to_string());
                    add.extend(self.tts_key().map(str::to_string));
                }
                _ => {}
            }
            for key in add {
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
        keys
    }

    /// The provider key recording counts on: its backend's, whoever's row
    /// shows it.
    fn recording_relies_on(&self) -> Option<&'static str> {
        let name = recording_backend(self.editor.form.config()).ok()?;
        Some(match Provider::of_backend(&name)? {
            Provider::Groq => "groq.api_key",
            Provider::OpenAi => "openai.api_key",
            Provider::Deepgram => "deepgram.api_key",
        })
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
                    if key == "agents" {
                        return Some(("[[agents]]".to_string(), names(old), names(new)));
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
                        "{} has backend {backend}. Start fresh asks every question again, \
                         keeping the API keys it has, and writes the answers over it; System \
                         checks only looks at the machine.",
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
                    "whisper.cpp runs on this machine, offline. h/l changes the model, space \
                     lists them with their sizes; enter downloads one not on disk yet and goes \
                     on once it is done. The .en ones know English only, and know it better."
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
            Step::Recording => (
                "Recording (optional)".to_string(),
                "Record a call or a meeting, your mic and what the speakers play: whisrs \
                 transcribes it after you stop."
                    .to_string(),
            ),
            Step::Command => (
                "Command mode (optional)".to_string(),
                "Select text, press a hotkey and say what to do: an LLM rewrites it.".to_string(),
            ),
            Step::ReadAloud => (
                "Read aloud (optional)".to_string(),
                "Select text and press a hotkey: whisrs reads it aloud. Jarvis speaks its \
                 replies with it too."
                    .to_string(),
            ),
            Step::Jarvis => {
                let mut text = "Press a hotkey and talk to a coding agent (Claude Code, \
                                OpenCode, Codex or Copilot) working in a folder of yours."
                    .to_string();
                let spoken = self.set_up.contains_key(&Step::ReadAloud)
                    || self
                        .existing
                        .as_ref()
                        .and_then(|e| e.config.tts.as_ref())
                        .is_some_and(|t| t.enabled);
                if !spoken {
                    text.push_str(" Read aloud is not on, so you would not hear its replies.");
                }
                ("Jarvis (optional)".to_string(), text)
            }
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
        for step in Step::all().into_iter().filter(|s| s.optional()) {
            let value = if self.set_up.contains_key(&step) {
                Shown::plain("set up").then(self.mode_summary(step, sep), Tone::Muted)
            } else if self.existing.is_some() {
                Shown::muted("skipped: as config.toml has it")
            } else {
                Shown::muted("skipped")
            };
            rows.push((step.mode().to_string(), value));
        }
        rows
    }

    /// A mode set up, as the review sums it up: its main values.
    fn mode_summary(&self, step: Step, sep: &str) -> String {
        let form = &self.editor.form;
        let text = |path: &str| {
            form.effective(path)
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
                .map(str::to_string)
        };
        let hotkey = |path: &str| text(path).unwrap_or_else(|| "no hotkey".to_string());
        let parts = match step {
            Step::Recording => vec![
                recording_backend(form.config()).unwrap_or_else(|_| "no backend".to_string()),
                hotkey("hotkeys.recording"),
            ],
            Step::Command => {
                let url = text("llm.api_url").unwrap_or_default();
                let host = reqwest::Url::parse(&url)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_string))
                    .unwrap_or(url);
                vec![
                    text("llm.model").unwrap_or_default(),
                    host,
                    hotkey("hotkeys.command"),
                ]
            }
            Step::ReadAloud => {
                let backend = self.tts_backend();
                let voice = text("tts.voice")
                    .or_else(|| whisrs::tts::default_voice(&backend).map(str::to_string))
                    .or_else(|| text("tts.model"))
                    .or_else(|| whisrs::tts::default_model(&backend).map(str::to_string))
                    .unwrap_or_default();
                vec![backend, voice, hotkey("hotkeys.speak")]
            }
            Step::Jarvis => match self.jarvis() {
                Some(n) => ["name", "harness", "dir"]
                    .iter()
                    .filter_map(|key| text(&format!("agents.{n}.{key}")))
                    .collect(),
                None => Vec::new(),
            },
            _ => Vec::new(),
        };
        format!("{sep} {}", parts.join(&format!(" {sep} ")))
    }

    /// What the write will do, and what it would refuse.
    fn review_notes(&self) -> Vec<Notice> {
        let mut notes = Vec::new();
        let local = matches!(self.backend().as_str(), "local-whisper" | "local");
        if local && !self.has_model(self.whisper_model().name) {
            notes.push(Notice::new(
                Tone::Warn,
                format!(
                    "{} is not downloaded, and the daemon cannot start without it: backspace \
                     back to the model step, where enter downloads it",
                    model::file_name(self.whisper_model().name)
                ),
            ));
        }
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
        self.moved |= other.moved;
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

/// The names of a list of `[[agents]]`, as the review shows it.
fn names(agents: Option<&Value>) -> String {
    let names: Vec<&str> = agents
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|entry| {
            entry
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .unwrap_or("unnamed")
        })
        .collect();
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

/// Whether `url` is a server on this machine, which needs no key.
fn local_url(url: &str) -> bool {
    reqwest::Url::parse(url.trim())
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]" | "::1"))
}

/// The backend recordings are transcribed with, or why there is none: one
/// that takes a file (`deepgram-streaming` is `deepgram`).
#[cfg(feature = "recording")]
fn recording_backend(config: &whisrs::Config) -> Result<String, String> {
    whisrs::transcription::factory::recording_backend_name(config).map_err(|e| e.to_string())
}

/// Built without recording, there is no recording step to ask it.
#[cfg(not(feature = "recording"))]
fn recording_backend(_: &whisrs::Config) -> Result<String, String> {
    Err(schema::RECORDING_MISSING.to_string())
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
        wrap(&setup.help(), help_room)
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
        Some(n) => format!("setup {n}/{}", Step::count()),
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
            _ => self.rows().len(),
        }
    }

    /// The help of the row with the cursor; an optional mode's hotkey adds
    /// that it can stay unset, bound in the compositor instead.
    fn help(&self) -> String {
        let mut text = config::row_help(&self.editor);
        let Some((field, path)) = self.editor.focused() else {
            return text;
        };
        if !self.step.optional() || !matches!(field.kind, Kind::Hotkey { .. }) {
            return text;
        }
        let name = || {
            self.jarvis()
                .and_then(|n| {
                    self.editor
                        .form
                        .value(&format!("agents.{n}.name"))
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "<name>".to_string())
        };
        let command = match path.rsplit('.').next().unwrap_or_default() {
            "recording" => "whisrs recording toggle".to_string(),
            "command" => "whisrs command".to_string(),
            "speak" => "whisrs speak".to_string(),
            "new_session_hotkey" => format!("whisrs agent {} --new", name()),
            _ => format!("whisrs agent {}", name()),
        };
        text.push_str(&format!(
            " Set one here, or leave it unset and bind {command} in your compositor."
        ));
        text
    }

    /// An optional step's question: its two rows, each with what it means.
    fn question(&self) -> [(&'static str, String); 2] {
        let kept = |section: &str| match self.existing {
            Some(_) => format!("{section} stays as config.toml has it"),
            None => "whisrs config sets it later".to_string(),
        };
        let theirs: Vec<String> = self
            .existing
            .as_ref()
            .map(|e| {
                e.config
                    .agents
                    .iter()
                    .map(|a| format!("{} ({})", a.name, a.harness))
                    .collect()
            })
            .unwrap_or_default();
        match self.step {
            Step::Recording => [
                ("Set it up", "your name, a backend, a hotkey".to_string()),
                ("Skip", kept("[recording]")),
            ],
            Step::Command => [
                ("Set it up", "an LLM, its key, a hotkey".to_string()),
                ("Skip", kept("[llm]")),
            ],
            Step::ReadAloud => [
                ("Set it up", "a voice, its key, a hotkey".to_string()),
                ("Skip", kept("[tts]")),
            ],
            Step::Jarvis if theirs.is_empty() => [
                (
                    "Set it up",
                    "a name, a coding agent, its folder, hotkeys".to_string(),
                ),
                ("Skip", kept("[[agents]]")),
            ],
            _ => [
                ("Add one", format!("beside {}", theirs.join(", "))),
                ("Keep as it is", theirs.join(", ")),
            ],
        }
    }

    /// The step's lines under the editor's rows: setup's own rows, the
    /// test, the meter, the download.
    fn lines_after(&self, app: &App, width: usize, ui: &Ui) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        // The choice's names are longer than a label.
        let labels = match self.step {
            Step::Existing => "System checks only".len() + 2,
            step if step.optional() && !self.filling => "Keep as it is".len() + 2,
            Step::Review => self
                .review(ui)
                .iter()
                .map(|(label, _)| crate::tui::widgets::width(label) + 2)
                .max()
                .map_or(LABEL_WIDTH, |w| w.max(LABEL_WIDTH)),
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
            step if step.optional() && !self.filling => {
                for (i, (name, about)) in self.question().into_iter().enumerate() {
                    lines.push(row(
                        name,
                        Shown::muted(about),
                        i == self.cursor,
                        false,
                        false,
                    ));
                }
            }
            Step::Key | Step::Command => lines.push(self.test_line(width, ui)),
            Step::ReadAloud => lines.push(self.sample_line(width, ui)),
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
        let rows = if self.editor_rows() > 0 {
            self.rows()
        } else {
            Vec::new()
        };
        rows.iter()
            .map(|(field, _)| crate::tui::widgets::width(field.label) + 2)
            .max()
            .map_or(LABEL_WIDTH, |w| w.min(24))
    }

    /// What `t` does on this step, before it ran.
    fn test_hint(&self) -> &'static str {
        match (self.step, self.backend().as_str()) {
            (Step::Command, _) => "t tests the key, sending no audio",
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
        self.note_line("test", &text, tone, width, ui)
    }

    /// The sample line of read aloud: what `t` says, while it says it, and
    /// how that ended.
    fn sample_line(&self, width: usize, ui: &Ui) -> Line<'static> {
        let (text, tone) = match &self.sample {
            None => (format!("t says \"{SAMPLE}\""), Tone::Muted),
            Some(Sample::Playing { .. }) => (
                format!(
                    "{} speaking: any key stops it",
                    ui.glyphs.spinner[self.spin % ui.glyphs.spinner.len()]
                ),
                Tone::Muted,
            ),
            Some(Sample::Said) => ("said it".to_string(), Tone::Ok),
            Some(Sample::Stopped) => ("stopped".to_string(), Tone::Muted),
            Some(Sample::Failed(why)) => (why.clone(), Tone::Error),
        };
        self.note_line("sample", &text, tone, width, ui)
    }

    /// A line under the rows, lined up with them: `label`, then `text`.
    fn note_line(
        &self,
        label: &str,
        text: &str,
        tone: Tone,
        width: usize,
        ui: &Ui,
    ) -> Line<'static> {
        let labels = self.label_width();
        let room = width.saturating_sub(labels + 2);
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{label:<labels$}"), ui.theme.muted),
            Span::styled(fit(text, room, ui.glyphs.ellipsis), tone.style(ui.theme)),
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
        } else if matches!(self.sample, Some(Sample::Playing { .. })) {
            hints.push(pair("any key", "stop"));
        } else {
            match self.step {
                Step::Existing => {
                    hints.extend([pair("j/k", "move"), pair("enter", "choose")]);
                }
                step if step.optional() && !self.filling => {
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
                    if self.has_model(self.whisper_model().name) {
                        hints.push(pair("enter", "next"));
                    } else {
                        hints.push(pair("enter", "download and go on"));
                    }
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
            if self.step == Step::ReadAloud && self.filling {
                hints.push(pair("t", "say a sample"));
            }
            if self.step.optional() || self.step == Step::Checks {
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
