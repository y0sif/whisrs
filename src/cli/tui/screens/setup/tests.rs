//! Setup, driven by keys through the pure update: the steps in order, back
//! and skip, the optional modes, the questions before leaving, the model
//! download, the read-aloud sample, the checks, and the #134 refusals.

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

/// A setup snapshot. The header counts the steps, one fewer without
/// recording: the snapshots are of the default build.
fn snapshot(name: &str, rows: &[String]) {
    if Gate::Recording.built() {
        assert_snapshot(name, rows);
    }
}

/// Skip every optional step from the one setup is on, to the review.
fn skip_modes(fixture: &mut Fixture) {
    while setup(fixture).step.optional() {
        press(&mut fixture.app, "s");
    }
    assert_eq!(setup(fixture).step, Step::Review);
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
    let first = format!("setup 1/{}", Step::count());
    assert!(drawn(&fixture, 90, 30).iter().any(|r| r.contains(&first)));

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

    // Typing, then the optional modes skipped.
    press(&mut fixture.app, "\n\n");
    assert!(setup(&fixture).step.optional());
    skip_modes(&mut fixture);
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
    snapshot("setup-model-90x30", &drawn(&fixture, 90, 30));
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

    // A failed download stays the choice: Enter tries it again.
    press(&mut fixture.app, " ");
    type_text(&mut fixture.app, "small.en");
    let failing = started(press(&mut fixture.app, "\n"), "small.en");
    assert_eq!(set(&fixture), Some(path("small.en").display().to_string()));
    fixture.app.update(Msg::Download(Download::Done {
        id: failing,
        result: Err(DownloadError::Failed("HTTP 404".into())),
    }));
    assert!(status(&fixture).contains("the download failed"));
    assert_eq!(set(&fixture), Some(path("small.en").display().to_string()));
    let again = started(press(&mut fixture.app, "\n"), "small.en");
    press(&mut fixture.app, "\x1b");
    fixture.app.update(Msg::Download(Download::Done {
        id: again,
        result: Err(DownloadError::Cancelled),
    }));

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
    skip_modes(&mut fixture);
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
    skip_modes(&mut fixture);
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
    // Start fresh (groq, the default), a new key over the file's; the
    // rest as offered; the optional modes skipped.
    press(&mut fixture.app, "\n");
    assert_eq!(setup(&fixture).editor.form.config().general.backend, "groq");
    press(&mut fixture.app, "\n ");
    type_text(&mut fixture.app, "gsk_new_key_4321");
    press(&mut fixture.app, "\n\n\n\n\n\n");
    skip_modes(&mut fixture);

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
    skip_modes(&mut fixture);
    write(&mut fixture);
    assert_eq!(setup(&fixture).step, Step::Checks);
    assert!(!setup(&fixture).unwritten());
    // Back to the typing step, past the skipped modes' questions, and a
    // change there.
    for _ in 0..Step::all().iter().filter(|s| s.optional()).count() + 2 {
        key(&mut fixture.app, KeyCode::Backspace);
    }
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
        snapshot(&format!("setup-key-{w}x{h}"), &drawn(&fixture, w, h));
    }
    let mut fixture = setup_on(
        Some("[general]\nbackend = \"groq\"\n[groq]\napi_key = \"k\"\n"),
        &[],
    );
    snapshot("setup-existing-90x30", &drawn(&fixture, 90, 30));
    press(&mut fixture.app, "j\n");
    fixture.app.update(Msg::Checked(Box::new(checks())));
    snapshot("setup-checks-90x30", &drawn(&fixture, 90, 30));
    snapshot("setup-checks-60x18", &drawn(&fixture, 60, 18));

    let mut fixture = on_the_key_step();
    press(&mut fixture.app, "\n\n\n\n\n");
    skip_modes(&mut fixture);
    snapshot("setup-review-90x30", &drawn(&fixture, 90, 30));

    // Over a config there: what changes, keys masked, and the .bak.
    let mut fixture = setup_on(Some(RICH), &[]);
    press(&mut fixture.app, "\n\n ");
    type_text(&mut fixture.app, "gsk_new_key_4321");
    press(&mut fixture.app, "\n\n\n\n\n\n");
    skip_modes(&mut fixture);
    snapshot("setup-review-changes-90x40", &drawn(&fixture, 90, 40));
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

/// A fresh setup on groq with a key, on its first optional step.
fn on_the_modes() -> Fixture {
    let mut fixture = on_the_key_step();
    press(&mut fixture.app, "\n\n\n\n\n");
    assert!(setup(&fixture).step.optional());
    fixture
}

/// Skip on to optional step `step`.
fn skip_to(fixture: &mut Fixture, step: Step) {
    while setup(fixture).step != step {
        assert!(setup(fixture).step.optional(), "{step:?} is not ahead");
        press(&mut fixture.app, "s");
    }
}

/// The paths of the rows setup shows.
fn shown_rows(fixture: &Fixture) -> Vec<String> {
    setup(fixture)
        .rows()
        .into_iter()
        .map(|(_, path)| path)
        .collect()
}

fn focused(fixture: &Fixture) -> Option<String> {
    setup(fixture).editor.focused().map(|(_, path)| path)
}

/// Set `path` in setup's answers, as a row edit would.
fn answer(fixture: &mut Fixture, path: &str, value: &str) {
    setup_mut(fixture)
        .editor
        .form
        .set(path, Some(Value::from(value)))
        .unwrap();
}

