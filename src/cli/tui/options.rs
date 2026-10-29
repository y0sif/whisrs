//! What a row offers: the values of its picker and of its `h`/`l` steps,
//! each with what it is, what applies while it is unset, and why a value
//! cannot be picked here. Built from the schema, the form as edited (the
//! TTS voices follow `[tts] backend`) and the lists the workers found.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use toml::Value;
use whisrs::config::catalog;
use whisrs::Config;

use super::form::Form;
use super::schema::{self, Field, Kind, ListKind, Opt, Source};
use super::Env;

/// A sound server input, as the worker found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// The name `[audio] device` takes.
    pub id: String,
    pub description: String,
    /// What an output plays, not a microphone.
    pub monitor: bool,
}

/// A whisper model in the models folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFile {
    pub path: PathBuf,
    pub size: u64,
}

/// A list a worker fills.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Listed<T> {
    #[default]
    NotAsked,
    Looking,
    Ready(Vec<T>),
    /// It could not be listed, and why.
    Failed(String),
}

/// The lists the workers found so far, and the harnesses found on `PATH`
/// when an agent's form opened (`None` before one did).
#[derive(Debug, Clone, Default)]
pub struct Lists {
    pub devices: Listed<Device>,
    pub models: Listed<ModelFile>,
    pub on_path: Option<BTreeSet<&'static str>>,
}

/// A worker's answer: a list, or why it could not be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listing {
    Devices(Result<Vec<Device>, String>),
    Models(Result<Vec<ModelFile>, String>),
}

impl Lists {
    /// Whether to ask the worker for `kind`: not while it is on its way.
    /// A list not had yet shows as looked for; one had stays shown while
    /// it is asked again.
    pub fn ask(&mut self, kind: ListKind) -> bool {
        fn mark<T>(listed: &mut Listed<T>) -> bool {
            match listed {
                Listed::Looking => false,
                Listed::Ready(_) => true,
                Listed::NotAsked | Listed::Failed(_) => {
                    *listed = Listed::Looking;
                    true
                }
            }
        }
        match kind {
            ListKind::Devices => mark(&mut self.devices),
            ListKind::Models => mark(&mut self.models),
        }
    }

    /// Whether `kind` is here.
    pub fn ready(&self, kind: ListKind) -> bool {
        match kind {
            ListKind::Devices => matches!(self.devices, Listed::Ready(_)),
            ListKind::Models => matches!(self.models, Listed::Ready(_)),
        }
    }

    /// Whether `kind` was never asked for.
    pub fn not_asked(&self, kind: ListKind) -> bool {
        match kind {
            ListKind::Devices => matches!(self.devices, Listed::NotAsked),
            ListKind::Models => matches!(self.models, Listed::NotAsked),
        }
    }
}

/// One value a row offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    /// What is stored.
    pub value: String,
    /// What the picker shows for it: the value, or a file's name.
    pub label: String,
    pub about: String,
    /// Shown beside it, and it can still be picked (`not on PATH`).
    pub note: Option<String>,
    /// It cannot be picked here: why, short for the list and in full.
    pub refused: Option<(String, String)>,
}

impl Pick {
    fn of((value, about): Opt) -> Pick {
        Pick {
            value: value.to_string(),
            label: value.to_string(),
            about: about.to_string(),
            note: None,
            refused: None,
        }
    }
}

/// Whether a worker's list is still to come.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loading {
    Done,
    Looking,
    /// It failed, and why; typing still works.
    Failed(String),
}

/// What a row offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub picks: Vec<Pick>,
    /// The unset row, saying what then applies; `None` when the key cannot
    /// be unset.
    pub unset: Option<String>,
    /// A typed value that is not listed can be used.
    pub open: bool,
    pub loading: Loading,
}

impl Options {
    fn closed(picks: Vec<Pick>, unset: Option<String>) -> Options {
        Options {
            picks,
            unset,
            open: false,
            loading: Loading::Done,
        }
    }

    /// The values `h`/`l` step through: unset first when it can be, then
    /// every value that can be picked.
    pub fn ring(&self) -> Vec<Option<String>> {
        let unset = self.unset.as_ref().map(|_| None);
        unset
            .into_iter()
            .chain(
                self.picks
                    .iter()
                    .filter(|p| p.refused.is_none())
                    .map(|p| Some(p.value.clone())),
            )
            .collect()
    }

