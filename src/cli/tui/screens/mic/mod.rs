//! Mic: a live level meter for the microphone `[audio] device` names, and
//! that row to change it. The capture runs on a worker, from the moment the
//! screen opens until it is left; `w` saves the device as Config's `w` does.
//!
//! [`Meter`] is the meter's state, shared with setup's microphone step.

pub mod analysis;

use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use whisrs::config::setup::mic::{Probe, ProbeError};

use self::analysis::MicReading;
use crate::tui::app::{App, MicEvent};
use crate::tui::screens::config;
use crate::tui::widgets::frame::{self, Header};
use crate::tui::widgets::{fit, meter, wrap, Tone, Ui};

/// How long the meter holds a peak.
pub const PEAK_HOLD: Duration = Duration::from_millis(1500);

/// The label column of the meter's lines: the device row's, `device` and
/// its gap, as Config's rows draw it.
pub const LABEL_WIDTH: usize = 8;

/// What the meter shows: the capture's latest reading, its held peak, the
/// device's format, or why it failed.
#[derive(Debug, Clone, Default)]
pub struct Meter {
    /// The session whose readings count; a late one from an earlier
    /// capture is dropped.
    pub session: u64,
    pub reading: Option<MicReading>,
    held: Option<(f32, Instant)>,
    pub probe: Option<Result<Probe, ProbeError>>,
    pub failed: Option<String>,
}

impl Meter {
    /// A new capture, `session`: nothing read yet.
    pub fn start(&mut self, session: u64) {
        *self = Meter {
            session,
            ..Meter::default()
        };
    }

    /// Take `event` of `session`.
    pub fn event(&mut self, session: u64, event: MicEvent) {
        if session != self.session {
            return;
        }
        match event {
            MicEvent::Probed(probe) => self.probe = Some(probe),
            MicEvent::Failed(why) => self.failed = Some(why),
            MicEvent::Reading(reading, at) => {
                let keep = self.held.is_some_and(|(peak, since)| {
                    reading.peak_dbfs < peak && at.duration_since(since) < PEAK_HOLD
                });
                if !keep {
                    self.held = Some((reading.peak_dbfs, at));
                }
                self.reading = Some(reading);
            }
        }
    }

    /// The level now, 0 (the meter's floor) to 1 (full scale), for the
    /// header's waveform; `None` before a reading.
    pub fn level(&self) -> Option<f32> {
        let floor = crate::tui::widgets::meter::FLOOR_DBFS;
        self.reading
            .map(|r| ((r.rms_dbfs - floor) / -floor).clamp(0.0, 1.0))
    }

    /// The peak held now.
    pub fn held(&self) -> Option<f32> {
        self.held.map(|(peak, _)| peak)
    }

    /// The meter's lines, `width` columns wide: the format, the level, the
    /// gate and peak, and the quality report when there is one.
    pub fn lines(&self, width: usize, ui: &Ui) -> Vec<Line<'static>> {
        let label = |text: &str| Span::styled(format!("{text:<LABEL_WIDTH$}"), ui.theme.muted);
        let room = width.saturating_sub(LABEL_WIDTH + 2);
        let indent = " ".repeat(LABEL_WIDTH + 2);
        let line = |name: &str, spans: Vec<Span<'static>>| {
            let mut all = vec![Span::raw("  "), label(name)];
            all.extend(spans);
            Line::from(all)
        };
        let mut lines = Vec::new();
        let format = match &self.probe {
            None => Span::styled("looking...", ui.theme.muted),
            Some(Ok(probe)) => Span::raw(fit(
                &format!(
                    "{} Hz, {} {}, captured as 16000 Hz mono",
                    probe.rate,
                    probe.channels,
                    if probe.channels == 1 {
                        "channel"
                    } else {
                        "channels"
                    }
                ),
                room,
                ui.glyphs.ellipsis,
            )),
            Some(Err(e)) => Span::styled(fit(&e.error, room, ui.glyphs.ellipsis), ui.theme.error),
        };
        lines.push(line("format", vec![format]));
        if let Some(Err(e)) = &self.probe {
            let names: Vec<&str> = e.devices.iter().map(|d| d.id.as_str()).collect();
            let text = if names.is_empty() {
                "no input devices: is the microphone connected and the sound server running?"
                    .to_string()
            } else {
                format!("there are: {}", names.join(", "))
            };
            for part in wrap(&text, room.max(1)).into_iter().take(2) {
                lines.push(Line::from(vec![
                    Span::raw(indent.clone()),
                    Span::styled(part, ui.theme.muted),
                ]));
            }
        }
        lines.push(Line::default());
        let gate = analysis::gate_dbfs();
        match (&self.failed, &self.reading) {
            (Some(why), _) => {
                for (i, part) in wrap(why, room.max(1)).into_iter().take(3).enumerate() {
                    let tone = Span::styled(part, Tone::Error.style(ui.theme));
                    if i == 0 {
                        lines.push(line("level", vec![tone]));
                    } else {
                        lines.push(Line::from(vec![Span::raw(indent.clone()), tone]));
                    }
                }
            }
            (None, None) => lines.push(line(
                "level",
                vec![Span::styled("opening the microphone...", ui.theme.muted)],
            )),
            (None, Some(reading)) => {
                let bar = meter::bar(reading.rms_dbfs, gate, self.held(), room, ui);
                lines.push(line("level", bar.spans));
                let peak = self.held().map_or_else(String::new, |peak| {
                    format!(" {} peak {}", ui.glyphs.separator, meter::number(peak))
                });
                lines.push(Line::from(vec![
                    Span::raw(indent.clone()),
                    Span::styled(format!("gate {:.0}{peak}", gate), ui.theme.muted),
                ]));
                if let Some(quality) = &reading.quality {
                    let text = format!(
                        "speech {} {sep} noise {:.0} dBFS {sep} SNR {:.0} dB",
                        if quality.speech { "yes" } else { "no" },
                        quality.noise_dbfs,
                        quality.snr_db,
                        sep = ui.glyphs.separator
                    );
                    lines.push(line("quality", vec![Span::raw(text)]));
                }
            }
        }
        lines
    }
}