/// A fresh setup on local whisper's model step, the models folder not
/// there yet (his test).
fn on_the_model_step() -> Fixture {
    let mut fixture = setup_on(None, &[]);
    answer(&mut fixture, "general.backend", "local-whisper");
    press(&mut fixture.app, "\n");
    fixture.app.update(Msg::Listed(Listing::Models(Err(
        "no models in ~/models yet: whisrs setup downloads one".into(),
    ))));
    fixture
}

/// Enter on the model step downloads a model that is not on disk and goes
/// on by itself once it is done; one on disk is used as it is. The footer
/// says which.
#[test]
fn enter_downloads_the_model_and_goes_on_when_it_is_done() {
    let mut fixture = setup_on(None, &[]);
    answer(&mut fixture, "general.backend", "local-whisper");
    press(&mut fixture.app, "\n");
    // Before the folder is looked at, nothing downloads.
    assert!(press(&mut fixture.app, "\n").is_empty());
    assert!(status(&fixture).starts_with("looking for the models"));
    fixture.app.update(Msg::Listed(Listing::Models(Err(
        "no models in ~/models yet: whisrs setup downloads one".into(),
    ))));
    assert!(drawn(&fixture, 90, 30)
        .iter()
        .any(|r| r.contains("enter download and go on")));

    let id = started(press(&mut fixture.app, "\n"), "base.en");
    assert_eq!(setup(&fixture).step, Step::Key);
    // Busy: Enter waits for it.
    press(&mut fixture.app, "\n");
    assert_eq!(setup(&fixture).step, Step::Key);
    let models = paths(fixture.dir.path()).models;
    let effect = fixture.app.update(Msg::Download(Download::Done {
        id,
        result: Ok(models.join("ggml-base.en.bin")),
    }));
    assert_eq!(effect, Some(Effect::List(ListKind::Models)));
    assert_eq!(setup(&fixture).step, Step::Language);
    assert_eq!(status(&fixture), "downloaded ~/models/ggml-base.en.bin");

    // Back at the step, the model is there: Enter goes on.
    key(&mut fixture.app, KeyCode::Backspace);
    assert_eq!(setup(&fixture).step, Step::Key);
    let footer = drawn(&fixture, 90, 30);
    assert!(footer.iter().any(|r| r.contains("enter next")));
    assert!(!footer.iter().any(|r| r.contains("download and go on")));
    assert!(press(&mut fixture.app, "\n").is_empty());
    assert_eq!(setup(&fixture).step, Step::Language);
}

/// A failed download stays on the step, says why, and Enter tries again.
#[test]
fn a_failed_download_stays_and_enter_tries_again() {
    let mut fixture = on_the_model_step();
    let id = started(press(&mut fixture.app, "\n"), "base.en");
    fixture.app.update(Msg::Download(Download::Done {
        id,
        result: Err(DownloadError::Failed("HTTP 404".into())),
    }));
    assert_eq!(setup(&fixture).step, Step::Key);
    assert_eq!(
        status(&fixture),
        "the download failed: HTTP 404; enter tries again"
    );
    let again = started(press(&mut fixture.app, "\n"), "base.en");
    assert_ne!(again, id);
    // Picked in the picker, a model downloads at once and the step stays.
    press(&mut fixture.app, "\x1b");
    press(&mut fixture.app, " ");
    type_text(&mut fixture.app, "tiny.en");
    let picked = started(press(&mut fixture.app, "\n"), "tiny.en");
    fixture.app.update(Msg::Download(Download::Done {
        id: picked,
        result: Ok(paths(fixture.dir.path()).models.join("ggml-tiny.en.bin")),
    }));
    assert_eq!(setup(&fixture).step, Step::Key);

    // One picked that fails stays the choice too: Enter tries that one
    // again, never the model before it.
    press(&mut fixture.app, " ");
    type_text(&mut fixture.app, "small.en");
    let failing = started(press(&mut fixture.app, "\n"), "small.en");
    fixture.app.update(Msg::Download(Download::Done {
        id: failing,
        result: Err(DownloadError::Failed("HTTP 503".into())),
    }));
    assert_eq!(setup(&fixture).whisper_model().name, "small.en");
    assert!(drawn(&fixture, 90, 30)
        .iter()
        .any(|r| r.contains("enter download and go on")));
    started(press(&mut fixture.app, "\n"), "small.en");
}

/// The review warns of a local whisper model that is not on disk: the
/// daemon cannot start without it.
#[test]
fn the_review_warns_of_a_model_not_downloaded() {
    let mut fixture = on_the_modes();
    skip_modes(&mut fixture);
    assert!(!setup(&fixture)
        .review_notes()
        .iter()
        .any(|n| n.text.contains("is not downloaded")));
    answer(&mut fixture, "general.backend", "local-whisper");
    let notes = setup(&fixture).review_notes();
    assert!(
        notes.iter().any(|n| n.tone == Tone::Warn
            && n.text
                .starts_with("ggml-base.en.bin is not downloaded, and the daemon")),
        "{notes:?}"
    );
}

/// The rows each optional step shows once it is set up, with groq for
/// dictation and recordings.
fn mode_rows(step: Step) -> Vec<&'static str> {
    match step {
        Step::Recording => vec![
            "recording.user_name",
            "recording.backend",
            "hotkeys.recording",
        ],
        Step::Command => vec!["llm.api_url", "llm.model", "llm.api_key", "hotkeys.command"],
        Step::ReadAloud => vec![
            "tts.backend",
            "tts.model",
            "tts.voice",
            "tts.api_key",
            "hotkeys.speak",
        ],
        Step::Jarvis => vec![
            "agents.0.name",
            "agents.0.harness",
            "agents.0.dir",
            "agents.0.permissions",
            "agents.0.feedback",
            "agents.0.hotkey",
            "agents.0.new_session_hotkey",
        ],
        _ => Vec::new(),
    }
}