    /// The pick for `value`, if listed.
    pub fn find(&self, value: &str) -> Option<&Pick> {
        self.picks.iter().find(|p| p.value == value)
    }
}

/// What `field` at `path` offers, in the form as edited.
pub fn options(field: &Field, path: &str, form: &Form, lists: &Lists) -> Options {
    let config = form.config();
    match &field.kind {
        Kind::Choice(choices) => Options::closed(choice_picks(path, choices, form, lists), None),
        Kind::MaybeChoice { unset, choices } => {
            let mut picks = choice_picks(path, choices, form, lists);
            if path == "recording.backend" {
                picks.extend(catalog::LIVE_ONLY_BACKENDS.iter().map(|&opt| {
                    let mut pick = Pick::of(opt);
                    pick.refused = Some((
                        "not file-capable".to_string(),
                        whisrs::transcription::live_only_error(opt.0).to_string(),
                    ));
                    pick
                }));
            }
            Options::closed(picks, Some(unset_text(path, unset, config)))
        }
        Kind::MaybeBool { unset } => Options::closed(
            vec![Pick::of(("true", "on")), Pick::of(("false", "off"))]
                .into_iter()
                .map(|mut pick| {
                    pick.label = pick.about.clone();
                    pick.about = String::new();
                    pick
                })
                .collect(),
            Some(unset.to_string()),
        ),
        Kind::Suggest { unset, from, .. } => {
            let (picks, loading) = source_picks(*from, config, lists);
            Options {
                picks,
                unset: unset.map(|unset| unset_text(path, unset, config)),
                open: true,
                loading,
            }
        }
        Kind::Path {
            unset,
            from: Some(from),
            ..
        } => {
            let (picks, loading) = source_picks(*from, config, lists);
            Options {
                picks,
                unset: unset.map(str::to_string),
                open: true,
                loading,
            }
        }
        Kind::List {
            items: Some((from, _)),
            ..
        } => {
            let (picks, loading) = source_picks(*from, config, lists);
            Options {
                picks,
                unset: None,
                open: true,
                loading,
            }
        }
        _ => Options::closed(Vec::new(), None),
    }
}

/// The key names a hotkey can end in, each with a word on the ones that
/// can be bound alone.
pub fn hotkey_keys() -> Options {
    let picks = whisrs::hotkey::KEY_NAMES
        .iter()
        .map(|name| {
            let alone = whisrs::hotkey::parse_hotkey(name).is_ok();
            Pick::of((name, if alone { "can be bound alone" } else { "" }))
        })
        .collect();
    Options::closed(picks, None)
}

/// A closed list's picks, with what this build or this machine says
/// about each.
fn choice_picks(path: &str, choices: &[Opt], form: &Form, lists: &Lists) -> Vec<Pick> {
    choices
        .iter()
        .map(|&opt| {
            let mut pick = Pick::of(opt);
            if matches!(path, "general.backend" | "recording.backend") {
                pick.refused =
                    schema::backend_refusal(opt.0).map(|(short, why)| (short.to_string(), why));
            }
            if path.starts_with("agents.") && path.ends_with(".harness") {
                pick.note = harness_note(path, opt.0, form, lists);
            }
            pick
        })
        .collect()
}

/// `not on PATH` for a harness whose binary was not found there when the
/// form opened, unless the entry names its own `command`.
fn harness_note(path: &str, harness: &str, form: &Form, lists: &Lists) -> Option<String> {
    let found = lists.on_path.as_ref()?;
    let entry = path.trim_end_matches(".harness");
    let own = form
        .value(&format!("{entry}.command"))
        .and_then(Value::as_str)
        .is_some_and(|c| !c.trim().is_empty());
    let binary = whisrs::agent::Harness::parse(harness)?.default_command();
    (!own && !found.contains(binary)).then(|| "not on PATH".to_string())
}

/// The harness binaries `env`'s `PATH` has.
pub fn harnesses_on_path(env: &Env) -> BTreeSet<&'static str> {
    whisrs::agent::Harness::ALL
        .iter()
        .map(|harness| harness.default_command())
        .filter(|binary| super::find_on_path(env, binary).is_some())
        .collect()
}

