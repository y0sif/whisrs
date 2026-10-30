//! What the screens draw: snapshots at five sizes (the mockups are the
//! 90x30 ones), the styles only the focused row gets, the layouts by width,
//! and no panic at any size.

use ratatui::buffer::Buffer;
use ratatui::style::Modifier;

use super::testing::{assert_snapshot, draw, mockup, mockup_with, press, rows, theme, Fixture};

const SIZES: [(u16, u16); 5] = [(59, 17), (60, 18), (89, 30), (90, 30), (200, 60)];

/// Home, as the mockup has it.
fn home() -> Fixture {
    let mut fixture = mockup();
    press(&mut fixture.app, "\x1b");
    fixture
}

/// Config with the cursor on the Deepgram key, as the mockup has it.
fn config() -> Fixture {
    let mut fixture = mockup();
    press(&mut fixture.app, "j\nj");
    fixture
}

/// Config with the language picker open on `ger`, as the mockup has it.
fn picker() -> Fixture {
    let mut fixture = mockup();
    press(&mut fixture.app, "\n\n");
    for c in "ger".chars() {
        press(&mut fixture.app, &c.to_string());
    }
    fixture
}

/// The toggle hotkey's dialog, Super added to its Alt+D.
fn hotkey() -> Fixture {
    let mut fixture = mockup();
    // Hotkeys is the seventh section.
    press(&mut fixture.app, "jjjjjj\n\n ");
    fixture
}

/// Mic with the yeti picked and a reading in, as the mockup has it.
fn mic() -> Fixture {
    use std::time::Instant;

    use whisrs::config::setup::mic::Probe;

    use super::app::{MicEvent, Msg};
    use super::screens::mic::analysis::MicReading;

    let mut fixture = mockup();
    press(&mut fixture.app, "\x1bj\n");
    for event in [
        MicEvent::Probed(Ok(Probe {
            name: "default".into(),
            rate: 48_000,
            channels: 2,
        })),
        MicEvent::Reading(
            MicReading {
                rms_dbfs: -18.0,
                peak_dbfs: -12.0,
                quality: None,
            },
            Instant::now(),
        ),
    ] {
        fixture.app.update(Msg::Mic { session: 1, event });
    }
    fixture
}

#[test]
fn mic_snapshots() {
    let fixture = mic();
    for (w, h) in SIZES {
        assert_snapshot(&format!("mic-{w}x{h}"), &rows(&draw(&fixture.app, w, h)));
    }
}

#[test]
fn picker_and_hotkey_snapshots() {
    for (w, h) in [(60, 18), (90, 30)] {
        assert_snapshot(
            &format!("picker-{w}x{h}"),
            &rows(&draw(&picker().app, w, h)),
        );
        assert_snapshot(
            &format!("hotkey-{w}x{h}"),
            &rows(&draw(&hotkey().app, w, h)),
        );
    }
}

/// Hotkeys with one agent, whose two keys follow the `[hotkeys]` rows.
fn hotkeys_with_an_agent() -> Fixture {
    let mut fixture =
        mockup_with("\n[[agents]]\nname = \"jarvis\"\nhotkey = \"Super+J\"\ndir = \"/tmp\"\n");
    // Hotkeys is the seventh section; the cursor on the agent's key.
    press(&mut fixture.app, "jjjjjj\njjjjj");
    fixture
}

#[test]
fn hotkeys_section_snapshots() {
    for (w, h) in [(60, 18), (90, 30)] {
        assert_snapshot(
            &format!("hotkeys-agent-{w}x{h}"),
            &rows(&draw(&hotkeys_with_an_agent().app, w, h)),
        );
    }
}

/// On Home a notice is never cut: in one pane, the info rows that do not
/// fit go from the bottom first.
#[test]
fn a_notice_is_never_cut_on_home() {
    let fixture = home();
    let short = rows(&draw(&fixture.app, 60, 18));
    assert!(short
        .iter()
        .any(|r| r.contains("! unknown key [general] silense_timeout_ms is")));
    assert!(short
        .iter()
        .any(|r| r.trim_matches(['\u{2502}', ' ']) == "ignored"));
    assert!(!short.iter().any(|r| r.contains("built with")));
    assert!(short.iter().any(|r| r.contains("hotkeys")));
    let tall = rows(&draw(&fixture.app, 60, 24));
    assert!(tall.iter().any(|r| r.contains("built with")));
    assert!(tall
        .iter()
        .any(|r| r.trim_matches(['\u{2502}', ' ']) == "ignored"));
}

#[test]
fn home_snapshots() {
    let fixture = home();
    for (w, h) in SIZES {
        assert_snapshot(&format!("home-{w}x{h}"), &rows(&draw(&fixture.app, w, h)));
    }
}

#[test]
fn config_snapshots() {
    let fixture = config();
    for (w, h) in SIZES {
        assert_snapshot(&format!("config-{w}x{h}"), &rows(&draw(&fixture.app, w, h)));
    }
}

/// The rows with any bold cell.
fn bold_rows(buf: &Buffer) -> Vec<u16> {
    (0..buf.area.height)
        .filter(|&y| (0..buf.area.width).any(|x| buf[(x, y)].modifier.contains(Modifier::BOLD)))
        .collect()
}

/// Where `text` starts on screen.
fn find(buf: &Buffer, text: &str) -> Option<(u16, u16)> {
    rows(buf).iter().enumerate().find_map(|(y, row)| {
        let byte = row.find(text)?;
        Some((row[..byte].chars().count() as u16, y as u16))
    })
}