/// Each optional step asks first, the cursor on Set it up; Set it up shows
/// its rows, the cursor on the first.
#[test]
fn each_optional_step_asks_then_shows_its_rows() {
    let mut fixture = on_the_modes();
    for step in Step::all().into_iter().filter(|s| s.optional()) {
        assert_eq!(setup(&fixture).step, step);
        assert!(shown_rows(&fixture).is_empty());
        let screen = drawn(&fixture, 90, 30);
        assert!(
            screen.iter().any(|r| r.contains("\u{258c} Set it up")),
            "{step:?}: {screen:#?}"
        );
        assert!(screen.iter().any(|r| r.contains("  Skip ")));
        assert!(press(&mut fixture.app, "\n").is_empty());
        assert_eq!(shown_rows(&fixture), mode_rows(step), "{step:?}");
        assert_eq!(focused(&fixture).as_deref(), Some(mode_rows(step)[0]));
        press(&mut fixture.app, "s");
    }
    assert_eq!(setup(&fixture).step, Step::Review);
}

/// Skip at the question, by s or by its row, goes on and writes none of
/// the mode's keys.
#[test]
fn a_skipped_question_writes_none_of_the_mode() {
    let mut fixture = on_the_modes();
    skip_to(&mut fixture, Step::Command);
    press(&mut fixture.app, "j\n");
    assert_eq!(setup(&fixture).step, Step::ReadAloud);
    assert_eq!(
        status(&fixture),
        "skipped command mode: whisrs config sets it later"
    );
    press(&mut fixture.app, "s");
    assert_eq!(setup(&fixture).step, Step::Jarvis);
    let asked = setup(&fixture).asked();
    assert!(
        asked.iter().all(|k| !k.starts_with("llm.")
            && !k.starts_with("tts.")
            && !k.starts_with("recording.")
            && !k.starts_with("hotkeys.")
            && k != "agents"),
        "{asked:?}"
    );
    // With a file, it says the file keeps them.
    let mut fixture = setup_on(Some("[groq]\napi_key = \"gsk_file_1234\"\n"), &[]);
    press(&mut fixture.app, "\n\n\n\n\n\n\n");
    skip_to(&mut fixture, Step::ReadAloud);
    press(&mut fixture.app, "s");
    assert_eq!(
        status(&fixture),
        "skipped read aloud: [tts] stays as config.toml has it; whisrs config sets it later"
    );
}

/// s midway puts the mode's answers back as they were before Set it up,
/// and none of its keys are asked.
#[test]
fn skip_midway_puts_the_answers_back() {
    let mut fixture = on_the_modes();
    skip_to(&mut fixture, Step::Command);
    press(&mut fixture.app, "\nl");
    assert!(setup(&fixture).editor.form.config().llm.is_some());
    assert!(setup(&fixture).asked().contains(&"llm.api_url".to_string()));
    press(&mut fixture.app, "s");
    assert_eq!(setup(&fixture).step, Step::ReadAloud);
    assert!(setup(&fixture).editor.form.config().llm.is_none());

    // Read aloud: Set it up turns it on, on the dictation provider.
    press(&mut fixture.app, "\n");
    let tts = setup(&fixture).editor.form.config().tts.clone().unwrap();
    assert!(tts.enabled);
    assert_eq!(tts.backend, "groq");
    assert!(setup(&fixture).asked().contains(&"tts.enabled".to_string()));
    // A voice picked, then skipped midway.
    press(&mut fixture.app, "jjl");
    assert_eq!(focused(&fixture).as_deref(), Some("tts.voice"));
    assert!(setup(&fixture)
        .editor
        .form
        .config()
        .tts
        .as_ref()
        .is_some_and(|t| t.voice.is_some()));
    press(&mut fixture.app, "s");
    assert_eq!(setup(&fixture).step, Step::Jarvis);
    assert!(setup(&fixture).editor.form.config().tts.is_none());
    assert!(status(&fixture).starts_with("skipped read aloud"));

    // Jarvis: its entry goes again.
    press(&mut fixture.app, "\n");
    assert_eq!(setup(&fixture).editor.form.entries("agents").len(), 1);
    press(&mut fixture.app, "s");
    assert!(setup(&fixture).editor.form.entries("agents").is_empty());
    let asked = setup(&fixture).asked();
    assert!(
        asked.iter().all(|k| !k.starts_with("llm.")
            && !k.starts_with("tts.")
            && !k.starts_with("hotkeys.")
            && k != "agents"),
        "{asked:?}"
    );
    // Nothing of them is written.
    assert_eq!(write(&mut fixture), Some(Effect::Checks));
    let text = std::fs::read_to_string(paths(fixture.dir.path()).config).unwrap();
    assert!(!text.contains("[llm]") && !text.contains("[tts]") && !text.contains("[[agents]]"));
}

