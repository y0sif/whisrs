//! Setup, driven by keys through the pure update: the steps in order, back
//! and skip, the questions before leaving, the model download, the checks,
//! and the #134 refusals.

use std::path::PathBuf;

use ratatui::crossterm::event::KeyCode;
use whisrs::config::setup::key_test::{Outcome as Checked, Source, Target, Tested};
use whisrs::config::setup::keybind::{Binding, Compositor};
use whisrs::config::setup::model::DownloadError;
use whisrs::config::setup::uinput::Uinput;
use whisrs::config::setup::unit::UnitFile;
use whisrs::service::ServiceManager;

use super::*;
use crate::tui::app::{Dialog, Effect, Msg, Origin, Screen};
use crate::tui::options::{Listing, ModelFile};
use crate::tui::screens::config::{reload, save_keys};
use crate::tui::testing::{
    assert_snapshot, draw, key, paths, press, rows, setup_on, type_text, Fixture,
};
use crate::tui::worker::Download;

fn setup(fixture: &Fixture) -> &Setup {
    fixture.app.setup.as_ref().expect("setup is open")
}

fn setup_mut(fixture: &mut Fixture) -> &mut Setup {
    fixture.app.setup.as_mut().expect("setup is open")
}

fn drawn(fixture: &Fixture, w: u16, h: u16) -> Vec<String> {
    rows(&draw(&fixture.app, w, h))
}

fn status(fixture: &Fixture) -> String {
    fixture
        .app
        .status
        .as_ref()
        .map(|s| s.text.clone())
        .unwrap_or_default()
}

/// A fresh setup on its key step, a Groq key typed and tested.
fn on_the_key_step() -> Fixture {
    let mut fixture = setup_on(None, &[]);
    press(&mut fixture.app, "\n");
    type_text(&mut fixture.app, "gsk_test_0123456789");
    press(&mut fixture.app, "\n");
    fixture.app.update(Msg::KeyTested {
        origin: Origin::Setup,
        path: String::new(),
        tested: Tested {
            outcome: Checked::Valid { ms: 214 },
            source: Source::Config,
        },
    });
    fixture
}

/// Write what setup holds, as the loop does for [`Effect::WriteSetup`].
fn write(fixture: &mut Fixture) -> Option<Effect> {
    let paths = paths(fixture.dir.path());
    let setup = setup(fixture);
    let written = save_keys(&setup.editor, &setup.asked(), true, &paths, false);
    let again = written.as_ref().ok().and_then(|_| reload(&paths).ok());
    fixture.app.update(Msg::SetupWritten(written, again))
}

/// The id of the download `effects` started, of model `name`.
fn started(effects: Vec<Effect>, name: &str) -> u64 {
    match effects.as_slice() {
        [Effect::Download { name: started, id }] if started == name => *id,
        other => panic!("no download of {name}: {other:?}"),
    }
}

fn checks() -> Checks {
    Checks {
        uinput: Uinput::Denied,
        manager: ServiceManager::Systemd,
        unit: UnitFile::Stale {
            binary: "/usr/bin/whisrsd".into(),
            new: "/home/ada/.cargo/bin/whisrsd".into(),
        },
        unit_path: Some(PathBuf::from(
            "/home/ada/.config/systemd/user/whisrs.service",
        )),
        enabled: true,
        active: true,
        whisrsd: "/home/ada/.cargo/bin/whisrsd".into(),
        whisrs: "/home/ada/.cargo/bin/whisrs".into(),
        compositor: Some(Compositor::Hyprland),
        binding: Some(Binding::Unbound(PathBuf::from(
            "/home/ada/.config/hypr/hyprland.conf",
        ))),
        gnome: None,
    }
}