/// What applies while `path` is unset: its `unset` words, with the value
/// they come to (`[general] language (en)`).
pub fn unset_text(path: &str, unset: &str, config: &Config) -> String {
    let tts_backend = config.tts.as_ref().map_or("groq", |t| t.backend.as_str());
    let recording = config.recording.clone().unwrap_or_default();
    let value = match path {
        "tts.model" => whisrs::tts::default_model(tts_backend).map(str::to_string),
        "tts.voice" => match whisrs::tts::default_voice(tts_backend) {
            Some(voice) => Some(voice.to_string()),
            None => return "not used: the voice is in the model".to_string(),
        },
        "recording.backend" => Some(config.general.backend.clone()),
        "recording.language" => Some(config.general.language.clone()),
        "recording.mic_language" | "recording.system_language" => Some(
            recording
                .language
                .clone()
                .unwrap_or_else(|| config.general.language.clone()),
        ),
        "recording.mic_device" => Some(config.audio.device.clone()),
        _ => None,
    };
    match value {
        Some(value) => format!("{unset} ({value})"),
        None => unset.to_string(),
    }
}

/// A source's picks, and whether its list is still coming.
fn source_picks(from: Source, config: &Config, lists: &Lists) -> (Vec<Pick>, Loading) {
    let list = |opts: &[Opt]| opts.iter().copied().map(Pick::of).collect::<Vec<_>>();
    let tts_backend = config.tts.as_ref().map_or("groq", |t| t.backend.as_str());
    match from {
        Source::List(opts) => (list(opts), Loading::Done),
        Source::Languages { auto } => {
            let mut picks = Vec::new();
            if auto {
                picks.push(Pick::of(catalog::AUTO_LANGUAGE));
            }
            picks.extend(list(catalog::LANGUAGES));
            (picks, Loading::Done)
        }
        Source::TtsModels => (list(catalog::tts_models(tts_backend)), Loading::Done),
        Source::TtsVoices => (list(catalog::tts_voices(tts_backend)), Loading::Done),
        Source::LlmModels => {
            let url = config
                .llm
                .as_ref()
                .map(|l| l.api_url.clone())
                .unwrap_or_else(|| whisrs::llm::LlmConfig::default().api_url);
            (list(catalog::llm_models(&url)), Loading::Done)
        }
        Source::Inputs => {
            let mut picks = vec![Pick::of(("default", "the system's default input"))];
            let loading = loading_of(&lists.devices);
            if let Listed::Ready(devices) = &lists.devices {
                // Microphones first, then what the outputs play.
                let (monitors, mics): (Vec<&Device>, Vec<&Device>) =
                    devices.iter().partition(|d| d.monitor);
                picks.extend(mics.into_iter().map(device_pick));
                picks.extend(monitors.into_iter().map(|d| {
                    let mut pick = device_pick(d);
                    pick.about = format!("{} (monitor)", pick.about);
                    pick
                }));
            }
            (picks, loading)
        }
        Source::Monitors => {
            let mut picks = vec![Pick::of(("none", "the mic only"))];
            let loading = loading_of(&lists.devices);
            if let Listed::Ready(devices) = &lists.devices {
                picks.extend(devices.iter().filter(|d| d.monitor).map(device_pick));
            }
            (picks, loading)
        }
        Source::ModelFiles => {
            let loading = loading_of(&lists.models);
            let picks = match &lists.models {
                Listed::Ready(files) => files.iter().map(model_pick).collect(),
                _ => Vec::new(),
            };
            (picks, loading)
        }
    }
}

fn loading_of<T>(listed: &Listed<T>) -> Loading {
    match listed {
        Listed::NotAsked | Listed::Ready(_) => Loading::Done,
        Listed::Looking => Loading::Looking,
        Listed::Failed(why) => Loading::Failed(why.clone()),
    }
}

fn device_pick(device: &Device) -> Pick {
    Pick {
        value: device.id.clone(),
        label: device.id.clone(),
        about: device.description.clone(),
        note: None,
        refused: None,
    }
}

fn model_pick(file: &ModelFile) -> Pick {
    let name = file
        .path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    Pick {
        value: file.path.to_string_lossy().into_owned(),
        label: name,
        about: size(file.size),
        note: None,
        refused: None,
    }
}