#[test]
fn only_the_focused_row_is_bold_and_has_the_accent_gutter() {
    let fixture = config();
    let buf = draw(&fixture.app, 90, 30);
    let (x, y) = find(&buf, "\u{258C} deepgram api_key").expect("the focused row");
    assert_eq!(bold_rows(&buf), vec![y]);
    assert_eq!(buf[(x, y)].fg, theme().accent.fg.unwrap());
    // The focused value is in the accent too.
    let (vx, _) = find(&buf, "set ****9f2c").unwrap();
    assert_eq!(buf[(vx, y)].fg, theme().accent.fg.unwrap());
    // One gutter on screen: the section list has no focus.
    let gutters = rows(&buf).iter().filter(|r| r.contains('\u{258C}')).count();
    assert_eq!(gutters, 1);

    // On Home, the focused row is Config's.
    let fixture = home();
    let buf = draw(&fixture.app, 90, 30);
    let (_, y) = find(&buf, "\u{258C} Config").expect("Config is focused");
    assert_eq!(bold_rows(&buf), vec![y]);
}

#[test]
fn a_choice_shows_its_arrows_on_the_focused_row_only() {
    let mut fixture = config();
    press(&mut fixture.app, "k");
    let buf = draw(&fixture.app, 90, 30);
    assert!(find(&buf, "\u{25C2} deepgram-streaming \u{25B8}").is_some());
    press(&mut fixture.app, "j");
    let buf = draw(&fixture.app, 90, 30);
    assert!(find(&buf, "\u{25C2}").is_none());
    assert!(find(&buf, "deepgram-streaming").is_some());
}

#[test]
fn the_screen_behind_a_dialog_is_dimmed() {
    let mut fixture = config();
    press(&mut fixture.app, "?");
    let buf = draw(&fixture.app, 90, 30);
    let dim = |x, y| {
        let cell: &ratatui::buffer::Cell = &buf[(x, y)];
        cell.modifier.contains(Modifier::DIM)
    };
    // The frame's corner and the header, behind the dialog.
    assert!(dim(3, 0));
    let (x, y) = find(&buf, "whisrs").unwrap();
    assert!(dim(x, y));
    assert_eq!(buf[(x, y)].fg, theme().faint.fg.unwrap());
    // The dialog itself is not dimmed, and is thick.
    let (x, y) = find(&buf, "first and last").expect("the help dialog");
    assert!(!dim(x, y));
    assert!(rows(&buf).iter().any(|r| r.contains('\u{250F}')));
    // Its keys keep the accent.
    let (x, y) = find(&buf, "j k").unwrap();
    assert_eq!(buf[(x, y)].fg, theme().accent.fg.unwrap());
}

/// Acceptance criterion 6: two panes at 90 columns and up, one with
/// drill-in from 60 to 89, the one line below 60x18.
#[test]
fn the_layout_follows_the_width() {
    let fixture = mockup();
    let two = rows(&draw(&fixture.app, 90, 30));
    assert!(two
        .iter()
        .any(|r| r.contains("General") && r.contains("language")));

    let one = rows(&draw(&fixture.app, 89, 30));
    assert!(one.iter().any(|r| r.contains("General")));
    assert!(!one.iter().any(|r| r.contains("silence timeout")));

    let mut fixture = mockup();
    press(&mut fixture.app, "\n");
    let drilled = rows(&draw(&fixture.app, 60, 18));
    assert!(drilled.iter().any(|r| r.contains("silence timeout")));
    assert!(!drilled.iter().any(|r| r.contains("Backend & keys")));

    for (w, h) in [(59, 30), (90, 17), (52, 16)] {
        let small = rows(&draw(&fixture.app, w, h));
        let text: Vec<&String> = small.iter().filter(|r| !r.is_empty()).collect();
        assert_eq!(text.len(), 1, "{w}x{h}: {small:?}");
        assert_eq!(
            text[0].trim(),
            format!("whisrs needs 60x18, this terminal is {w}x{h}")
        );
    }
}

/// No size panics, from one cell up, on any screen, with a dialog or an
/// input open.
#[test]
fn no_size_panics() {
    let mut states = vec![home(), config(), mockup()];
    let mut dialog = config();
    press(&mut dialog.app, "?");
    states.push(dialog);
    let mut typing = config();
    press(&mut typing.app, "\n");
    states.push(typing);
    let mut list = mockup();
    press(&mut list.app, "jjj\n\n");
    states.push(list);
    states.push(picker());
    states.push(hotkey());
    states.push(hotkeys_with_an_agent());
    states.push(mic());
    for fixture in &states {
        for w in [1, 2, 10, 40, 59, 60, 61, 70, 89, 90, 91, 120, 199, 200, 250] {
            for h in [1, 2, 5, 17, 18, 19, 25, 30, 60, 90] {
                draw(&fixture.app, w, h);
            }
        }
    }
}

/// Built without the recording feature, Recordings says so and reads
/// nothing.
#[cfg(not(feature = "recording"))]
#[test]
fn recordings_say_the_feature_is_missing() {
    use super::app::Effect;
    let mut fixture = mockup();
    assert!(!fixture.app.recordings.built);
    press(&mut fixture.app, "\x1bjj");
    assert!(press(&mut fixture.app, "\n").is_empty(), "nothing to read");
    let drawn = rows(&draw(&fixture.app, 90, 30));
    assert!(drawn
        .iter()
        .any(|r| r.contains("built without the recording feature")));
    assert_eq!(press(&mut fixture.app, "\x1b"), [Effect::StopRecordings]);
}
