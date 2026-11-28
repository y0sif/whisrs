//! Transactional GNOME layout control for uinput. Capability checks finish before
//! any text is sent. After that point errors never replay text through a fallback.

use std::ops::ControlFlow;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::{Typed, XkbKeymap};

pub(crate) trait LayoutControl {
    fn select(&mut self, id: &str) -> Result<()>;
}

struct GnomeControl {
    sources: Value,
}

impl GnomeControl {
    fn call(&self, method: &str, id: Option<&str>) -> Result<Value> {
        let mut command = std::process::Command::new("gdbus");
        command.args([
            "call",
            "--session",
            "--timeout",
            "2",
            "--dest",
            "org.gnome.Shell",
            "--object-path",
            "/org/whisrs/Keyboard",
            "--method",
            &format!("org.whisrs.Keyboard.{method}"),
        ]);
        if let Some(id) = id {
            command.arg(id);
        }
        let output = command
            .output()
            .context("cannot run gdbus layout control")?;
        anyhow::ensure!(
            output.status.success(),
            "GNOME layout control: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        decode_reply(&String::from_utf8(output.stdout)?)
    }
}

// The bridge returns JSON as an array of bytes, avoiding GVariant string
// escaping entirely (source IDs and XKB options can contain quoted strings).
fn decode_reply(reply: &str) -> Result<Value> {
    let bytes = reply
        .trim()
        .strip_prefix("([byte ")
        .and_then(|s| s.strip_suffix("],)"))
        .context("unexpected GNOME layout reply")?
        .split(',')
        .map(|byte| {
            u8::from_str_radix(
                byte.trim()
                    .strip_prefix("0x")
                    .context("invalid reply byte")?,
                16,
            )
            .context("invalid reply byte")
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(serde_json::from_slice(&bytes)?)
}

impl LayoutControl for GnomeControl {
    fn select(&mut self, id: &str) -> Result<()> {
        let state = self.call("Select", Some(id))?;
        anyhow::ensure!(
            state["current"] == id,
            "GNOME did not select requested source"
        );
        anyhow::ensure!(
            state["sources"] == self.sources,
            "GNOME keyboard configuration changed"
        );
        Ok(())
    }
}

pub(crate) struct LayoutSession<C> {
    control: C,
    original: usize,
    pub(crate) maps: Vec<XkbKeymap>,
    ids: Vec<String>,
}

impl LayoutSession<GnomeControl> {
    fn connect() -> Result<Self> {
        anyhow::ensure!(std::env::var_os("WAYLAND_DISPLAY").is_some(), "not Wayland");
        let mut control = GnomeControl {
            sources: Value::Null,
        };
        let state = control.call("Snapshot", None)?;
        let sources = state["sources"]
            .as_array()
            .context("missing GNOME sources")?;
        let mut maps = Vec::new();
        let mut ids = Vec::new();
        for source in sources {
            let string = |key: &str| source[key].as_str().context("missing XKB rule");
            maps.push(XkbKeymap::for_group(
                string("layouts")?,
                string("variants")?,
                string("model")?,
                string("options")?,
                source["group"]
                    .as_u64()
                    .and_then(|n| n.try_into().ok())
                    .context("missing XKB group")?,
            )?);
            ids.push(string("id")?.to_owned());
        }
        let original = ids
            .iter()
            .position(|id| state["current"] == *id)
            .context("unknown active GNOME source")?;
        control.sources = state["sources"].clone();
        Ok(Self {
            control,
            original,
            maps,
            ids,
        })
    }
}

/// Missing capabilities fall back only before typing and after successful restore.
/// A failed restore is an error: typing with an unknown layout is unsafe.
pub(crate) fn prepare() -> Result<Option<LayoutSession<impl LayoutControl>>> {
    let session = match LayoutSession::connect() {
        Ok(session) => session,
        Err(_error) => {
            #[cfg(feature = "logging")]
            log::debug!("layout control unavailable, using legacy input: {_error:#}");
            return Ok(None);
        }
    };
    session.preflight()
}

impl<C: LayoutControl> LayoutSession<C> {
    fn preflight(mut self) -> Result<Option<Self>> {
        let checked = self.ids.iter().try_for_each(|id| self.control.select(id));
        self.control
            .select(&self.ids[self.original])
            .context("failed to restore layout after preflight")?;
        match checked {
            Ok(()) => Ok(Some(self)),
            Err(_error) => {
                #[cfg(feature = "logging")]
                log::debug!("layout preflight failed, original source restored, using legacy input: {_error:#}");
                Ok(None)
            }
        }
    }

    /// Keep the current layout while it can produce the character. Switch only
    /// when necessary; punctuation uses that same layout's mapping. The gate
    /// retains byte offsets in the original string, including clipboard runs.
    pub(crate) fn type_text(
        &mut self,
        text: &str,
        gate: &mut dyn FnMut(usize) -> ControlFlow<()>,
        mut send: impl FnMut(&XkbKeymap, &str) -> Result<()>,
    ) -> Result<Typed> {
        let mut current = self.original;
        let result = (|| {
            let mut chars = text.char_indices().peekable();
            while let Some((at, ch)) = chars.next() {
                let selected = if self.maps[current].lookup(ch).is_some() {
                    Some(current)
                } else {
                    self.maps.iter().position(|map| map.lookup(ch).is_some())
                };
                let mut end = at + ch.len_utf8();
                if selected.is_none() {
                    while let Some(&(next, ch)) = chars.peek() {
                        if self.maps.iter().any(|map| map.lookup(ch).is_some()) {
                            break;
                        }
                        end = next + ch.len_utf8();
                        chars.next();
                    }
                }
                if gate(at).is_break() {
                    return Ok(Typed::StoppedAt(at));
                }
                // Reassert after a potentially blocking gate: the user may have
                // switched sources while typing was paused.
                let target = selected.unwrap_or(current);
                if current != target {
                    std::thread::sleep(Duration::from_millis(20));
                }
                self.control.select(&self.ids[target]).with_context(|| {
                    format!("layout switch failed before byte {at}; prefix already sent, no replay")
                })?;
                current = target;
                send(&self.maps[current], &text[at..end]).with_context(|| format!("injection failed at byte {at}; current character may be partially sent, no replay"))?;
            }
            Ok(Typed::All)
        })();
        // Let the compositor consume the final uinput release before restoring.
        std::thread::sleep(Duration::from_millis(20));
        let restored = self.control.select(&self.ids[self.original]);
        match (result, restored) {
            (Ok(typed), Ok(())) => Ok(typed),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => {
                Err(error
                    .context("text sent or stopped; failed to restore original layout; no replay"))
            }
            (Err(error), Err(restore)) => Err(error.context(format!(
                "also failed to restore original layout: {restore:#}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn byte_array_replies_decode_without_gvariant_string_unescaping() {
        let json = r#"{"current":"us","sources":[{"id":"us+intl","options":"compose:ralt"}]}"#;
        let encoded = json
            .bytes()
            .map(|byte| format!("0x{byte:02x}"))
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(
            decode_reply(&format!("([byte {encoded}],)\n")).unwrap(),
            serde_json::from_str::<Value>(json).unwrap()
        );
        for invalid in [
            "('',)",
            "(@ay [],)",
            "([byte 0xff],)",
            "([byte 0x100],)",
            "([byte nope],)",
        ] {
            assert!(decode_reply(invalid).is_err());
        }
    }

    #[derive(Default)]
    struct State {
        selected: String,
        calls: Vec<String>,
        fail_at: Option<usize>,
    }
    struct Mock(Rc<RefCell<State>>);
    impl LayoutControl for Mock {
        fn select(&mut self, id: &str) -> Result<()> {
            let mut state = self.0.borrow_mut();
            state.calls.push(id.to_owned());
            // Model a switch which took effect but whose acknowledgement failed.
            state.selected = id.to_owned();
            anyhow::ensure!(
                state.fail_at != Some(state.calls.len()),
                "lost switch acknowledgement"
            );
            Ok(())
        }
    }

    fn session(layouts: &str, original: usize) -> (LayoutSession<Mock>, Rc<RefCell<State>>) {
        let ids: Vec<_> = layouts.split(',').map(str::to_owned).collect();
        let state = Rc::new(RefCell::new(State {
            selected: ids[original].clone(),
            ..State::default()
        }));
        let maps = ids
            .iter()
            .enumerate()
            .map(|(group, _)| XkbKeymap::for_group(layouts, "", "", "", group as u32).unwrap())
            .collect();
        (
            LayoutSession {
                control: Mock(state.clone()),
                original,
                maps,
                ids,
            },
            state,
        )
    }

    fn produced(layouts: &str, group: u32, mapping: crate::KeyMapping) -> char {
        use xkbcommon::xkb;
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "evdev",
            "",
            layouts,
            "",
            Some(String::new()),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        let mut state = xkb::State::new(&keymap);
        state.update_mask(0, 0, 0, 0, 0, group);
        assert!(mapping.follow.is_none());
        if mapping.main.shift {
            state.update_key(
                xkb::Keycode::new(u32::from(crate::Key::KEY_LEFTSHIFT.code()) + 8),
                xkb::KeyDirection::Down,
            );
        }
        if mapping.main.altgr {
            state.update_key(
                xkb::Keycode::new(u32::from(crate::Key::KEY_RIGHTALT.code()) + 8),
                xkb::KeyDirection::Down,
            );
        }
        char::from_u32(state.key_get_utf32(xkb::Keycode::new(u32::from(mapping.main.keycode) + 8)))
            .unwrap()
    }

    #[test]
    fn mixed_text_and_punctuation_use_only_the_selected_group() {
        for (layouts, text) in [
            ("us,ru,fr", "Сегодня нужно проверить pull request и сделать deploy на production. Да, всё! Bonjour, été."),
            ("us,gr,de", "Hello, κοσμε. Grüße, Welt!"),
        ] {
            for original in 0..3 {
                let (session, state) = session(layouts, original);
                let mut session = session.preflight().unwrap().unwrap();
                let mut sent = String::new();
                let mut offsets = Vec::new();
                let typed = session.type_text(text, &mut |at| { offsets.push(at); ControlFlow::Continue(()) }, |map, fragment| {
                    let selected = state.borrow().selected.clone();
                    let group = session_group(layouts, &selected);
                    for ch in fragment.chars() {
                        let mapping = *map.lookup(ch).unwrap_or_else(|| panic!("missing {ch:?} on {selected}"));
                        assert_eq!(produced(layouts, group, mapping), ch, "layout {selected}");
                    }
                    sent.push_str(fragment);
                    Ok(())
                }).unwrap();
                assert_eq!(typed, Typed::All);
                assert_eq!(sent, text);
                assert_eq!(offsets, text.char_indices().map(|(at, _)| at).collect::<Vec<_>>());
                assert_eq!(state.borrow().selected, layouts.split(',').nth(original).unwrap());
            }
        }
    }

    fn session_group(layouts: &str, selected: &str) -> u32 {
        layouts.split(',').position(|id| id == selected).unwrap() as u32
    }

    #[test]
    fn failed_preflight_restores_before_allowing_legacy_fallback() {
        for fail_at in 1..=3 {
            let (session, state) = session("us,ru,fr", 1);
            state.borrow_mut().fail_at = Some(fail_at);
            assert!(session.preflight().unwrap().is_none());
            assert_eq!(state.borrow().selected, "ru");
        }
        let (session, state) = session("us,ru", 0);
        state.borrow_mut().fail_at = Some(3); // restoration fails
        assert!(session.preflight().is_err());
    }

    #[test]
    fn stops_and_errors_restore_without_replaying_the_prefix() {
        for failure in ["stop", "send", "switch", "restore"] {
            let (mut session, state) = session("us,ru", 0);
            let mut sent = String::new();
            if failure == "switch" {
                state.borrow_mut().fail_at = Some(2);
            }
            if failure == "restore" {
                state.borrow_mut().fail_at = Some(4);
            }
            let result = session.type_text(
                "aя.",
                &mut |at| {
                    if failure == "stop" && at == 3 {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                },
                |_, fragment| {
                    if failure == "send" && fragment == "я" {
                        anyhow::bail!("device disconnected");
                    }
                    sent.push_str(fragment);
                    Ok(())
                },
            );
            match failure {
                "stop" => {
                    assert_eq!(result.unwrap(), Typed::StoppedAt(3));
                    assert_eq!(sent, "aя");
                }
                "restore" => {
                    assert!(result.is_err());
                    assert_eq!(sent, "aя.");
                }
                _ => {
                    assert!(result.is_err());
                    assert_eq!(sent, "a");
                }
            }
            assert_eq!(state.borrow().selected, "us");
            assert_eq!(state.borrow().calls.last().unwrap(), "us");
        }
    }

    #[test]
    fn missing_characters_keep_clipboard_runs_and_gate_offsets() {
        let (mut session, state) = session("us,ru", 1);
        let mut sent = Vec::new();
        let mut offsets = Vec::new();
        let typed = session
            .type_text(
                "я😀🎉a",
                &mut |at| {
                    offsets.push(at);
                    ControlFlow::Continue(())
                },
                |_, text| {
                    sent.push(text.to_owned());
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(typed, Typed::All);
        assert_eq!(sent, ["я", "😀🎉", "a"]);
        assert_eq!(offsets, [0, 2, 10]);
        assert_eq!(state.borrow().selected, "ru");
    }
}