/// A file size as people read it: `142 MB`, `1.6 GB`.
pub fn size(bytes: u64) -> String {
    const MB: f64 = 1_000_000.0;
    let mb = bytes as f64 / MB;
    if mb >= 1000.0 {
        format!("{:.1} GB", mb / 1000.0)
    } else if mb >= 1.0 {
        format!("{mb:.0} MB")
    } else {
        format!("{:.0} kB", bytes as f64 / 1000.0)
    }
}

/// The `ggml-*.bin` files in `dir`, by name; `home` shortens the folder
/// to `~` in what it says.
pub fn model_files(dir: &Path, home: Option<&str>) -> Result<Vec<ModelFile>, String> {
    let shown = super::tilde(dir, home);
    let entries = std::fs::read_dir(dir).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!(
            "no models in {} yet: whisrs setup downloads one",
            shown.display()
        ),
        _ => format!("cannot read {}: {e}", shown.display()),
    })?;
    let mut files: Vec<ModelFile> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("ggml-") && name.ends_with(".bin")
        })
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            meta.is_file().then(|| ModelFile {
                path: entry.path(),
                size: meta.len(),
            })
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    if files.is_empty() {
        return Err(format!(
            "no ggml-*.bin in {}: whisrs setup downloads one",
            shown.display()
        ));
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(toml: &str) -> Form {
        Form::new(toml::from_str(toml).unwrap()).unwrap()
    }

    fn field(path: &str) -> &'static Field {
        schema::SECTIONS
            .iter()
            .flat_map(|s| match s.body {
                schema::Body::Rows(fields) => fields.iter(),
                schema::Body::Entries(e) => e.fields.iter(),
                schema::Body::Map(_) => [].iter(),
            })
            .find(|f| f.path == path)
            .unwrap_or_else(|| panic!("no row {path}"))
    }

    fn values(options: &Options) -> Vec<&str> {
        options.picks.iter().map(|p| p.value.as_str()).collect()
    }

    /// The TTS lists follow the backend as edited.
    #[test]
    fn tts_models_and_voices_follow_the_backend() {
        let lists = Lists::default();
        let groq = form("");
        let model = options(field("tts.model"), "tts.model", &groq, &lists);
        assert_eq!(values(&model)[0], "canopylabs/orpheus-v1-english");
        assert_eq!(
            model.unset.as_deref(),
            Some("backend default (canopylabs/orpheus-v1-english)")
        );
        let openai = form("[tts]\nbackend = \"openai\"\n");
        let voice = options(field("tts.voice"), "tts.voice", &openai, &lists);
        assert!(values(&voice).contains(&"marin"));
        assert_eq!(voice.unset.as_deref(), Some("backend default (alloy)"));
        let deepgram = form("[tts]\nbackend = \"deepgram\"\n");
        let voice = options(field("tts.voice"), "tts.voice", &deepgram, &lists);
        assert!(voice.picks.is_empty());
        assert_eq!(
            voice.unset.as_deref(),
            Some("not used: the voice is in the model")
        );
    }

    #[test]
    fn llm_models_follow_the_url_host() {
        let lists = Lists::default();
        let groq = form("[llm]\napi_url = \"https://api.groq.com/openai/v1/chat/completions\"\n");
        let models = options(field("llm.model"), "llm.model", &groq, &lists);
        assert!(values(&models).contains(&"openai/gpt-oss-20b"));
        let own = form("[llm]\napi_url = \"http://10.0.0.2:8000/v1/chat/completions\"\n");
        let models = options(field("llm.model"), "llm.model", &own, &lists);
        assert!(models.picks.is_empty() && models.open);
    }

    /// The unset row names what then applies.
    #[test]
    fn unset_says_what_applies() {
        let lists = Lists::default();
        let f = form("[general]\nlanguage = \"de\"\n[recording]\nlanguage = \"fr\"\n");
        let language = options(
            field("recording.language"),
            "recording.language",
            &f,
            &lists,
        );
        assert_eq!(language.unset.as_deref(), Some("[general] language (de)"));
        let mic = options(
            field("recording.mic_language"),
            "recording.mic_language",
            &f,
            &lists,
        );
        assert_eq!(mic.unset.as_deref(), Some("as language (fr)"));
        let backend = options(field("recording.backend"), "recording.backend", &f, &lists);
        assert_eq!(backend.unset.as_deref(), Some("same as dictation (groq)"));
        // The realtime backends are there, and refused with the reason.
        let realtime = backend.find("openai-realtime").unwrap();
        let (short, why) = realtime.refused.clone().unwrap();
        assert_eq!(short, "not file-capable");
        assert!(why.contains("only transcribes live audio"), "{why}");
        assert!(!backend.ring().contains(&Some("openai-realtime".into())));
    }

    /// Microphones, then monitors, marked; the system device lists only
    /// monitors.
    #[test]
    fn devices_list_microphones_then_monitors() {
        let lists = Lists {
            devices: Listed::Ready(vec![
                Device {
                    id: "alsa_output.speaker.monitor".into(),
                    description: "Monitor of Speaker".into(),
                    monitor: true,
                },
                Device {
                    id: "alsa_input.usb-yeti".into(),
                    description: "Blue Yeti".into(),
                    monitor: false,
                },
            ]),
            ..Lists::default()
        };
        let f = form("");
        let inputs = options(field("audio.device"), "audio.device", &f, &lists);
        assert_eq!(
            values(&inputs),
            [
                "default",
                "alsa_input.usb-yeti",
                "alsa_output.speaker.monitor"
            ]
        );
        assert_eq!(inputs.picks[2].about, "Monitor of Speaker (monitor)");
        let system = options(
            field("recording.system_device"),
            "recording.system_device",
            &f,
            &lists,
        );
        assert_eq!(values(&system), ["none", "alsa_output.speaker.monitor"]);

        let looking = Lists {
            devices: Listed::Looking,
            ..Lists::default()
        };
        let inputs = options(field("audio.device"), "audio.device", &f, &looking);
        assert_eq!(inputs.loading, Loading::Looking);
        assert_eq!(values(&inputs), ["default"]);
    }

    #[test]
    fn a_harness_not_on_path_is_noted_unless_the_entry_names_its_command() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("codex");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = Env::from_pairs(&[("PATH", dir.path().to_str().unwrap())]);
        let f = form("[[agents]]\nname = \"j\"\n");
        // Not looked up yet: nothing is said.
        let unknown = options(field("harness"), "agents.0.harness", &f, &Lists::default());
        assert_eq!(unknown.find("claude").unwrap().note, None);
        let lists = Lists {
            on_path: Some(harnesses_on_path(&env)),
            ..Lists::default()
        };
        assert_eq!(lists.on_path.as_ref().unwrap().len(), 1);
        let harness = options(field("harness"), "agents.0.harness", &f, &lists);
        assert_eq!(
            harness.find("claude").unwrap().note.as_deref(),
            Some("not on PATH")
        );
        assert_eq!(harness.find("codex").unwrap().note, None);
        let own = form("[[agents]]\nname = \"j\"\ncommand = \"/opt/claude\"\n");
        let harness = options(field("harness"), "agents.0.harness", &own, &lists);
        assert_eq!(harness.find("claude").unwrap().note, None);
    }

    #[test]
    fn model_files_are_the_ggml_bins_by_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ggml-small.en.bin"), vec![0u8; 2_000_000]).unwrap();
        std::fs::write(dir.path().join("ggml-base.en.bin"), b"x").unwrap();
        std::fs::write(dir.path().join("ggml-base.en.bin.part"), b"x").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"x").unwrap();
        let files = model_files(dir.path(), None).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["ggml-base.en.bin", "ggml-small.en.bin"]);
        assert_eq!(size(files[1].size), "2 MB");
        assert!(model_files(&dir.path().join("none"), None)
            .unwrap_err()
            .contains("whisrs setup downloads one"));
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(size(142_000_000), "142 MB");
        assert_eq!(size(1_620_000_000), "1.6 GB");
        assert_eq!(size(900), "1 kB");
    }

    /// Every key a hotkey picker offers parses as a hotkey's key.
    #[test]
    fn hotkey_keys_all_parse() {
        let keys = hotkey_keys();
        for pick in &keys.picks {
            assert!(
                whisrs::hotkey::parse_hotkey(&format!("Super+{}", pick.value)).is_ok(),
                "{}",
                pick.value
            );
        }
        assert_eq!(keys.find("F13").unwrap().about, "can be bound alone");
        assert_eq!(keys.find("D").unwrap().about, "");
    }
}
