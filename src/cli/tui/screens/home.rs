//! Home, the hub: the screens on the left, and on the right what the daemon
//! is doing (from `info`, or `status` for an older daemon) beside the
//! config's backend, language, profile and hotkeys.

use ratatui::layout::Rect;
use ratatui::Frame;
use whisrs::config::profile::BASE;
use whisrs::recording::RecordingPhase;
use whisrs::InfoAnswer;

use crate::tui::app::{App, Screen};
use crate::tui::keys::Action;
use crate::tui::widgets::frame::{self, Header};
use crate::tui::widgets::row::{Row, Value};
use crate::tui::widgets::{Notice, Tone, Ui};
use crate::tui::worker::DaemonView;

/// The screens Home opens, and their names.
const SCREENS: &[(Screen, &str)] = &[
    (Screen::Setup, "Setup"),
    (Screen::Config, "Config"),
    (Screen::Mic, "Mic test"),
    (Screen::Recordings, "Recordings"),
    (Screen::Profiles, "Profiles"),
    (Screen::History, "History"),
];

/// The list's width in the two-pane layout: the gutter, a space and a
/// 22-column label.
pub const LIST_WIDTH: u16 = 24;

/// Info labels and their gap.
const INFO_LABEL_WIDTH: usize = 12;

#[derive(Debug, Default)]
pub struct Home {
    cursor: usize,
}

impl Home {
    /// Handle `action`; the screen to open, if it asks for one.
    pub fn key(&mut self, action: Action) -> Option<Screen> {
        let last = SCREENS.len() - 1;
        match action {
            Action::Up => self.cursor = self.cursor.saturating_sub(1),
            Action::Down => self.cursor = (self.cursor + 1).min(last),
            Action::First => self.cursor = 0,
            Action::Last => self.cursor = last,
            Action::Enter | Action::Right | Action::Pane => return Some(SCREENS[self.cursor].0),
            _ => {}
        }
        None
    }

    /// Put the cursor on `screen`'s row: the one just left.
    pub fn point(&mut self, screen: Screen) {
        if let Some(at) = SCREENS.iter().position(|(s, _)| *s == screen) {
            self.cursor = at;
        }
    }
}

pub fn view(app: &App, f: &mut Frame, ui: &Ui) {
    let area = f.area();
    let two_panes = area.width >= frame::TWO_PANES;
    let mut info = info_rows(app, ui);
    let width = frame::body_width(area.width).saturating_sub(2);
    // A profile that did not load is said here, where the daemon is.
    let mut notices = app.notices.clone();
    if let DaemonView::Up(InfoAnswer::Info(info)) = &app.daemon {
        if let Some(error) = &info.profile_error {
            notices.push(Notice::new(Tone::Warn, error.clone()));
        }
    }
    let messages = frame::messages(&notices, app.status.as_ref(), ui, usize::from(width));
    let rows = |info: usize| match (two_panes, info) {
        (true, _) => SCREENS.len().max(info),
        (false, 0) => SCREENS.len(),
        (false, _) => SCREENS.len() + 1 + info,
    };
    let foot = if messages.is_empty() {
        0
    } else {
        messages.len() + 1
    };
    // What does not fit goes from the bottom of the info, which says the
    // least there (built with, then the hotkeys, ...): a notice is never
    // cut.
    let room = usize::from(frame::max_body(area));
    while !info.is_empty() && rows(info.len()) + foot > room {
        info.pop();
    }
    let rows = rows(info.len());
    let Some(placed) = frame::place(area, Some((rows + foot) as u16)) else {
        return frame::too_small(f, ui);
    };
    let body = frame::draw(
        f,
        placed.frame,
        ui,
        Header {
            screen: "home",
            profile: app.header_profile().as_deref(),
            daemon: app.daemon_up(),
            live: None,
        },
    );

    let line = |y: u16, x: u16, width: u16| Rect::new(x, y, width, 1);
    for (i, (_, name)) in SCREENS.iter().enumerate() {
        let y = body.y + i as u16;
        if y >= body.bottom() {
            break;
        }
        let row = Row {
            focused: i == app.home.cursor,
            ..Row::new(name, Value::default())
        };
        let width = if two_panes { LIST_WIDTH } else { body.width };
        row.draw(line(y, body.x, width), f.buffer_mut(), ui, 22);
    }

    let (info_x, info_y) = if two_panes {
        (body.x + LIST_WIDTH, body.y)
    } else {
        (body.x, body.y + SCREENS.len() as u16 + 1)
    };
    let info_width = body.right().saturating_sub(info_x);
    for (i, (label, value)) in info.into_iter().enumerate() {
        let y = info_y + i as u16;
        if y >= body.bottom() {
            break;
        }
        let row = Row {
            gutter: false,
            ..Row::new(label, value)
        };
        row.draw(
            line(y, info_x, info_width),
            f.buffer_mut(),
            ui,
            INFO_LABEL_WIDTH,
        );
    }

    let first = body.y + (rows + 1) as u16;
    for (i, text) in messages.into_iter().enumerate() {
        let y = first + i as u16;
        if y >= body.bottom() {
            break;
        }
        f.render_widget(text, line(y, body.x + 2, body.width.saturating_sub(2)));
    }

    frame::footer(
        f,
        placed.footer,
        ui,
        &[
            ("j/k", "move"),
            ("enter", "open"),
            ("?", "help"),
            ("q", "quit"),
        ],
    );
}

