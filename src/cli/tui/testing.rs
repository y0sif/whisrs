//! Fixtures for the TUI's tests: an app on a config in a temp dir, keys
//! pressed through the pure update, and frames drawn on ratatui's
//! `TestBackend` as row strings.

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use whisrs::recording::RecordingPhase;
use whisrs::service::ServiceManager;
use whisrs::{DaemonInfo, InfoAnswer, State};

use super::app::{App, Effect, Msg};
use super::glyphs::Glyphs;
use super::theme::{Depth, Ground, Rgb, Theme};
use super::widgets::Ui;
use super::worker::DaemonView;
use super::{Entry, Env, Paths};

/// An app and the temp dir its files are in.
pub struct Fixture {
    pub dir: tempfile::TempDir,
    pub app: App,
}

/// The paths of a config in `dir`.
pub fn paths(dir: &std::path::Path) -> Paths {
    Paths {
        config: dir.join("config.toml"),
        vocabulary: dir.join("vocabulary.txt"),
        socket: dir.join("whisrs.sock"),
        models: dir.join("models"),
        xdg_config: dir.join("xdg-config"),
        xdg_data: dir.join("xdg-data"),
        profiles: dir.join("profiles"),
        state: dir.join("state").join("active-profile"),
    }
}

/// `whisrs config` on `toml` (no file for `None`), with `env`.
pub fn app_on(toml: Option<&str>, env: &[(&str, &str)]) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let paths = paths(dir.path());
    if let Some(toml) = toml {
        std::fs::write(&paths.config, toml).unwrap();
    }
    let app = App::prepare(
        Entry::Config,
        paths,
        Env::from_pairs(env),
        ServiceManager::Systemd,
    )
    .expect("the fixture loads");
    Fixture { dir, app }
}

pub fn app(toml: &str) -> Fixture {
    app_on(Some(toml), &[])
}

/// `whisrs setup` on `toml` (no file for `None`), with `env`; `HOME` is
/// the temp dir unless `env` names one, so paths show as `~/...`.
pub fn setup_on(toml: Option<&str>, env: &[(&str, &str)]) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let paths = paths(dir.path());
    if let Some(toml) = toml {
        std::fs::write(&paths.config, toml).unwrap();
    }
    let home = dir.path().to_str().unwrap().to_string();
    let mut env: Vec<(&str, &str)> = env.to_vec();
    if !env.iter().any(|(key, _)| *key == "HOME") {
        env.push(("HOME", &home));
    }
    let app = App::prepare(
        Entry::Setup,
        paths,
        Env::from_pairs(&env),
        ServiceManager::Systemd,
    )
    .expect("the fixture loads");
    Fixture { dir, app }
}

/// Press a key that has no character: Backspace, the arrows.
pub fn key(app: &mut App, code: KeyCode) -> Option<Effect> {
    app.update(Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)))
}

/// The config the mockups show: deepgram-streaming with a key in the file,
/// two hotkeys and a typo'd key.
pub const MOCKUP: &str = r#"# hand-tuned
[general]
backend = "deepgram-streaming"
language = "en"
silense_timeout_ms = 1500

[deepgram]
api_key = "dg-0123456789abcdef9f2c"
model = "nova-3"

[local-whisper]
model_path = "~/.local/share/whisrs/models/ggml-base.en.bin"

[hotkeys]
toggle = "Alt+D"
recording = "Alt+Shift+M"
"#;

/// The mockups' app: [`MOCKUP`], a Groq key in this shell, a daemon that
/// answered `info`, and the profiles folder read (none: base).
pub fn mockup() -> Fixture {
    mockup_with("")
}

/// [`mockup`] with `extra` after [`MOCKUP`]'s text.
pub fn mockup_with(extra: &str) -> Fixture {
    mockup_on(&format!("{MOCKUP}{extra}"))
}

/// [`mockup`] on `toml` in place of [`MOCKUP`].
pub fn mockup_on(toml: &str) -> Fixture {
    let mut fixture = app_on(Some(toml), &[("WHISRS_GROQ_API_KEY", "gsk_live_1234")]);
    let info = DaemonInfo {
        version: "0.2.0".into(),
        state: Some(State::Idle),
        recording: Some(RecordingPhase::Idle),
        profile: None,
        profile_error: None,
        backend: "deepgram-streaming".into(),
        features: [
            "local-whisper",
            "tray",
            "overlay",
            "hooks",
            "recording",
            "mcp",
        ]
        .map(String::from)
        .to_vec(),
        started_at: 1,
        busy: false,
        agent_turn: false,
    };
    fixture
        .app
        .update(Msg::Daemon(DaemonView::Up(InfoAnswer::Info(info))));
    read_profiles(&mut fixture.app);
    fixture
}

/// The profiles folder of `app`'s temp dir read, as the worker reads it.
pub fn read_profiles(app: &mut App) {
    let folder = super::screens::profiles::read_folder(&app.paths.profile_paths());
    app.update(Msg::ProfilesRead(folder));
}