/// With no config, setup starts at the backend, and the steps go in
/// order to the write and the checks; the answers land in config.toml,
/// 0600, and the farewell says what to know.
#[test]
fn a_fresh_setup_goes_step_by_step_and_writes_the_answers() {
    let mut fixture = setup_on(None, &[]);
    assert_eq!(fixture.app.screen(), Screen::Setup);
    assert_eq!(setup(&fixture).step, Step::Backend);
    assert!(drawn(&fixture, 90, 30)
        .iter()
        .any(|r| r.contains("setup 1/9")));

    // The key step opens its input at once: there is no key anywhere.
    press(&mut fixture.app, "\n");
    assert_eq!(setup(&fixture).step, Step::Key);
    assert_eq!(setup(&fixture).mode(), Mode::Text);
    type_text(&mut fixture.app, "gsk_test_0123456789");
    press(&mut fixture.app, "\n");
    assert_eq!(setup(&fixture).mode(), Mode::Browse);
    assert_eq!(
        press(&mut fixture.app, "t"),
        [Effect::TestKey {
            origin: Origin::Setup,
            path: String::new(),
            target: Target::Backend("groq".into()),
        }]
    );
    assert_eq!(setup(&fixture).test, Some(Test::Running));

    // The language from its picker.
    press(&mut fixture.app, "\n ");
    assert_eq!(setup(&fixture).mode(), Mode::Picker);
    type_text(&mut fixture.app, "ger");
    press(&mut fixture.app, "\n");
    assert_eq!(setup(&fixture).editor.form.config().general.language, "de");

    // The microphone opens with the step, and stops after it.
    let effects = press(&mut fixture.app, "\n");
    assert_eq!(setup(&fixture).step, Step::Mic);
    assert_eq!(
        effects,
        [Effect::All(vec![
            Effect::List(ListKind::Devices),
            Effect::MicStart {
                session: 1,
                device: "default".into(),
            },
        ])]
    );
    assert_eq!(press(&mut fixture.app, "\n"), [Effect::MicStop]);

    // Extras: the overlay on, and its theme colours the TUI at once.
    assert_eq!(setup(&fixture).step, Step::Extras);
    press(&mut fixture.app, "jj j");
    assert!(setup(&fixture).editor.form.config().general.overlay || !Gate::Overlay.built());
    press(&mut fixture.app, "l");
    assert_eq!(fixture.app.overlay().theme, "ember");

    // Typing, then the LLM step skipped.
    press(&mut fixture.app, "\n\n");
    assert_eq!(setup(&fixture).step, Step::Llm);
    press(&mut fixture.app, "s");
    assert_eq!(setup(&fixture).step, Step::Review);
    let review = drawn(&fixture, 90, 30);
    assert!(
        review.iter().any(|r| r.contains("set ****6789")),
        "the rendered screen differs"
    );
    assert!(review
        .iter()
        .any(|r| r.contains("language") && r.contains("de")));

    assert_eq!(
        press(&mut fixture.app, "\n"),
        [Effect::WriteSetup { overwrite: false }]
    );
    assert_eq!(write(&mut fixture), Some(Effect::Checks));
    assert_eq!(setup(&fixture).step, Step::Checks);
    let file = paths(fixture.dir.path()).config;
    let text = std::fs::read_to_string(&file).unwrap();
    let written: whisrs::Config = toml::from_str(&text).unwrap();
    assert_eq!(written.general.language, "de");
    assert!(written.general.remove_filler_words && written.general.audio_feedback);
    assert_eq!(written.overlay.map(|o| o.theme).as_deref(), Some("ember"));
    assert!(written.llm.is_none());
    assert!(
        written
            .groq
            .is_some_and(|g| g.api_key == "gsk_test_0123456789"),
        "the key was written"
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // Config edits on from the file written.
    assert_eq!(fixture.app.config.form.config().general.language, "de");

    fixture.app.update(Msg::Checked(Box::new(checks())));
    assert_eq!(press(&mut fixture.app, "\n"), [Effect::Quit]);
    assert!(fixture.app.setup.is_none());
    assert_eq!(fixture.app.farewell[0], "You're all set.");
}

/// Backspace is the step before, but erases in an input; the first step
/// has none before it.
#[test]
fn backspace_goes_back_unless_typing() {
    let mut fixture = setup_on(None, &[]);
    press(&mut fixture.app, "\n");
    type_text(&mut fixture.app, "ab");
    key(&mut fixture.app, KeyCode::Backspace);
    assert_eq!(setup(&fixture).step, Step::Key);
    assert_eq!(setup(&fixture).mode(), Mode::Text);
    // Esc closes the input; then Backspace goes back.
    press(&mut fixture.app, "\x1b");
    assert_eq!(setup(&fixture).step, Step::Key);
    key(&mut fixture.app, KeyCode::Backspace);
    assert_eq!(setup(&fixture).step, Step::Backend);
    key(&mut fixture.app, KeyCode::Backspace);
    assert_eq!(status(&fixture), "this is the first step");
    // s skips only an optional step.
    press(&mut fixture.app, "s");
    assert_eq!(status(&fixture), "this step is not optional");
}

/// Unwritten answers are asked about, on q and on Esc; none, and both go.
#[test]
fn leaving_with_unwritten_answers_asks_first() {
    let mut fixture = setup_on(None, &[]);
    press(&mut fixture.app, "l");
    assert!(setup(&fixture).unwritten());
    assert!(press(&mut fixture.app, "q").is_empty());
    assert_eq!(fixture.app.dialog, Some(Dialog::Unwritten { quit: true }));
    press(&mut fixture.app, "\x1b");
    assert_eq!(fixture.app.dialog, None);
    assert!(press(&mut fixture.app, "\x1b").is_empty());
    assert_eq!(fixture.app.dialog, Some(Dialog::Unwritten { quit: false }));
    assert!(press(&mut fixture.app, "d").is_empty());
    assert_eq!(fixture.app.screen(), Screen::Home);
    assert!(fixture.app.setup.is_none());

    let mut fixture = setup_on(None, &[]);
    press(&mut fixture.app, "l");
    press(&mut fixture.app, "q");
    assert_eq!(press(&mut fixture.app, "d"), [Effect::Quit]);

    // Nothing answered: Esc goes straight to Home.
    let mut fixture = setup_on(None, &[]);
    press(&mut fixture.app, "\x1b");
    assert_eq!(fixture.app.screen(), Screen::Home);
}

/// With a config there, setup asks first: start fresh, or only the
/// checks.
#[test]
fn a_config_on_disk_offers_start_fresh_or_checks_only() {
    let toml = "[general]\nbackend = \"deepgram\"\n[deepgram]\napi_key = \"dg_key_1\"\n\
                [hotkeys]\ntoggle = \"Alt+D\"\n";
    let mut fixture = setup_on(Some(toml), &[]);
    assert_eq!(setup(&fixture).step, Step::Existing);
    let first = drawn(&fixture, 90, 30);
    assert!(
        first.iter().any(|r| r.contains("has backend deepgram")),
        "the rendered screen differs"
    );
    assert_eq!(
        setup(&fixture).existing.as_ref().unwrap().backend,
        "deepgram"
    );

    assert_eq!(press(&mut fixture.app, "j\n"), [Effect::Checks]);
    assert_eq!(setup(&fixture).step, Step::Checks);
    // Nothing to write: leaving does not ask.
    assert!(!setup(&fixture).unwritten());
    key(&mut fixture.app, KeyCode::Backspace);
    assert_eq!(setup(&fixture).step, Step::Existing);
    press(&mut fixture.app, "k\n");
    assert_eq!(setup(&fixture).step, Step::Backend);
    // Fresh answers start from the defaults, not the file.
    assert_eq!(setup(&fixture).editor.form.config().general.backend, "groq");
}

/// Each check row's Space does its fix: sudo for uinput, the repoint, the
/// keybinding after a yes.
#[test]
fn the_checks_fix_what_they_can() {
    let mut fixture = setup_on(Some("[general]\n"), &[("HOME", "/home/ada")]);
    press(&mut fixture.app, "j\n");
    let looking = drawn(&fixture, 90, 30);
    assert!(looking.iter().any(|r| r.contains("looking...")));
    assert!(fixture.app.spinning());
    fixture.app.update(Msg::Checked(Box::new(checks())));
    assert!(!fixture.app.spinning());
    assert_eq!(press(&mut fixture.app, " "), [Effect::FixUinput]);
    assert_eq!(
        press(&mut fixture.app, "j "),
        [Effect::System(SystemAction::Repoint)]
    );
    assert!(press(&mut fixture.app, "j ").is_empty());
    match &fixture.app.dialog {
        Some(Dialog::Bind { line, path }) => {
            assert_eq!(
                line,
                "bind = $mainMod, W, exec, /home/ada/.cargo/bin/whisrs toggle"
            );
            assert_eq!(path, "~/.config/hypr/hyprland.conf");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        press(&mut fixture.app, "y"),
        [Effect::System(SystemAction::Bind)]
    );
    // A no adds nothing.
    press(&mut fixture.app, " ");
    assert!(press(&mut fixture.app, "n").is_empty());
    assert_eq!(status(&fixture), "not added");

    // After the fix, a still-denied uinput needs a new login.
    assert_eq!(
        fixture.app.update(Msg::Fixed(Vec::new())),
        Some(Effect::Checks)
    );
    fixture.app.update(Msg::Checked(Box::new(checks())));
    assert!(drawn(&fixture, 90, 30)
        .iter()
        .any(|r| r.contains("log out and back in")));

    // A repoint restarts the daemon, through the gate, when it runs.
    fixture
        .app
        .update(Msg::Daemon(crate::tui::worker::DaemonView::Up(
            whisrs::InfoAnswer::Older(whisrs::State::Idle),
        )));
    assert_eq!(
        fixture.app.update(Msg::SystemDone(
            SystemAction::Repoint,
            Ok(vec!["repointed".into()])
        )),
        Some(Effect::All(vec![Effect::Checks, Effect::Restart]))
    );
}

/// Local whisper's model step: the picker marks what is downloaded, a
/// pick downloads, Esc cancels it, a downloaded one is just used.
#[test]
fn a_whisper_model_is_picked_and_downloaded() {
    let mut fixture = setup_on(None, &[]);
    setup_mut(&mut fixture)
        .editor
        .form
        .set("general.backend", Some(Value::from("local-whisper")))
        .unwrap();
    assert_eq!(
        press(&mut fixture.app, "\n"),
        [Effect::List(ListKind::Models)]
    );
    let models = paths(fixture.dir.path()).models;
    let path = |name: &str| models.join(format!("ggml-{name}.bin"));
    let set = |fixture: &Fixture| {
        setup(fixture)
            .editor
            .form
            .config()
            .local_whisper
            .as_ref()
            .map(|l| l.model_path.clone())
    };
    assert_eq!(set(&fixture), Some(path("base.en").display().to_string()));
    fixture
        .app
        .update(Msg::Listed(Listing::Models(Ok(vec![ModelFile {
            path: path("base.en"),
            size: 147_964_211,
        }]))));

    press(&mut fixture.app, " ");
    assert_eq!(setup(&fixture).mode(), Mode::Picker);
    let picker = drawn(&fixture, 90, 30);
    assert!(
        picker
            .iter()
            .any(|r| r.contains("base.en") && r.contains("downloaded")),
        "the rendered screen differs"
    );
    type_text(&mut fixture.app, "tiny.en");
    let id = started(press(&mut fixture.app, "\n"), "tiny.en");
    assert_eq!(set(&fixture), Some(path("tiny.en").display().to_string()));
    fixture.app.update(Msg::Download(Download::Progress {
        id,
        done: 38_852_357,
        total: Some(77_704_715),
    }));
    assert_snapshot("setup-model-90x30", &drawn(&fixture, 90, 30));
    // Busy: the next step waits.
    press(&mut fixture.app, "\n");
    assert_eq!(setup(&fixture).step, Step::Key);
    let part = whisrs::config::setup::model::part_path(&models, "tiny.en", id);
    assert_eq!(setup(&fixture).partial(), Some((id, part.clone())));
    // Esc ends it on screen at once, the model before it back; its worker
    // removes its own `.part`, and the way out does if the TUI goes first.
    assert_eq!(
        press(&mut fixture.app, "\x1b"),
        [Effect::CancelDownload(id)]
    );
    assert!(setup(&fixture).download.is_none());
    assert!(status(&fixture).contains("nothing is left behind"));
    assert_eq!(set(&fixture), Some(path("base.en").display().to_string()));
    assert_eq!(fixture.app.orphans, [(id, part)]);
    // Its late news changes nothing, and its end clears the orphan.
    fixture.app.update(Msg::Download(Download::Progress {
        id,
        done: 1,
        total: None,
    }));
    assert!(setup(&fixture).download.is_none());
    fixture.app.update(Msg::Download(Download::Done {
        id,
        result: Err(DownloadError::Cancelled),
    }));
    assert!(fixture.app.orphans.is_empty());

    // A failed download puts the model before it back too.
    press(&mut fixture.app, " ");
    type_text(&mut fixture.app, "small.en");
    let failing = started(press(&mut fixture.app, "\n"), "small.en");
    assert_eq!(set(&fixture), Some(path("small.en").display().to_string()));
    fixture.app.update(Msg::Download(Download::Done {
        id: failing,
        result: Err(DownloadError::Failed("HTTP 404".into())),
    }));
    assert!(status(&fixture).contains("the download failed"));
    assert_eq!(set(&fixture), Some(path("base.en").display().to_string()));

    // A model already there is used as it is.
    press(&mut fixture.app, " ");
    type_text(&mut fixture.app, "base.en");
    assert!(press(&mut fixture.app, "\n").is_empty());
    assert_eq!(status(&fixture), "using ggml-base.en.bin");
    assert_eq!(set(&fixture), Some(path("base.en").display().to_string()));
}

/// Leaving setup with a download running cancels it, and keeps the
/// `.part` to remove on the way out.
#[test]
fn leaving_mid_download_cancels_it() {
    let mut fixture = setup_on(None, &[]);
    setup_mut(&mut fixture)
        .editor
        .form
        .set("general.backend", Some(Value::from("local-whisper")))
        .unwrap();
    press(&mut fixture.app, "\n ");
    type_text(&mut fixture.app, "small.en");
    let id = started(press(&mut fixture.app, "\n"), "small.en");
    press(&mut fixture.app, "q");
    assert_eq!(fixture.app.dialog, Some(Dialog::Unwritten { quit: true }));
    assert_eq!(
        press(&mut fixture.app, "d"),
        [Effect::All(vec![Effect::CancelDownload(id), Effect::Quit])]
    );
    assert_eq!(fixture.app.orphans.len(), 1);
    fixture.app.update(Msg::Download(Download::Done {
        id,
        result: Err(DownloadError::Cancelled),
    }));
    assert!(fixture.app.orphans.is_empty());
}

/// A config that does not load is not refused: setup says so, Config
/// refuses to save over it, and the write keeps it as `.bak` (#134).
#[test]
fn an_undeserializable_config_is_backed_up_by_the_write() {
    let broken = "[general]\nsilence_timeout_ms = \"soon\"\n";
    let mut fixture = setup_on(Some(broken), &[]);
    assert_eq!(setup(&fixture).step, Step::Backend);
    assert!(fixture
        .app
        .notices
        .iter()
        .any(|n| n.text.contains("backed up")));
    assert!(fixture.app.config.broken.is_some());
    setup_mut(&mut fixture)
        .editor
        .form
        .set("groq.api_key", Some(Value::from("gsk_k")))
        .unwrap();
    write(&mut fixture);
    let dir = fixture.dir.path();
    assert_eq!(
        std::fs::read_to_string(dir.join("config.toml.bak")).unwrap(),
        broken
    );
    assert!(fixture
        .app
        .notices
        .iter()
        .any(|n| n.text.contains("written from scratch")));
    assert!(fixture.app.config.broken.is_none());
}

/// A config that cannot be read is refused before the TUI opens, with
/// the hand fix (#134).
#[test]
fn an_unreadable_config_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let paths = paths(dir.path());
    std::fs::write(&paths.config, "[general]\n").unwrap();
    std::fs::set_permissions(&paths.config, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Root reads anything.
    if std::fs::read(&paths.config).is_ok() {
        return;
    }
    let refused = App::prepare(
        crate::tui::Entry::Setup,
        paths,
        crate::tui::Env::default(),
        ServiceManager::None,
    )
    .err()
    .expect("refused");
    assert!(refused.contains("Fix it by hand"), "{refused}");
}

/// The review says when the write would be refused, and a refused write
/// keeps every answer.
#[test]
fn a_write_the_config_refuses_keeps_the_answers() {
    let mut fixture = setup_on(None, &[]);
    setup_mut(&mut fixture)
        .editor
        .form
        .set(
            "general.backend",
            Some(Value::from("openai-compatible-realtime")),
        )
        .unwrap();
    press(&mut fixture.app, "\n");
    // The URL setup fills in, cleared: the realtime backend needs one.
    setup_mut(&mut fixture)
        .editor
        .form
        .set("openai-compatible-realtime.url", Some(Value::from("")))
        .unwrap();
    press(&mut fixture.app, "\n\n\n\n\n");
    press(&mut fixture.app, "s");
    assert_eq!(setup(&fixture).step, Step::Review);
    let review = drawn(&fixture, 90, 30);
    assert!(
        review.iter().any(|r| r.contains("not written yet")),
        "the rendered screen differs"
    );
    assert_eq!(write(&mut fixture), None);
    assert!(status(&fixture).starts_with("not written"));
    assert_eq!(setup(&fixture).step, Step::Review);
    assert!(!paths(fixture.dir.path()).config.exists());
}

/// A blank cloud key is no refusal: the review warns where the daemon
/// must find it, and the write goes through (the daemon reads the env var
/// at transcription time, from its own environment).
#[test]
fn a_blank_key_is_written_with_a_warning() {
    // Validation reads the process env for the Groq key.
    if std::env::var_os("WHISRS_GROQ_API_KEY").is_some() {
        return;
    }
    let mut fixture = setup_on(None, &[]);
    press(&mut fixture.app, "\n\x1b\n\n\n\n\n");
    press(&mut fixture.app, "s");
    assert_eq!(setup(&fixture).step, Step::Review);
    let notes = setup(&fixture).review_notes();
    assert!(notes
        .iter()
        .any(|n| n.tone == Tone::Warn && n.text.contains("needs WHISRS_GROQ_API_KEY in its own")));
    assert!(!notes.iter().any(|n| n.text.contains("not written yet")));
    assert_eq!(write(&mut fixture), Some(Effect::Checks));
    assert!(fixture
        .app
        .notices
        .iter()
        .any(|n| n.text.contains("the daemon needs WHISRS_GROQ_API_KEY")));
    assert!(paths(fixture.dir.path()).config.exists());
}

/// The config every fresh start below goes over: sections setup asks
/// nothing about, a key of another backend, keys of asked sections it does
/// not ask, and comments.
const RICH: &str = r#"# my whisrs config
[general]
backend = "deepgram"
language = "de"
# tuned by hand
silence_timeout_ms = 4500
remove_filler_words = false

[deepgram]
api_key = "dg_old_key_1234"
model = "nova-2"

[groq]
api_key = "gsk_other_backend_5678"

[input]
backend = "uinput"
key_delay_ms = 7

[overlay]
theme = "ember"
position = "top-left"

[tts]
enabled = true
backend = "openai"

[llm]
api_key = "sk_llm_key_9999"
model = "gpt-6-luna"

[hotkeys]
toggle = "Alt+D"

[recording]
user_name = "Ada"

[replacements]
"package build" = "PKGBUILD"

[[llm_commands]]
name = "translate"
hotkey = "Super+T"
instruction = "Translate to German."

[[agents]]
name = "jarvis"
dir = "/tmp"
"#;

/// Start fresh changes only the keys setup asked, leaf by leaf, keeps the
/// rest of the file (comments, other sections, other keys of the asked
/// sections), keeps the file as `.bak` first, and the review lists exactly
/// what changes, keys masked.
#[test]
fn start_fresh_writes_only_the_asked_keys() {
    let mut fixture = setup_on(Some(RICH), &[]);
    assert_eq!(setup(&fixture).step, Step::Existing);
    // Start fresh (groq, the default), a new key; the rest as offered;
    // the LLM step skipped.
    press(&mut fixture.app, "\n");
    assert_eq!(setup(&fixture).editor.form.config().general.backend, "groq");
    press(&mut fixture.app, "\n");
    type_text(&mut fixture.app, "gsk_new_key_4321");
    press(&mut fixture.app, "\n\n\n\n\n\n");
    press(&mut fixture.app, "s");
    assert_eq!(setup(&fixture).step, Step::Review);

    let changes: Vec<String> = setup(&fixture)
        .changes()
        .expect("a file to compare with")
        .into_iter()
        .map(|(label, old, new)| format!("{label}: {old} > {new}"))
        .collect();
    assert_eq!(
        changes,
        [
            "[general] backend: deepgram > groq",
            "[groq] api_key: ****5678 > ****4321",
            "[general] language: de > en",
            "[general] remove_filler_words: off > on",
            "[general] audio_feedback: off > on",
            "[overlay] theme: ember > carbon",
            "[input] backend: uinput > auto",
        ],
        "the review lists exactly the changed keys"
    );
    let review = drawn(&fixture, 90, 40).join("\n");
    assert!(review.contains("What changes in config.toml:"));
    assert!(review.contains("****5678 \u{2192} ****4321"));
    assert!(review.contains("config.toml.bak first"));
    assert!(!review.contains("gsk_new_key_4321") && !review.contains("gsk_other_backend_5678"));

    assert_eq!(write(&mut fixture), Some(Effect::Checks));
    let dir = fixture.dir.path();
    assert_eq!(
        std::fs::read(dir.join("config.toml.bak")).unwrap(),
        RICH.as_bytes(),
        "the .bak is the old file byte for byte"
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(dir.join("config.toml.bak"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let text = std::fs::read_to_string(dir.join("config.toml")).unwrap();
    assert!(text.starts_with("# my whisrs config\n"));
    // No default was written, and no template appended.
    for default in [
        "history =",
        "llm_commands = []",
        "modifier_wait_ms",
        "flash_attn",
        "llm_instruction",
        "# [hooks]",
    ] {
        assert!(!text.contains(default), "{default} was written");
    }
    assert!(text.contains("# tuned by hand\nsilence_timeout_ms = 4500"));
    let written: whisrs::Config = toml::from_str(&text).unwrap();
    // The asked keys changed.
    assert_eq!(written.general.backend, "groq");
    assert_eq!(written.general.language, "en");
    assert!(written.general.remove_filler_words);
    assert!(
        written
            .groq
            .as_ref()
            .is_some_and(|g| g.api_key == "gsk_new_key_4321"),
        "the new key was written"
    );
    // Everything else stayed.
    assert_eq!(written.general.silence_timeout_ms, 4500);
    let deepgram = written.deepgram.expect("[deepgram] stays");
    assert!(
        deepgram.api_key == "dg_old_key_1234",
        "the other backend's key stays"
    );
    assert_eq!(deepgram.model, "nova-2");
    assert_eq!(written.input.key_delay_ms, 7);
    let overlay = written.overlay.expect("[overlay] stays");
    assert_eq!(overlay.position, "top-left");
    assert_eq!(overlay.theme, "carbon");
    assert!(written
        .tts
        .is_some_and(|t| t.enabled && t.backend == "openai"));
    let llm = written.llm.expect("[llm] stays: its step was skipped");
    assert!(llm.api_key == "sk_llm_key_9999", "the LLM key stays");
    assert_eq!(llm.model, "gpt-6-luna");
    assert_eq!(
        written.hotkeys.and_then(|h| h.toggle).as_deref(),
        Some("Alt+D")
    );
    assert_eq!(
        written.recording.and_then(|r| r.user_name).as_deref(),
        Some("Ada")
    );
    assert_eq!(
        written
            .replacements
            .get("package build")
            .map(String::as_str),
        Some("PKGBUILD")
    );
    assert_eq!(written.llm_commands.len(), 1);
    assert_eq!(written.agents.len(), 1);
    assert!(status(&fixture).contains("config.toml.bak"));
}

/// Written answers changed since, and answers changed on the way back to
/// the first step, are asked about before leaving.
#[test]
fn answers_changed_after_a_write_or_reached_back_are_asked_about() {
    let mut fixture = on_the_key_step();
    press(&mut fixture.app, "\n\n\n\n\n");
    press(&mut fixture.app, "s");
    write(&mut fixture);
    assert_eq!(setup(&fixture).step, Step::Checks);
    assert!(!setup(&fixture).unwritten());
    // Back to the typing step, and a change there.
    key(&mut fixture.app, KeyCode::Backspace);
    key(&mut fixture.app, KeyCode::Backspace);
    key(&mut fixture.app, KeyCode::Backspace);
    assert_eq!(setup(&fixture).step, Step::Typing);
    press(&mut fixture.app, "l");
    assert!(setup(&fixture).unwritten());
    press(&mut fixture.app, "q");
    assert_eq!(fixture.app.dialog, Some(Dialog::Unwritten { quit: true }));

    // Start fresh, a change, then Backspace to the first step.
    let mut fixture = setup_on(Some("[general]\n"), &[]);
    press(&mut fixture.app, "\nl");
    key(&mut fixture.app, KeyCode::Backspace);
    assert_eq!(setup(&fixture).step, Step::Existing);
    assert!(setup(&fixture).unwritten());
    press(&mut fixture.app, "\x1b");
    assert_eq!(fixture.app.dialog, Some(Dialog::Unwritten { quit: false }));
}

/// Unknown keys are named as setup opens, before any question (#116).
#[test]
fn unknown_keys_are_named_before_the_first_question() {
    let fixture = setup_on(
        Some("[general]\nsilense_timeout_ms = 1\nbackend = \"groq\"\n"),
        &[],
    );
    assert_eq!(setup(&fixture).step, Step::Existing);
    assert!(fixture
        .app
        .notices
        .iter()
        .any(|n| n.text == "unknown key [general] silense_timeout_ms is ignored"));
    assert!(drawn(&fixture, 90, 30)
        .iter()
        .any(|r| r.contains("unknown key [general] silense_timeout_ms is ignored")));
}

/// The key step as the mockup has it, at every size, and the other steps
/// at 90x30.
#[test]
fn setup_snapshots() {
    let fixture = on_the_key_step();
    for (w, h) in [(59, 17), (60, 18), (89, 30), (90, 30), (200, 60)] {
        assert_snapshot(&format!("setup-key-{w}x{h}"), &drawn(&fixture, w, h));
    }
    let mut fixture = setup_on(
        Some("[general]\nbackend = \"groq\"\n[groq]\napi_key = \"k\"\n"),
        &[],
    );
    assert_snapshot("setup-existing-90x30", &drawn(&fixture, 90, 30));
    press(&mut fixture.app, "j\n");
    fixture.app.update(Msg::Checked(Box::new(checks())));
    assert_snapshot("setup-checks-90x30", &drawn(&fixture, 90, 30));
    assert_snapshot("setup-checks-60x18", &drawn(&fixture, 60, 18));

    let mut fixture = on_the_key_step();
    press(&mut fixture.app, "\n\n\n\n\n");
    press(&mut fixture.app, "s");
    assert_snapshot("setup-review-90x30", &drawn(&fixture, 90, 30));

    // Over a config there: what changes, keys masked, and the .bak.
    let mut fixture = setup_on(Some(RICH), &[]);
    press(&mut fixture.app, "\n\n");
    type_text(&mut fixture.app, "gsk_new_key_4321");
    press(&mut fixture.app, "\n\n\n\n\n\n");
    press(&mut fixture.app, "s");
    assert_snapshot("setup-review-changes-90x40", &drawn(&fixture, 90, 40));
}

/// No size panics on any step.
#[test]
fn no_setup_size_panics() {
    let mut states = vec![on_the_key_step(), setup_on(Some("[general]\n"), &[])];
    let mut mic = on_the_key_step();
    press(&mut mic.app, "\n\n");
    states.push(mic);
    let mut picker = setup_on(None, &[]);
    press(&mut picker.app, " ");
    states.push(picker);
    let mut checks_step = setup_on(Some("[general]\n"), &[]);
    press(&mut checks_step.app, "j\n");
    checks_step.app.update(Msg::Checked(Box::new(checks())));
    states.push(checks_step);
    for fixture in &states {
        for w in [1, 10, 59, 60, 61, 89, 90, 120, 200] {
            for h in [1, 5, 17, 18, 19, 30, 60] {
                draw(&fixture.app, w, h);
            }
        }
    }
}