/// The profile row, once the folder is read: the state file's profile and
/// how many there are (`base (2 profiles)`), or, with the daemon on
/// another, that it applies when whisrsd restarts.
fn profile_row(app: &App, info: Option<&whisrs::DaemonInfo>) -> Option<Value> {
    let count = app.profiles.count()?;
    let active = app.profiles.active();
    let name = active.unwrap_or(BASE);
    let elsewhere =
        info.is_some_and(|info| info.profile_error.is_none() && info.profile.as_deref() != active);
    Some(match count {
        _ if elsewhere => {
            Value::plain(format!("{name},")).then("applies when whisrsd restarts", Tone::Muted)
        }
        0 => Value::plain(name),
        1 => Value::plain(name).then("(1 profile)", Tone::Muted),
        n => Value::plain(name).then(format!("({n} profiles)"), Tone::Muted),
    })
}

/// The right pane: the daemon, then the config.
fn info_rows(app: &App, ui: &Ui) -> Vec<(&'static str, Value)> {
    let config = app.config.form.config();
    let sep = ui.glyphs.separator;
    let mut rows = Vec::new();
    let info = match &app.daemon {
        DaemonView::Up(InfoAnswer::Info(info)) => Some(info),
        _ => None,
    };
    match &app.daemon {
        DaemonView::Checking => rows.push(("daemon", Value::muted("checking"))),
        DaemonView::Up(answer) => {
            let state = answer
                .state()
                .map_or_else(|| "unknown".to_string(), |s| s.to_string());
            let mut parts = vec![("running".to_string(), Tone::Ok), (state, Tone::Plain)];
            match answer {
                InfoAnswer::Info(info) => {
                    match info.recording {
                        Some(RecordingPhase::Recording) => {
                            parts.push(("recording".into(), Tone::Plain));
                        }
                        Some(RecordingPhase::Processing) => {
                            parts.push(("processing a recording".into(), Tone::Plain));
                        }
                        _ => {}
                    }
                    if info.agent_turn {
                        parts.push(("agent turn".into(), Tone::Plain));
                    }
                    if info.busy {
                        parts.push(("busy".into(), Tone::Muted));
                    }
                    if !info.version.is_empty() {
                        parts.push((format!("v{}", info.version), Tone::Plain));
                    }
                }
                InfoAnswer::Older(_) => {
                    parts.push(("older than this whisrs".into(), Tone::Muted));
                }
            }
            rows.push(("daemon", Value::joined(parts, sep)));
        }
        DaemonView::Down => {
            rows.push(("daemon", Value::of("not running", Tone::Warn)));
            let start = app
                .service
                .enable_hint()
                .unwrap_or(app.service.manual_start_hint());
            rows.push(("start", Value::plain(start)));
        }
        DaemonView::NoAnswer(why) => {
            rows.push((
                "daemon",
                Value::of("not answering", Tone::Warn).then(why.clone(), Tone::Muted),
            ));
        }
    }

    // The profile in use runs on its own backend and language.
    let profile = app
        .active_profile()
        .and_then(|name| app.profiles.item(&name));
    let backend = info
        .map(|info| info.backend.as_str())
        .filter(|backend| !backend.is_empty())
        .or_else(|| profile.and_then(|p| p.backend.as_deref()))
        .unwrap_or(&config.general.backend);
    let language = profile
        .and_then(|p| p.language.as_deref())
        .unwrap_or(&config.general.language);
    rows.push(("backend", Value::plain(backend)));
    rows.push(("language", Value::plain(language)));
    if let Some(profile) = profile_row(app, info) {
        rows.push(("profile", profile));
    }

    let hotkeys: Vec<(String, Tone)> = config
        .hotkeys
        .as_ref()
        .map(|h| {
            [
                ("toggle", &h.toggle),
                ("cancel", &h.cancel),
                ("command", &h.command),
                ("speak", &h.speak),
                ("recording", &h.recording),
            ]
            .into_iter()
            .filter_map(|(name, key)| Some((format!("{name} {}", key.as_deref()?), Tone::Plain)))
            .collect()
        })
        .unwrap_or_default();
    rows.push((
        "hotkeys",
        if hotkeys.is_empty() {
            Value::muted("none in [hotkeys]")
        } else {
            Value::joined(hotkeys, sep)
        },
    ));

    let features = info.map_or_else(whisrs::built_features, |info| info.features.clone());
    let features: Vec<&str> = features
        .iter()
        .map(|f| if f == "recording" { "rec" } else { f.as_str() })
        .collect();
    rows.push(("built with", Value::plain(features.join(" "))));
    rows
}