/// Backspace from a mode's rows is its question, the answers kept; back at
/// a step later, a mode set up shows its rows, one skipped its question.
#[test]
fn backspace_from_the_rows_is_the_question() {
    let mut fixture = on_the_modes();
    skip_to(&mut fixture, Step::Command);
    press(&mut fixture.app, "\nl");
    let url = setup(&fixture)
        .editor
        .form
        .config()
        .llm
        .clone()
        .unwrap()
        .api_url;
    key(&mut fixture.app, KeyCode::Backspace);
    assert_eq!(setup(&fixture).step, Step::Command);
    assert!(shown_rows(&fixture).is_empty());
    press(&mut fixture.app, "\n");
    assert_eq!(
        setup(&fixture)
            .editor
            .form
            .config()
            .llm
            .clone()
            .unwrap()
            .api_url,
        url
    );
    assert_eq!(shown_rows(&fixture), mode_rows(Step::Command));

    answer(&mut fixture, "llm.api_key", "sk_test_0001");
    press(&mut fixture.app, "\n");
    assert_eq!(setup(&fixture).step, Step::ReadAloud);
    press(&mut fixture.app, "s");
    assert_eq!(setup(&fixture).step, Step::Jarvis);
    key(&mut fixture.app, KeyCode::Backspace);
    assert_eq!(setup(&fixture).step, Step::ReadAloud);
    assert!(shown_rows(&fixture).is_empty());
    key(&mut fixture.app, KeyCode::Backspace);
    assert_eq!(setup(&fixture).step, Step::Command);
    assert_eq!(shown_rows(&fixture), mode_rows(Step::Command));
}

/// Enter goes on only from a complete mode; otherwise it says what is
/// missing, and that s skips it.
#[test]
fn enter_waits_for_a_complete_mode() {
    let mut fixture = on_the_modes();
    let refused = |fixture: &mut Fixture, step: Step, said: &str| {
        press(&mut fixture.app, "\n");
        assert_eq!(setup(fixture).step, step);
        assert!(status(fixture).starts_with(said), "{}", status(fixture));
        assert!(status(fixture).contains(", or s skips"));
    };

    if Gate::Recording.built() {
        assert_eq!(setup(&fixture).step, Step::Recording);
        press(&mut fixture.app, "\n");
        answer(&mut fixture, "recording.backend", "openai-realtime");
        refused(
            &mut fixture,
            Step::Recording,
            "recording needs a backend that takes a file, not openai-realtime",
        );
        answer(&mut fixture, "recording.backend", "groq");
        // Another provider's backend brings its key row, and needs it.
        press(&mut fixture.app, "jl");
        assert_eq!(
            setup(&fixture)
                .editor
                .form
                .config()
                .recording
                .as_ref()
                .and_then(|r| r.backend.as_deref()),
            Some("openai")
        );
        assert_eq!(shown_rows(&fixture).last().unwrap(), "openai.api_key");
        assert_eq!(focused(&fixture).as_deref(), Some("recording.backend"));
        refused(
            &mut fixture,
            Step::Recording,
            "recording needs a key for openai",
        );
        answer(&mut fixture, "openai.api_key", "sk_test_0002");
        press(&mut fixture.app, "\n");
    }

    assert_eq!(setup(&fixture).step, Step::Command);
    press(&mut fixture.app, "\n");
    refused(&mut fixture, Step::Command, "command mode needs an LLM key");
    // A server on this machine needs none.
    answer(
        &mut fixture,
        "llm.api_url",
        "http://localhost:11434/v1/chat/completions",
    );
    press(&mut fixture.app, "\n");

    // Read aloud: groq's key is there from the key step; openai's is not.
    assert_eq!(setup(&fixture).step, Step::ReadAloud);
    press(&mut fixture.app, "\nl");
    assert_eq!(
        setup(&fixture)
            .editor
            .form
            .config()
            .tts
            .as_ref()
            .unwrap()
            .backend,
        "openai"
    );
    if !Gate::Recording.built() {
        refused(
            &mut fixture,
            Step::ReadAloud,
            "read aloud needs a key for openai",
        );
    }
    press(&mut fixture.app, "l");
    refused(
        &mut fixture,
        Step::ReadAloud,
        "read aloud needs a key for deepgram",
    );
    // A local server needs none, and has a URL row.
    press(&mut fixture.app, "l");
    assert!(shown_rows(&fixture).contains(&"tts.url".to_string()));
    press(&mut fixture.app, "\n");

    // Jarvis: a valid name no other agent has, and a dir that exists.
    assert_eq!(setup(&fixture).step, Step::Jarvis);
    press(&mut fixture.app, "\n");
    refused(&mut fixture, Step::Jarvis, "Jarvis needs a dir");
    answer(&mut fixture, "agents.0.dir", "/nowhere/at/all");
    refused(
        &mut fixture,
        Step::Jarvis,
        "/nowhere/at/all is not a folder",
    );
    let dir = fixture.dir.path().to_str().unwrap().to_string();
    answer(&mut fixture, "agents.0.dir", &dir);
    answer(&mut fixture, "agents.0.name", "Jarvis!");
    refused(&mut fixture, Step::Jarvis, "Jarvis! cannot name an agent");
    answer(&mut fixture, "agents.0.name", "");
    refused(&mut fixture, Step::Jarvis, "Jarvis needs a name");
    answer(&mut fixture, "agents.0.name", "jarvis");
    // A harness not on PATH is a warning only.
    press(&mut fixture.app, "\n");
    assert_eq!(setup(&fixture).step, Step::Review);
    assert_eq!(
        status(&fixture),
        "claude is not on PATH: Jarvis cannot start it until it is"
    );
}

/// The file every Jarvis test below adds to: two agents of its own, and
/// the answers Start fresh gives, so the write changes nothing else.
const WITH_AGENTS: &str = r#"[general]
remove_filler_words = true
audio_feedback = true