/// Three profiles for the mockup's config: `offline` (local whisper, in
/// German, commented), `work` (French, with base-only keys, a Groq key
/// among them) and `broken` (not TOML).
pub const PROFILES: &[(&str, &str)] = &[
    (
        "offline",
        "# on the train\n[general]\nbackend = \"local-whisper\"\nlanguage = \"de\"\n\n\
         [local-whisper]\nmodel_path = \"/m/ggml-base.bin\"\n",
    ),
    (
        "work",
        "[general]\nlanguage = \"fr\"\n[hotkeys]\ntoggle = \"Super+W\"\n\
         [groq]\napi_key = \"gsk_profile_secret\"\n",
    ),
    ("broken", "[general\nbackend = \"groq\"\n"),
];

/// [`mockup`] with `profiles` in `profiles/`, `active` in the state file,
/// the folder read.
pub fn mockup_profiles(profiles: &[(&str, &str)], active: Option<&str>) -> Fixture {
    with_profiles(mockup(), profiles, active)
}

/// `fixture` with `profiles` in `profiles/`, `active` in the state file,
/// the folder read.
pub fn with_profiles(
    mut fixture: Fixture,
    profiles: &[(&str, &str)],
    active: Option<&str>,
) -> Fixture {
    let paths = fixture.app.paths.profile_paths();
    std::fs::create_dir_all(&paths.profiles).unwrap();
    for (name, text) in profiles {
        std::fs::write(paths.profile(name), text).unwrap();
    }
    if let Some(active) = active {
        whisrs::config::profile::set_active(&paths.state, Some(active)).unwrap();
    }
    read_profiles(&mut fixture.app);
    fixture
}

/// A daemon that answered `info` as the mockup's does, on `profile`, with
/// `error` for a profile that did not load, started at `at`. It names no
/// backend, so Home shows the profile's.
pub fn daemon_on(app: &mut App, profile: Option<&str>, error: Option<&str>, at: u64) {
    let info = DaemonInfo {
        version: "0.2.0".into(),
        state: Some(State::Idle),
        recording: Some(RecordingPhase::Idle),
        profile: profile.map(String::from),
        profile_error: error.map(String::from),
        features: [
            "local-whisper",
            "tray",
            "overlay",
            "hooks",
            "recording",
            "mcp",
        ]
        .map(String::from)
        .to_vec(),
        started_at: at,
        ..Default::default()
    };
    app.update(Msg::Daemon(DaemonView::Up(InfoAnswer::Info(info))));
}

/// Profiles open from Home, the folder read.
pub fn open_profiles(fixture: &mut Fixture) {
    press(&mut fixture.app, "\x1bgjjjj");
    assert_eq!(press(&mut fixture.app, "\n"), [Effect::ReadProfiles]);
    read_profiles(&mut fixture.app);
}

/// Profile mode on `name` from Profiles, read as the loop reads it.
pub fn open_profile(fixture: &mut Fixture, name: &str) {
    let load = super::screens::config::load_profile(&fixture.app.paths, name);
    fixture
        .app
        .update(Msg::ProfileOpened(name.to_string(), load));
}

/// Press each key of `keys`: a character, or `\n` Enter, `\x1b` Esc, `\t`
/// Tab. The effects they asked for, in order.
pub fn press(app: &mut App, keys: &str) -> Vec<Effect> {
    keys.chars()
        .filter_map(|c| {
            let code = match c {
                '\n' => KeyCode::Enter,
                '\x1b' => KeyCode::Esc,
                '\t' => KeyCode::Tab,
                c => KeyCode::Char(c),
            };
            app.update(Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        })
        .collect()
}

/// Press one key with `modifiers`.
pub fn chord(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Option<Effect> {
    app.update(Msg::Key(KeyEvent::new(code, modifiers)))
}

/// Type `text` into an open input.
pub fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )));
    }
}

/// A dark truecolor terminal, carbon.
pub fn theme() -> Theme {
    Theme::new(
        &Default::default(),
        Depth::Truecolor,
        Ground {
            light: false,
            colors: Some((Rgb(220, 220, 220), Rgb(20, 20, 24))),
        },
    )
}

/// `app` drawn on a `width` by `height` terminal.
pub fn draw(app: &App, width: u16, height: u16) -> Buffer {
    let theme = theme();
    let glyphs = Glyphs::UNICODE;
    let ui = Ui {
        theme: &theme,
        glyphs: &glyphs,
    };
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| app.view(f, &ui)).unwrap();
    terminal.backend().buffer().clone()
}

/// A buffer's rows, trailing spaces off.
pub fn rows(buf: &Buffer) -> Vec<String> {
    (0..buf.area.height)
        .map(|y| {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            row.trim_end().to_string()
        })
        .collect()
}

/// Compare `rows` with the snapshot `name` in `src/cli/tui/snapshots/`;
/// `WHISRS_UPDATE_SNAPSHOTS=1` writes it instead.
pub fn assert_snapshot(name: &str, rows: &[String]) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/cli/tui/snapshots")
        .join(format!("{name}.txt"));
    let actual = rows.join("\n") + "\n";
    if std::env::var_os("WHISRS_UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("no snapshot {}: {e}", path.display()));
    assert_eq!(
        actual,
        expected,
        "{name} differs from its snapshot ({}); WHISRS_UPDATE_SNAPSHOTS=1 rewrites it",
        path.display()
    );
}