/// The text under the meter.
const HINT: &str = "Say a sentence. The bar passes the gate while you talk and drops under it \
                    when you stop.";

pub fn view(app: &App, f: &mut Frame, ui: &Ui) {
    let area = f.area();
    let width = usize::from(frame::body_width(area.width).saturating_sub(2));
    let messages = app.messages(ui, width);
    let meter = app.mic.lines(width, ui);
    let hint = wrap(HINT, width.saturating_sub(2).max(1));
    let rows = 2 + meter.len() + 1 + hint.len() + 1 + messages.len();
    let Some(placed) = frame::place(area, Some(rows as u16)) else {
        return frame::too_small(f, ui);
    };
    let body = frame::draw(
        f,
        placed.frame,
        ui,
        Header {
            screen: "mic",
            profile: app.header_profile().as_deref(),
            daemon: app.daemon_up(),
            live: app.mic.level(),
        },
    );
    let mut y = body.y;
    let row = Rect::new(body.x, y, body.width, 1);
    config::draw_rows(
        &app.config,
        &app.env,
        app.flash.as_deref(),
        f,
        row,
        ui,
        config::Pane {
            crumb: false,
            help: false,
        },
    );
    y += 2;
    let put = |f: &mut Frame, line: Line<'static>, y: &mut u16| {
        if *y < body.bottom() {
            f.render_widget(Paragraph::new(line), Rect::new(body.x, *y, body.width, 1));
        }
        *y += 1;
    };
    for line in meter {
        put(f, line, &mut y);
    }
    y += 1;
    for part in hint {
        put(
            f,
            Line::from(vec![Span::raw("  "), Span::styled(part, ui.theme.muted)]),
            &mut y,
        );
    }
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
    if !config::draw_open_footer(&app.config, f, placed.footer, ui) {
        frame::footer(
            f,
            placed.footer,
            ui,
            &[
                ("h/l", "device"),
                ("enter", "pick"),
                ("w", "save device"),
                ("esc", "back"),
                ("?", "help"),
            ],
        );
    }
    config::draw_open(&app.config, f, placed.frame, body, ui);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(rms: f32, peak: f32) -> MicEvent {
        MicEvent::Reading(
            MicReading {
                rms_dbfs: rms,
                peak_dbfs: peak,
                quality: None,
            },
            Instant::now(),
        )
    }

    /// A peak is held for 1.5 s unless a higher one comes, and a reading
    /// of an earlier capture is dropped.
    #[test]
    fn the_peak_is_held() {
        let mut meter = Meter::default();
        meter.start(2);
        let at = Instant::now();
        let at_ = |ms: u64, rms: f32, peak: f32| {
            MicEvent::Reading(
                MicReading {
                    rms_dbfs: rms,
                    peak_dbfs: peak,
                    quality: None,
                },
                at + Duration::from_millis(ms),
            )
        };
        meter.event(2, at_(0, -30.0, -10.0));
        meter.event(2, at_(500, -40.0, -25.0));
        assert_eq!(meter.held(), Some(-10.0));
        meter.event(2, at_(900, -20.0, -5.0));
        assert_eq!(meter.held(), Some(-5.0));
        meter.event(2, at_(2500, -50.0, -40.0));
        assert_eq!(meter.held(), Some(-40.0));
        meter.event(1, reading(-1.0, -1.0));
        assert_eq!(meter.reading.unwrap().rms_dbfs, -50.0);
        meter.event(2, MicEvent::Failed("gone".into()));
        assert_eq!(meter.failed.as_deref(), Some("gone"));
        meter.start(3);
        assert!(meter.reading.is_none() && meter.failed.is_none());
    }
}