[groq]
api_key = "gsk_file_key_1234"

# the agents I talk to
[[agents]]
name = "jarvis" # the first
dir = "/tmp"

[[agents]]
name = "friday"
harness = "codex"
dir = "/tmp"
permissions = "ask"
"#;

/// With agents in the file, Jarvis asks Add one or Keep as it is, naming
/// them; the new one starts unnamed (jarvis is taken), on the first
/// harness PATH has. The write appends it and changes none of the file's.
#[test]
fn jarvis_adds_one_beside_the_files_agents_and_changes_none() {
    let bin = tempfile::tempdir().unwrap();
    let codex = bin.path().join("codex");
    std::fs::write(&codex, "#!/bin/sh\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut fixture = setup_on(Some(WITH_AGENTS), &[("PATH", bin.path().to_str().unwrap())]);
    // Start fresh: the file's key is kept, so the key step opens nothing.
    press(&mut fixture.app, "\n\n");
    assert_eq!(setup(&fixture).mode(), Mode::Browse);
    press(&mut fixture.app, "\n\n\n\n\n");
    skip_to(&mut fixture, Step::Jarvis);
    let question = drawn(&fixture, 90, 30).join("\n");
    assert!(
        question.contains("Add one        beside jarvis (claude), friday (codex)"),
        "{question}"
    );
    assert!(question.contains("Keep as it is  jarvis (claude), friday (codex)"));

    press(&mut fixture.app, "\n");
    assert_eq!(shown_rows(&fixture)[0], "agents.2.name");
    let form = &setup(&fixture).editor.form;
    assert_eq!(form.value("agents.2.name"), Some(&Value::from("")));
    assert_eq!(form.value("agents.2.harness"), Some(&Value::from("codex")));
    // The name and the dir, typed.
    press(&mut fixture.app, " ");
    type_text(&mut fixture.app, "kitt");
    press(&mut fixture.app, "\njj ");
    let dir = fixture.dir.path().to_str().unwrap().to_string();
    type_text(&mut fixture.app, &dir);
    press(&mut fixture.app, "\n\n");
    assert_eq!(setup(&fixture).step, Step::Review);
    assert_eq!(
        setup(&fixture).changes().unwrap(),
        [(
            "[[agents]]".to_string(),
            "jarvis, friday".to_string(),
            "jarvis, friday, kitt".to_string()
        )]
    );

    assert_eq!(write(&mut fixture), Some(Effect::Checks));
    let text = std::fs::read_to_string(paths(fixture.dir.path()).config).unwrap();
    let added = text
        .strip_prefix(WITH_AGENTS)
        .unwrap_or_else(|| panic!("the file's own lines changed:\n{text}"));
    assert_eq!(
        added,
        format!("\n[[agents]]\nname = \"kitt\"\nharness = \"codex\"\ndir = \"{dir}\"\n")
    );
}

/// Keep as it is changes no agent.
#[test]
fn jarvis_kept_as_it_is_writes_no_agent() {
    let mut fixture = setup_on(Some(WITH_AGENTS), &[]);
    press(&mut fixture.app, "\n\n\n\n\n\n\n");
    skip_to(&mut fixture, Step::Jarvis);
    press(&mut fixture.app, "j\n");
    assert_eq!(setup(&fixture).step, Step::Review);
    assert!(!setup(&fixture).asked().contains(&"agents".to_string()));
    assert_eq!(setup(&fixture).changes().unwrap(), []);
}

/// Start fresh keeps every API key the file has: the key step shows it set
/// and opens no input, and command mode set up has its key. Nothing else
/// of the file comes with it, and nothing is unwritten yet.
#[test]
fn start_fresh_keeps_the_files_keys() {
    let mut fixture = setup_on(Some(RICH), &[]);
    press(&mut fixture.app, "\n");
    assert!(!setup(&fixture).unwritten());
    let form = &setup(&fixture).editor.form;
    for (path, key) in [
        ("groq.api_key", "gsk_other_backend_5678"),
        ("deepgram.api_key", "dg_old_key_1234"),
        ("llm.api_key", "sk_llm_key_9999"),
    ] {
        assert_eq!(form.value(path), Some(&Value::from(key)), "{path}");
    }
    assert_eq!(form.config().general.language, "en");
    // The LLM key keeps the model it is for.
    assert_eq!(form.config().llm.as_ref().unwrap().model, "gpt-6-luna");
    press(&mut fixture.app, "\n");
    assert_eq!(setup(&fixture).mode(), Mode::Browse);
    assert!(drawn(&fixture, 90, 30)
        .iter()
        .any(|r| r.contains("set ****5678")));
    press(&mut fixture.app, "\n\n\n\n\n");
    skip_to(&mut fixture, Step::Command);
    press(&mut fixture.app, "\n");
    assert!(drawn(&fixture, 90, 30)
        .iter()
        .any(|r| r.contains("set ****9999")));
    press(&mut fixture.app, "\n");
    assert_eq!(
        setup(&fixture).step,
        Step::ReadAloud,
        "the LLM key resolves"
    );
}

/// The step numbers come from the list of steps: recording is step 7 in a
/// build with it, and not there without.
#[test]
fn the_steps_are_numbered_from_their_list() {
    #[cfg(feature = "recording")]
    {
        assert_eq!(Step::count(), 12);
        assert_eq!(Step::Recording.number(), Some(7));
        assert_eq!(Step::Command.number(), Some(8));
        assert_eq!(Step::Typing.after(), Some(Step::Recording));
    }
    #[cfg(not(feature = "recording"))]
    {
        assert_eq!(Step::count(), 11);
        assert_eq!(Step::Recording.number(), None);
        assert_eq!(Step::Command.number(), Some(7));
        assert_eq!(Step::Typing.after(), Some(Step::Command));
    }
    assert_eq!(Step::ReadAloud.after(), Some(Step::Jarvis));
    assert_eq!(Step::Jarvis.after(), Some(Step::Review));
    assert_eq!(Step::Checks.number(), Some(Step::count()));
    assert_eq!(Step::Existing.number(), None);
    let fixture = on_the_modes();
    let header = format!(
        "setup {}/{}",
        setup(&fixture).step.number().unwrap(),
        Step::count()
    );
    assert!(drawn(&fixture, 90, 30).iter().any(|r| r.contains(&header)));
}

/// t on read aloud says a sample with the voice picked; any key stops it,
/// even one with no meaning here, and its late end says nothing. A failure
/// says why, and with no key for the voice t says that first.
#[test]
fn t_says_a_sample_and_any_key_stops_it() {
    let mut fixture = on_the_modes();
    skip_to(&mut fixture, Step::ReadAloud);
    press(&mut fixture.app, "\n");
    assert_eq!(press(&mut fixture.app, "t"), [Effect::Speak { id: 1 }]);
    assert!(fixture.app.spinning());
    let playing = drawn(&fixture, 90, 30);
    assert!(playing
        .iter()
        .any(|r| r.contains("speaking: any key stops it")));
    assert!(playing.iter().any(|r| r.contains("any key stop")));
    assert_eq!(press(&mut fixture.app, "z"), [Effect::StopSpeaking]);
    assert_eq!(setup(&fixture).sample, Some(Sample::Stopped));
    assert_eq!(setup(&fixture).step, Step::ReadAloud);
    fixture.app.update(Msg::Spoke {
        id: 1,
        said: Ok(()),
    });
    assert_eq!(setup(&fixture).sample, Some(Sample::Stopped));

    assert_eq!(press(&mut fixture.app, "t"), [Effect::Speak { id: 2 }]);
    fixture.app.update(Msg::Spoke {
        id: 2,
        said: Err("TTS request failed: HTTP 401".into()),
    });
    assert_eq!(
        status(&fixture),
        "the sample failed: TTS request failed: HTTP 401"
    );
    assert!(!fixture.app.spinning());

    // Another backend, with no key: t says what is missing.
    press(&mut fixture.app, "l");
    assert_eq!(setup(&fixture).sample, None);
    assert!(press(&mut fixture.app, "t").is_empty());
    assert!(status(&fixture).starts_with("read aloud needs a key for openai"));
}

/// Leaving setup while the sample plays stops it.
#[test]
fn leaving_setup_stops_the_sample() {
    let mut fixture = on_the_modes();
    skip_to(&mut fixture, Step::ReadAloud);
    press(&mut fixture.app, "\nt");
    fixture.app.dialog = Some(Dialog::Unwritten { quit: false });
    assert_eq!(press(&mut fixture.app, "d"), [Effect::StopSpeaking]);
    assert!(fixture.app.setup.is_none());
}

/// A mode's hotkey can stay unset: its help says the compositor can bind
/// the command instead.
#[test]
fn a_modes_hotkey_can_be_bound_in_the_compositor_instead() {
    let mut fixture = on_the_modes();
    skip_to(&mut fixture, Step::Command);
    press(&mut fixture.app, "\njjj");
    assert_eq!(focused(&fixture).as_deref(), Some("hotkeys.command"));
    assert!(setup(&fixture)
        .help()
        .ends_with("Set one here, or leave it unset and bind whisrs command in your compositor."));
    // Labelled hotkey on its mode's step; its dialog names it as
    // [hotkeys] does.
    assert!(drawn(&fixture, 90, 30)
        .iter()
        .any(|r| r.contains("\u{258c} hotkey   not set")));
    press(&mut fixture.app, " ");
    assert!(drawn(&fixture, 90, 30)
        .iter()
        .any(|r| r.contains("command hotkey")));
    press(&mut fixture.app, "\x1b");
    press(&mut fixture.app, "ss\njjjjj");
    assert_eq!(focused(&fixture).as_deref(), Some("agents.0.hotkey"));
    assert!(setup(&fixture)
        .help()
        .contains("bind whisrs agent jarvis in your compositor"));
    // Its dialog, as any hotkey row's.
    press(&mut fixture.app, " ");
    assert_eq!(setup(&fixture).mode(), Mode::Hotkey);
    press(&mut fixture.app, "\x1bj");
    assert!(setup(&fixture)
        .help()
        .contains("bind whisrs agent jarvis --new in your compositor"));
}

/// The review has a row per mode: set up with its main values, or
/// skipped.
#[test]
fn the_review_sums_up_each_mode() {
    let mut fixture = on_the_modes();
    skip_to(&mut fixture, Step::ReadAloud);
    press(&mut fixture.app, "\n\n");
    skip_modes(&mut fixture);
    let review = drawn(&fixture, 90, 30).join("\n");
    assert!(review.contains("command mode  skipped"), "{review}");
    assert!(review.contains("read aloud    set up \u{b7} groq \u{b7} autumn \u{b7} no hotkey"));
    assert!(review.contains("Jarvis        skipped"));
    assert!(setup(&fixture).changes().is_none());
}

/// An optional step's question and its rows, at the mockups' sizes, and
/// Jarvis's rows.
#[test]
fn mode_snapshots() {
    let mut fixture = on_the_modes();
    skip_to(&mut fixture, Step::ReadAloud);
    for (w, h) in [(60, 18), (90, 30)] {
        snapshot(&format!("setup-question-{w}x{h}"), &drawn(&fixture, w, h));
    }
    press(&mut fixture.app, "\n");
    for (w, h) in [(60, 18), (90, 30)] {
        snapshot(&format!("setup-fill-{w}x{h}"), &drawn(&fixture, w, h));
    }
    press(&mut fixture.app, "s\n");
    snapshot("setup-jarvis-90x30", &drawn(&fixture, 90, 30));
}

/// No size panics on a question or a mode's rows.
#[test]
fn no_mode_size_panics() {
    let mut question = on_the_modes();
    skip_to(&mut question, Step::Jarvis);
    let mut rows = on_the_modes();
    skip_to(&mut rows, Step::Jarvis);
    press(&mut rows.app, "\n");
    for fixture in [&question, &rows] {
        for w in [1, 10, 59, 60, 61, 89, 90, 120, 200] {
            for h in [1, 5, 17, 18, 19, 30, 60] {
                draw(&fixture.app, w, h);
            }
        }
    }
}

/// His first run: Start fresh over a file whose `[llm]` held only its key,
/// command mode answered with the defaults. The key was reset to the
/// default and the write left a bare `[llm]`; kept, the file's `[llm]`
/// stays as it was.
#[test]
fn command_mode_with_the_defaults_keeps_the_files_llm_key() {
    let toml = "[groq]\napi_key = \"gsk_file_1234\"\n\n[llm]\napi_key = \"sk_llm_1234\"\n";
    let mut fixture = setup_on(Some(toml), &[]);
    press(&mut fixture.app, "\n\n\n\n\n\n\n");
    skip_to(&mut fixture, Step::Command);
    press(&mut fixture.app, "\n\n");
    skip_modes(&mut fixture);
    assert!(setup(&fixture)
        .changes()
        .unwrap()
        .iter()
        .all(|(label, _, _)| !label.starts_with("[llm]")));
    assert_eq!(write(&mut fixture), Some(Effect::Checks));
    let text = std::fs::read_to_string(paths(fixture.dir.path()).config).unwrap();
    assert!(
        text.contains("[llm]\napi_key = \"sk_llm_1234\"\n"),
        "{text}"
    );
}

/// A mode judged complete on a provider key has that key written: read
/// aloud set on groq, its key typed when dictation was still groq, writes
/// it after dictation moved to deepgram. Each key is asked once.
#[test]
fn a_mode_writes_the_provider_key_it_counts_on() {
    let mut fixture = on_the_key_step();
    key(&mut fixture.app, KeyCode::Backspace);
    answer(&mut fixture, "general.backend", "deepgram");
    press(&mut fixture.app, "\n");
    assert_eq!(setup(&fixture).mode(), Mode::Text);
    type_text(&mut fixture.app, "dg_test_key_0001");
    press(&mut fixture.app, "\n\n\n\n\n\n");
    skip_to(&mut fixture, Step::ReadAloud);
    press(&mut fixture.app, "\n");
    assert_eq!(
        setup(&fixture)
            .editor
            .form
            .config()
            .tts
            .as_ref()
            .unwrap()
            .backend,
        "deepgram"
    );
    answer(&mut fixture, "tts.backend", "groq");
    press(&mut fixture.app, "\n");
    assert_eq!(setup(&fixture).step, Step::Jarvis);
    let asked = setup(&fixture).asked();
    assert!(asked.contains(&"groq.api_key".to_string()), "{asked:?}");
    for key in &asked {
        assert_eq!(asked.iter().filter(|k| *k == key).count(), 1, "{key} twice");
    }
    skip_modes(&mut fixture);
    assert_eq!(write(&mut fixture), Some(Effect::Checks));
    let text = std::fs::read_to_string(paths(fixture.dir.path()).config).unwrap();
    let written: whisrs::Config = toml::from_str(&text).unwrap();
    assert_eq!(
        written.groq.map(|g| g.api_key).as_deref(),
        Some("gsk_test_0123456789"),
        "{text}"
    );
    assert_eq!(written.tts.map(|t| t.backend).as_deref(), Some("groq"));

    // With a key of its own, read aloud counts on no other.
    let mut fixture = on_the_modes();
    skip_to(&mut fixture, Step::ReadAloud);
    press(&mut fixture.app, "\n");
    answer(&mut fixture, "tts.api_key", "gsk_tts_0002");
    assert!(!setup(&fixture)
        .asked()
        .iter()
        .any(|k| k.ends_with(".api_key") && k != "tts.api_key" && k != "groq.api_key"));
}

/// Recording on another provider's backend has that provider's key asked,
/// and with dictation's own backend, the key step's alone.
#[cfg(feature = "recording")]
#[test]
fn recording_asks_the_key_of_its_backend() {
    let mut fixture = on_the_modes();
    press(&mut fixture.app, "\n");
    let asked = setup(&fixture).asked();
    assert_eq!(asked.iter().filter(|k| *k == "groq.api_key").count(), 1);
    answer(&mut fixture, "recording.backend", "deepgram");
    assert!(setup(&fixture)
        .asked()
        .contains(&"deepgram.api_key".to_string()));
}

/// The file every test below starts fresh over: an LLM and a voice of its
/// own, each with its key, and the answers Start fresh gives otherwise.
const KEYED: &str = r#"[general]
remove_filler_words = true
audio_feedback = true

[groq]
api_key = "gsk_file_1234"

[llm]
api_url = "https://api.groq.com/openai/v1/chat/completions"
model = "llama-3.3-70b-versatile"
api_key = "gsk_llm_1234"

[tts]
enabled = true
backend = "openai"
api_key = "sk_tts_5678"
"#;

/// Start fresh keeps each key with what says where it goes: the LLM key
/// with its URL and model, the TTS key with its backend (read aloud does
/// not move to the dictation provider then). Set up as they come, the
/// modes change nothing in the file.
#[test]
fn start_fresh_keeps_each_key_with_where_it_goes() {
    let mut fixture = setup_on(Some(KEYED), &[]);
    press(&mut fixture.app, "\n\n\n\n\n\n\n");
    skip_to(&mut fixture, Step::Command);
    press(&mut fixture.app, "\n");
    let llm = setup(&fixture).editor.form.config().llm.clone().unwrap();
    assert_eq!(
        llm.api_url,
        "https://api.groq.com/openai/v1/chat/completions"
    );
    assert_eq!(llm.model, "llama-3.3-70b-versatile");
    press(&mut fixture.app, "\n\n");
    assert_eq!(
        setup(&fixture)
            .editor
            .form
            .config()
            .tts
            .as_ref()
            .unwrap()
            .backend,
        "openai"
    );
    press(&mut fixture.app, "\n");
    assert_eq!(setup(&fixture).step, Step::Jarvis);
    skip_modes(&mut fixture);
    assert_eq!(setup(&fixture).changes().unwrap(), []);
    assert_eq!(write(&mut fixture), Some(Effect::Checks));
    let text = std::fs::read_to_string(paths(fixture.dir.path()).config).unwrap();
    assert_eq!(text, KEYED);
}

/// A relative dir is taken from this terminal's folder and written out
/// whole: the daemon has a folder of its own. `~` stays as typed.
#[test]
fn jarvis_writes_a_relative_dir_out_whole() {
    let mut fixture = on_the_modes();
    skip_to(&mut fixture, Step::Jarvis);
    press(&mut fixture.app, "\n");
    // Tests run in the package's folder, which has `src`.
    answer(&mut fixture, "agents.0.dir", "src");
    press(&mut fixture.app, "\n");
    assert_eq!(setup(&fixture).step, Step::Review);
    let full = std::env::current_dir().unwrap().join("src");
    assert_eq!(
        setup(&fixture).editor.form.value("agents.0.dir"),
        Some(&Value::from(full.to_str().unwrap()))
    );
    key(&mut fixture.app, KeyCode::Backspace);
    answer(&mut fixture, "agents.0.dir", "~");
    press(&mut fixture.app, "\n");
    assert_eq!(setup(&fixture).step, Step::Review);
    assert_eq!(
        setup(&fixture).editor.form.value("agents.0.dir"),
        Some(&Value::from("~"))
    );
}

/// Without read aloud, Jarvis's intro says its replies would not be
/// heard; with it set up, it says nothing of it.
#[test]
fn jarvis_says_when_its_replies_would_not_be_heard() {
    let unheard = "Read aloud is not on, so you would not hear its replies.";
    let mut fixture = on_the_modes();
    skip_to(&mut fixture, Step::Jarvis);
    assert!(drawn(&fixture, 200, 60).join(" ").contains(unheard));
    let mut fixture = on_the_modes();
    skip_to(&mut fixture, Step::ReadAloud);
    press(&mut fixture.app, "\n\n");
    assert_eq!(setup(&fixture).step, Step::Jarvis);
    assert!(!drawn(&fixture, 200, 60)
        .join(" ")
        .contains("would not hear"));
}

/// A step change clears what the step before said, unless it says
/// something itself: a refusal does not stay on the next step, or on the
/// question Backspace goes back to; a skip's notice is its own.
#[test]
fn a_refusal_does_not_outlive_its_step() {
    let bin = tempfile::tempdir().unwrap();
    let claude = bin.path().join("claude");
    std::fs::write(&claude, "#!/bin/sh\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut fixture = setup_on(None, &[("PATH", bin.path().to_str().unwrap())]);
    press(&mut fixture.app, "\n");
    type_text(&mut fixture.app, "gsk_test_0123456789");
    press(&mut fixture.app, "\n\n\n\n\n\n");
    skip_to(&mut fixture, Step::Jarvis);
    assert!(status(&fixture).starts_with("skipped read aloud"));
    press(&mut fixture.app, "\n");
    assert_eq!(status(&fixture), "", "Set it up says nothing of the skip");
    press(&mut fixture.app, "\n");
    assert!(status(&fixture).starts_with("Jarvis needs a dir"));
    let dir = fixture.dir.path().to_str().unwrap().to_string();
    answer(&mut fixture, "agents.0.dir", &dir);
    press(&mut fixture.app, "\n");
    assert_eq!(setup(&fixture).step, Step::Review);
    assert_eq!(status(&fixture), "");

    key(&mut fixture.app, KeyCode::Backspace);
    answer(&mut fixture, "agents.0.dir", "");
    press(&mut fixture.app, "\n");
    assert!(status(&fixture).starts_with("Jarvis needs a dir"));
    key(&mut fixture.app, KeyCode::Backspace);
    assert!(shown_rows(&fixture).is_empty());
    assert_eq!(status(&fixture), "");
}
