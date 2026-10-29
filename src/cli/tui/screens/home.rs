//! Home, the hub: the screens on the left, and on the right what the daemon
//! is doing (from `info`, or `status` for an older daemon) beside the
//! config's backend, language and hotkeys.

use ratatui::layout::Rect;
use ratatui::Frame;
use whisrs::recording::RecordingPhase;
use whisrs::InfoAnswer;

use crate::tui::app::App;
use crate::tui::keys::Action;
use crate::tui::widgets::frame::{self, Header};
use crate::tui::widgets::row::{Row, Value};
use crate::tui::widgets::{Tone, Ui};
use crate::tui::worker::DaemonView;

/// The screens Home opens.
const SCREENS: &[&str] = &["Config"];

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
    /// Handle `action`; `true` opens the screen under the cursor.
    pub fn key(&mut self, action: Action) -> bool {
        let last = SCREENS.len() - 1;
        match action {
            Action::Up => self.cursor = self.cursor.saturating_sub(1),
            Action::Down => self.cursor = (self.cursor + 1).min(last),
            Action::First => self.cursor = 0,
            Action::Last => self.cursor = last,
            Action::Enter | Action::Right | Action::Pane => return true,
            _ => {}
        }
        false
    }
}

pub fn view(app: &App, f: &mut Frame, ui: &Ui) {
    let area = f.area();
    let two_panes = area.width >= frame::TWO_PANES;
    let info = info_rows(app, ui);
    let width = frame::body_width(area.width).saturating_sub(2);
    let messages = app.messages(ui, usize::from(width));
    let rows = if two_panes {
        SCREENS.len().max(info.len())
    } else {
        SCREENS.len() + 1 + info.len()
    };
    let foot = if messages.is_empty() {
        0
    } else {
        messages.len() + 1
    };
    let Some(placed) = frame::place(area, Some((rows + foot) as u16)) else {
        return frame::too_small(f, ui);
    };
    let body = frame::draw(
        f,
        placed.frame,
        ui,
        Header {
            screen: "home",
            daemon: app.daemon_up(),
        },
    );

    let line = |y: u16, x: u16, width: u16| Rect::new(x, y, width, 1);
    for (i, name) in SCREENS.iter().enumerate() {
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

    let backend = info
        .map(|info| info.backend.as_str())
        .filter(|backend| !backend.is_empty())
        .unwrap_or(&config.general.backend);
    rows.push(("backend", Value::plain(backend)));
    rows.push(("language", Value::plain(&config.general.language)));

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
