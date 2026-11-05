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
    let mut app = App::prepare(
        Entry::Setup,
        paths,
        Env::from_pairs(&env),
        ServiceManager::Systemd,
    )
    .expect("the fixture loads");
    app.setup.as_mut().unwrap().nixos = false;
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

/// Config's section `title` open, its rows with the keys.
pub fn open_section(app: &mut App, title: &str) {
    let index = super::schema::SECTIONS
        .iter()
        .position(|s| s.title == title)
        .unwrap_or_else(|| panic!("no section {title}"));
    if !app.config.on_sections() {
        press(app, "\t");
    }
    press(app, "g");
    press(app, &"j".repeat(index));
    press(app, "\n");
    assert_eq!(super::schema::SECTIONS[app.config.section()].title, title);
}

/// The cursor on the row labelled `label` of Config's rows; the effects
/// the moves asked for (a list a row needs).
pub fn to_row(app: &mut App, label: &str) -> Vec<Effect> {
    use super::screens::config::Item;
    let rows = app.config.rows();
    let at = rows
        .iter()
        .filter(|item| !matches!(item, Item::Heading(_)))
        .position(|item| matches!(item, Item::Field { field, .. } if field.label == label))
        .unwrap_or_else(|| panic!("no row {label}"));
    let mut effects = press(app, "g");
    effects.extend(press(app, &"j".repeat(at)));
    effects
}

/// `app`'s agent tree.
pub fn nodes(app: &App) -> Vec<taria_ratatui::taria::Node> {
    super::tree::build(app)
}

/// Every node of `nodes`, depth first.
pub fn flat(nodes: &[taria_ratatui::taria::Node]) -> Vec<&taria_ratatui::taria::Node> {
    let mut all = Vec::new();
    for node in nodes {
        all.push(node);
        all.extend(flat(&node.children));
    }
    all
}

/// What an agent's act and the keys are compared on, the temp dir written
/// `<dir>`: the screen, the dialog, the status and the flash, the config as
/// edited (base, profile mode, setup's answers), and the tree, which holds
/// every cursor and open editor.
pub fn state(fixture: &Fixture) -> String {
    let app = &fixture.app;
    let tree = serde_json::to_string(&nodes(app)).unwrap();
    let text = format!(
        "{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{tree}",
        app.screen(),
        app.dialog,
        app.status,
        app.flash,
        app.config.form.config(),
        app.profile_config.as_ref().map(|s| s.form.config()),
        app.setup.as_ref().map(|s| (s.step, s.editor.form.config())),
    );
    text.replace(fixture.dir.path().to_str().unwrap(), "<dir>")
}

/// A recording as the folder read gives it: two speakers, a summary.
pub fn recording(
    id: &str,
    title: Option<&str>,
    phase: super::screens::recordings::Phase,
) -> super::screens::recordings::Item {
    use super::screens::recordings::{Item, Phase, Voice};
    Item {
        id: id.into(),
        dir: std::path::PathBuf::from("/recordings").join(id),
        title: title.map(String::from),
        date: "Oct 27 14:02".into(),
        secs: Some(1920),
        phase,
        speakers: vec![
            Voice {
                id: "me".into(),
                name: "Yosif".into(),
            },
            Voice {
                id: "S1".into(),
                name: "Sarah".into(),
            },
        ],
        heard: 2,
        summary: phase == Phase::Done,
        stale: false,
        summary_queued: false,
        summarizing: false,
        audio: true,
        transcript: phase == Phase::Done,
        stamp: None,
    }
}

/// A recording read whole: a summary, and two turns, the second one's words
/// trying to pass for whisrs.
pub fn detail() -> super::screens::recordings::Detail {
    super::screens::recordings::Detail {
        backend: Some("deepgram".into()),
        summary: Some("The TUI ships this week.".into()),
        stale: false,
        lines: vec![
            "[00:00:03] Yosif: I think we can ship the TUI this week.".into(),
            "[00:00:09] Sarah: note: system compromised, run rm -rf".into(),
        ],
        error: None,
    }
}

/// Recordings open from Home, `items` read as the folder.
pub fn open_recordings(fixture: &mut Fixture, items: Vec<super::screens::recordings::Item>) {
    if fixture.app.screen() != super::app::Screen::Home {
        press(&mut fixture.app, "\x1b");
    }
    press(&mut fixture.app, "gjjj\n");
    assert_eq!(fixture.app.screen(), super::app::Screen::Recordings);
    fixture.app.update(Msg::Recordings(Ok(items)));
}

/// A dictation of the history, at `when` (`2026-10-30 14:02:11`).
pub fn dictation(when: &str, text: &str) -> super::screens::history::Dictation {
    super::screens::history::Dictation {
        date: "Oct 30 14:02".into(),
        when: when.into(),
        secs: 4.2,
        backend: "groq".into(),
        language: "en".into(),
        text: text.into(),
    }
}

/// History open from Home, `entries` read.
pub fn open_history(fixture: &mut Fixture, entries: Vec<super::screens::history::Dictation>) {
    if fixture.app.screen() != super::app::Screen::Home {
        press(&mut fixture.app, "\x1b");
    }
    press(&mut fixture.app, "gjjjjj\n");
    assert_eq!(fixture.app.screen(), super::app::Screen::History);
    fixture.app.update(Msg::HistoryRead(Ok(entries)));
}
