use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use whisrs::llm;
use whisrs::InjectorBackend;
use xkb_type::ClipboardBackend;

static KEYBOARD: OnceLock<StdMutex<Option<Box<dyn xkb_type::KeyInjector>>>> = OnceLock::new();

/// Delay before the post-paste clipboard restore: long enough for the paste
/// keystroke to land in the target app, short enough that the user's
/// clipboard contents are back before they next reach for them.
const CLIPBOARD_RESTORE_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// What may be done with an LLM reply that is headed for the cursor, decided
/// by [`prepare_llm_injection`].
///
/// Three-way rather than a `String`, because two of the outcomes must not
/// reach the injector and the caller has to tell the user which one happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LlmInjection {
    /// Cleaned text, safe to inject at this target. Under `clipboard_only`
    /// there is no target (the text is only copied), so a multi-line reply
    /// with a terminal focused lands here too.
    Inject(String),
    /// Nothing usable came back: an empty reply, an all-whitespace one, or a
    /// code fence with nothing in it.
    Empty,
    /// Multi-line text aimed at a terminal. Never injected — see
    /// [`prepare_llm_injection`]. Carries the cleaned text so the caller can
    /// put it in the history log instead of dropping it.
    RefusedMultiLine(String),
}

/// Clean an LLM reply and decide whether it may be injected at the current
/// target. Shared by `whisrs command` and `[[llm_commands]]`.
///
/// Two rules, and only one of them is absolute:
///
/// * **Cleaning is unconditional.** [`llm::clean_llm_output`] normalizes line
///   endings and strips a code fence wrapping the whole reply, everywhere. A
///   fenced reply is never what the user wanted typed.
/// * **The multi-line refusal is conditional on the target.** A line break is
///   only dangerous where it *submits*: at a shell prompt it is an Enter that
///   runs a command the user has not read. Everywhere else a multi-line reply
///   is the normal, wanted result — translating a paragraph, drafting an
///   email, reformatting a list — so refusing it outright would break the
///   feature it was meant to protect. Hence `is_terminal`, resolved by the
///   caller from the focused window class.
///
/// Truncating to the first line was considered and rejected for the terminal
/// case: `cd /tmp` out of `cd /tmp` + `rm -rf x` is a different, still
/// destructive command. Refusing costs one retry and loses nothing — the text
/// comes back in [`LlmInjection::RefusedMultiLine`] for the history log, where
/// `whisrs log` recovers it.
///
/// **`[input] paste` is deliberately not a parameter.** The refusal fires the
/// same way under `paste = true`, where the hazard is weaker: that path sends
/// Ctrl+Shift+V, and a terminal with bracketed paste enabled inserts the whole
/// multi-line text literally instead of running each line. Weaker is not
/// absent. Bracketed paste is the *foreground program's* choice, not the
/// terminal's — it is off inside plenty of TUI programs and in some readline
/// modes — so from here, with only the window class to go on, we cannot know
/// whether it is on at the moment the keystroke lands. The trade is asymmetric:
/// refusing wrongly costs one `whisrs log` lookup, injecting wrongly runs
/// commands the user never read. So the gate stays target-shaped, not
/// injection-method-shaped, and this function keeps a signature that cannot
/// express the weaker rule.
///
/// **`[input] clipboard_only` is a parameter, for the opposite reason.** It
/// does not weaken the hazard, it removes it: in that mode [`inject_text`]
/// writes the text to the clipboard and returns, so not one keystroke reaches
/// the focused window and a line break has nothing to submit. That is not an
/// injection method, it is the absence of one — the gate is still
/// target-shaped, and `clipboard_only` means there is no target. Refusing
/// there would withhold the reply from the clipboard, the one place the user
/// asked for it, to guard against a keystroke that is never sent (#121). The
/// caller must pass the same value it hands to [`inject_text`]: a multi-line
/// [`LlmInjection::Inject`] decided under `clipboard_only` is only safe
/// because it will be copied, not typed. Cleaning and
/// [`LlmInjection::Empty`] do not depend on it.
pub(crate) fn prepare_llm_injection(
    raw: &str,
    is_terminal: bool,
    clipboard_only: bool,
) -> LlmInjection {
    // Sanitized here as well as in `inject_text`, so the verdict and the
    // history entry see exactly the text that would be typed.
    let cleaned = sanitize_for_injection(&llm::clean_llm_output(raw)).into_owned();
    if cleaned.is_empty() {
        return LlmInjection::Empty;
    }
    if is_terminal && !clipboard_only && llm::contains_line_break(&cleaned) {
        return LlmInjection::RefusedMultiLine(cleaned);
    }
    LlmInjection::Inject(cleaned)
}

/// Make text safe to hand to the injector, whatever produced it: a
/// transcription backend, an LLM, or a remote endpoint we do not control.
///
/// Every control character other than `\n` and `\t` is removed. Both injector
/// backends tap a control character as the key it names, not as text: `\x1b`
/// is Escape, `\x08` BackSpace, `\x7f` Delete, so a reply carrying
/// `\x1b` + `ZZ` saves and quits a vim session without a single Enter. In paste
/// mode the same bytes travel through the clipboard, where `\x1b[201~` ends a
/// terminal's bracketed paste early and turns the rest into keystrokes.
///
/// Line endings are kept but normalized: CRLF, bare CR, vertical tab, form
/// feed and NEL all become `\n`, so a caller deciding whether a line break may
/// be typed (see [`fold_line_breaks`] and [`prepare_llm_injection`]) only has
/// `\n` and the two Unicode separators to look for.
pub(crate) fn sanitize_for_injection(text: &str) -> std::borrow::Cow<'_, str> {
    if !text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\u{000b}' | '\u{000c}' | '\u{0085}' => out.push('\n'),
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Replace every run of line breaks with a single space, for dictation headed
/// to a window where an Enter could submit something (see
/// [`line_breaks_unsafe_at`]).
///
/// Dictation folds rather than refuses, unlike the LLM gate in
/// [`prepare_llm_injection`]: no spoken phrase produces a line break, so one in
/// a transcript is either backend noise or something injected on purpose, and
/// a space loses nothing the user said. Expects [`sanitize_for_injection`] to
/// have run first.
pub(crate) fn fold_line_breaks(text: &str) -> std::borrow::Cow<'_, str> {
    if !llm::contains_line_break(text) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut in_break = false;
    for c in text.chars() {
        if llm::is_line_break(c) {
            if !in_break {
                out.push(' ');
            }
            in_break = true;
        } else {
            out.push(c);
            in_break = false;
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Whether a line break typed at the focused window could act as an Enter
/// that submits something: a known terminal, or a window whose class the
/// tracker could not report.
///
/// `class` is `WindowTracker::get_focused_window_class()`. It is `None` on
/// GNOME and KDE for every window (#72, #127), and on the other desktops when
/// the query fails. Treating that as "not a terminal" made every line-break
/// guard a no-op on those two desktops, so unknown counts as a terminal unless
/// `[input] unknown_window_is_terminal = false`.
///
/// Only the line-break guards use this. The paste combo, the selection copy
/// and command mode's line clear still key off [`is_terminal_class`] alone:
/// a wrong guess there sends Ctrl+A / Ctrl+K into a GUI text field (#70),
/// while a wrong guess here only withholds or folds a line break.
pub(crate) fn line_breaks_unsafe_at(class: Option<&str>, input: &whisrs::InputConfig) -> bool {
    match class.map(str::trim).filter(|c| !c.is_empty()) {
        Some(c) => is_terminal_class(c, &input.terminal_classes),
        None => input.unknown_window_is_terminal,
    }
}

/// The `[input]` settings every synthetic keystroke needs, bundled because
/// they always travel together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeystrokeSettings {
    /// `[input] key_delay_ms`.
    pub(crate) key_delay: std::time::Duration,
    /// `[input] backend`.
    pub(crate) backend: InjectorBackend,
    /// `[input] modifier_wait_ms`: the cap on waiting for held physical
    /// modifiers before a batch or command-mode keystroke (#154). Zero means
    /// don't wait: a held modifier sends the text straight to the clipboard.
    /// Streaming ignores it and always waits for the release (see
    /// [`deliver_streaming_delta`]).
    pub(crate) modifier_wait: std::time::Duration,
}

impl KeystrokeSettings {
    pub(crate) fn from_config(input: &whisrs::InputConfig) -> Self {
        Self {
            key_delay: std::time::Duration::from_millis(input.key_delay_ms),
            backend: input.backend,
            modifier_wait: std::time::Duration::from_millis(input.modifier_wait_ms),
        }
    }
}

/// What a keystroke request did.
///
/// A held modifier is not an error: the keys were deliberately withheld, and
/// the caller owes the text a clipboard copy instead (#154). Hence an `Ok`
/// variant the caller must match, not an `Err` it could log and drop.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeystrokeOutcome {
    /// The keystrokes were sent.
    Sent,
    /// A physical modifier was still held when
    /// [`KeystrokeSettings::modifier_wait`] ran out. Nothing was sent.
    ModifierHeld,
}

/// Where a batch injection's text ended up.
///
/// The held-modifier copy (#154) is a success for the text, but the user
/// expects it at the cursor, so the caller (which holds the notify config)
/// owes them a toast; the injection code runs in `spawn_blocking` without it.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Injection {
    /// Typed, pasted, or (under `clipboard_only`) copied as configured.
    Delivered,
    /// A physical modifier was still held after `[input] modifier_wait_ms`:
    /// no keys were sent and the text was copied to the clipboard instead.
    CopiedForHeldModifier,
}

/// Toast summary for [`Injection::CopiedForHeldModifier`].
pub(crate) const HELD_MODIFIER_TOAST: &str = "Modifier key held: text copied to clipboard";

/// Tell the user their text went to the clipboard, not the cursor (#154).
/// The caller gates this on `notify_error()`: the `warn!` alone only reaches
/// the journal, and a dictation that silently did not type looks lost.
pub(crate) fn notify_copied_for_held_modifier(text: &str) {
    crate::notify::send_notification(
        HELD_MODIFIER_TOAST,
        &crate::notify::truncate_preview(text, 77),
    );
}

/// Batch typing at the cursor via the persistent virtual keyboard,
/// optionally clearing the prompt line first (see
/// [`clear_line_best_effort`]). Sends nothing while a physical modifier is
/// held past `keys.modifier_wait`; see [`KeystrokeOutcome`]. The clear and
/// the typing share one modifier wait and one hold of [`KEYBOARD`], so a
/// modifier pressed in between cannot leave a cleared line with nothing
/// typed on it.
fn type_text_with_clear(
    text: &str,
    keys: KeystrokeSettings,
    clear_line: bool,
) -> Result<KeystrokeOutcome> {
    with_persistent_keyboard(keys, capped_wait(keys), |keyboard| {
        if clear_line {
            clear_line_best_effort(keyboard);
        }
        keyboard.type_text(text).context("failed to type text")
    })
    .map(batch_outcome)
}

/// The batch modifier wait: up to `[input] modifier_wait_ms`, no cancel.
fn capped_wait(keys: KeystrokeSettings) -> impl FnOnce() -> ModifierWait {
    move || wait_for_physical_modifier_release(keys.modifier_wait)
}

/// A batch keystroke's [`KeystrokeOutcome`] from how its modifier wait
/// ended. The batch wait has no cancel, so anything but a release (or no
/// hold at all) means the cap ran out.
fn batch_outcome(wait: ModifierWait) -> KeystrokeOutcome {
    if wait.lets_keys_through() {
        KeystrokeOutcome::Sent
    } else {
        KeystrokeOutcome::ModifierHeld
    }
}

/// Put `text` on the clipboard because a held modifier stopped it from being
/// typed or pasted (#154). A failed copy is an error: the copy is the only
/// place the text went.
fn copy_instead_of_keystrokes(
    clipboard: &dyn ClipboardBackend,
    text: &str,
    modifier_wait: std::time::Duration,
) -> Result<Injection> {
    clipboard
        .set_text(text)
        .context("a modifier key was held, and copying the text to the clipboard instead failed")?;
    warn_copied_instead(modifier_wait, text.len());
    Ok(Injection::CopiedForHeldModifier)
}

/// The one warning for "not typed, copied instead" (#154).
fn warn_copied_instead(modifier_wait: std::time::Duration, len: usize) {
    warn!(
        "a modifier key was still held after {} ms; copied {len} chars to the \
         clipboard instead of typing them",
        modifier_wait.as_millis()
    );
}

/// What happened to one streaming delta (#154).
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamingDelivery {
    /// The delta was typed at the cursor.
    Typed,
    /// The session was cancelled while the delta waited for a held modifier
    /// to be released. Nothing was typed or copied: the text is discarded.
    Cancelled,
}

/// Streaming dictation's delivery of one typed delta (#154).
///
/// Unlike the batch path, streaming never gives up on a held modifier and
/// never switches to the clipboard: it waits for every physical modifier to
/// be released, however long that takes, then types. Deltas that arrive
/// meanwhile queue in the typing batcher and are typed in order afterwards.
/// `[input] modifier_wait_ms` does not apply here. The only way out of the
/// wait is `cancel` (`whisrs cancel`), which discards the delta.
///
/// `tracker` records the time spent waiting, so the pipeline's drain timeout
/// after stop does not count it (see [`ModifierWaitTracker`]).
///
/// The delta is sanitized first, and with `fold_breaks` (see
/// [`line_breaks_unsafe_at`]) its line breaks become spaces, in that order so
/// a bare `\r` is folded too rather than typed as Return.
pub(crate) fn deliver_streaming_delta(
    delta: &str,
    fold_breaks: bool,
    keys: KeystrokeSettings,
    cancel: &AtomicBool,
    tracker: &ModifierWaitTracker,
) -> Result<StreamingDelivery> {
    let sanitized = sanitize_for_injection(delta);
    let delta = if fold_breaks {
        fold_line_breaks(&sanitized).into_owned()
    } else {
        sanitized.into_owned()
    };
    let delta = delta.as_str();
    let wait = with_persistent_keyboard(
        keys,
        || wait_for_physical_modifier_release_until_cancelled(cancel, tracker),
        |keyboard| keyboard.type_text(delta).context("failed to type text"),
    )?;
    Ok(if wait.lets_keys_through() {
        StreamingDelivery::Typed
    } else {
        StreamingDelivery::Cancelled
    })
}

/// Time streaming deliveries spend waiting on a held modifier (#154), shared
/// with the pipeline's drain after stop. The drain timeout guards against a
/// stuck typing task, not a user holding Alt: while a delta waits, the drain
/// must not abort it, and once the wait ends the drain gets its full budget
/// again.
#[derive(Debug, Default)]
pub(crate) struct ModifierWaitTracker {
    state: StdMutex<ModifierWaitState>,
}

#[derive(Debug, Default, Clone, Copy)]
struct ModifierWaitState {
    /// A delta is waiting on a held modifier right now.
    waiting: bool,
    /// When the last such wait ended.
    last_end: Option<std::time::Instant>,
}

impl ModifierWaitTracker {
    fn update(&self, f: impl FnOnce(&mut ModifierWaitState)) {
        f(&mut self.state.lock().unwrap_or_else(|e| e.into_inner()));
    }

    fn begin(&self) {
        self.update(|s| s.waiting = true);
    }

    fn end(&self) {
        self.update(|s| {
            s.waiting = false;
            s.last_end = Some(std::time::Instant::now());
        });
    }

    /// When the drain that started at `drain_start` may give up on the
    /// typing task: `budget` after the later of `drain_start` and the end of
    /// the last modifier wait. `None` while a delta is still waiting: no
    /// deadline at all until the modifier is released (or the session is
    /// cancelled).
    pub(crate) fn drain_deadline(
        &self,
        drain_start: std::time::Instant,
        budget: std::time::Duration,
    ) -> Option<std::time::Instant> {
        let state = *self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.waiting {
            return None;
        }
        let from = state
            .last_end
            .map_or(drain_start, |end| end.max(drain_start));
        Some(from + budget)
    }
}

/// Send a paste keystroke — Ctrl+V, or Ctrl+Shift+V in terminals — via the
/// **persistent** virtual keyboard (the same device typing uses).
///
/// Must NOT use a fresh per-call uinput device: on some compositors (e.g.
/// KWin) keystrokes from a device the compositor hasn't finished enumerating
/// are dropped, so the paste silently no-ops. The persistent device is already
/// recognized, so its keystrokes land. The combo is raw keycodes (`KEY_V` is
/// `v` in every common layout), so it stays layout-independent.
///
/// With `clear_line`, the prompt line is cleared first under the same
/// modifier wait and lock hold, as in [`type_text_with_clear`].
fn paste_via_keyboard(
    is_terminal: bool,
    keys: KeystrokeSettings,
    clear_line: bool,
) -> Result<KeystrokeOutcome> {
    use evdev::Key;

    let combo: &[Key] = if is_terminal {
        &[Key::KEY_LEFTCTRL, Key::KEY_LEFTSHIFT, Key::KEY_V]
    } else {
        &[Key::KEY_LEFTCTRL, Key::KEY_V]
    };

    with_persistent_keyboard(keys, capped_wait(keys), |keyboard| {
        if clear_line {
            clear_line_best_effort(keyboard);
        }
        keyboard
            .send_combo(combo)
            .context("failed to send paste combo")
    })
    .map(batch_outcome)
}

/// Clear the current shell prompt line by sending Ctrl+A ("move to start of
/// line") then Ctrl+K ("kill to end of line") on `keyboard`, the
/// **persistent** virtual keyboard, from inside the same
/// [`with_persistent_keyboard`] call that then types or pastes the text. That
/// readline / zle / fish editing pair empties the line in bash, zsh and fish
/// alike.
///
/// Only ever called for terminals. A terminal's mouse highlight is a visual
/// overlay, not an editable selection, so injecting at the cursor would append
/// to the existing line rather than replace it. Clearing the line first makes
/// the injected text the whole line, which is what a "rewrite my selection"
/// command means at a prompt. In GUI text widgets no clear is needed (or
/// wanted): typing/pasting over a real selection replaces it natively.
///
/// Must NOT use a fresh per-call uinput device: on some compositors (e.g.
/// KWin) keystrokes from a device the compositor hasn't finished enumerating
/// are dropped, so the clear silently no-ops. The persistent device is already
/// recognized, so its keystrokes land.
///
/// Best-effort: a failed clear is logged and the text is injected anyway,
/// because appending beats losing it.
fn clear_line_best_effort(keyboard: &mut dyn xkb_type::KeyInjector) {
    use evdev::Key;

    let result = keyboard
        .send_combo(&[Key::KEY_LEFTCTRL, Key::KEY_A])
        .context("failed to send Ctrl+A (move to start of line)")
        .and_then(|()| {
            keyboard
                .send_combo(&[Key::KEY_LEFTCTRL, Key::KEY_K])
                .context("failed to send Ctrl+K (kill to end of line)")
        });
    if let Err(e) = result {
        warn!("command mode: failed to clear terminal line, injecting anyway: {e:#}");
    }
}

/// Run `send` against the persistent virtual keyboard, creating it on first
/// use, once `wait_for_release` says no physical modifier is held.
///
/// The modifier wait (#154) is what keeps a `Super+W` stop hotkey from
/// turning the injected keys into compositor binds: the final text is ready
/// while the user's finger is often still on Super. No key is ever sent while
/// a modifier is held: unless the wait ends in a release (or found nothing
/// held), `send` is not called. Returns how the wait ended; `send` ran iff
/// [`ModifierWait::lets_keys_through`]. The wait runs inside the lock, so a
/// second injection queues behind it instead of slipping past. On a failed
/// send the device is dropped so the next call rebuilds it.
fn with_persistent_keyboard(
    keys: KeystrokeSettings,
    wait_for_release: impl FnOnce() -> ModifierWait,
    send: impl FnOnce(&mut dyn xkb_type::KeyInjector) -> Result<()>,
) -> Result<ModifierWait> {
    let KeystrokeSettings {
        key_delay, backend, ..
    } = keys;
    let keyboard_slot = KEYBOARD.get_or_init(|| StdMutex::new(None));
    let mut keyboard_guard = keyboard_slot
        .lock()
        .map_err(|_| anyhow::anyhow!("keyboard mutex poisoned"))?;

    if keyboard_guard.is_none() {
        // Runtime path: short settle delay. The startup prewarm in
        // `warm_keyboard` is what gives X11 time to attach its keymap;
        // we don't want every error-recovery to stall the user's typing
        // path for 1s.
        *keyboard_guard = Some(new_keyboard(
            key_delay, /* prewarm = */ false, backend,
        )?);
    }

    let wait = wait_for_release();
    if !wait.lets_keys_through() {
        return Ok(wait);
    }

    let keyboard = keyboard_guard
        .as_mut()
        .expect("keyboard exists after initialization");
    keyboard.set_key_delay(key_delay);

    let result = send(keyboard.as_mut());
    if result.is_err() {
        *keyboard_guard = None;
    }
    result.map(|()| wait)
}

/// Poll interval for [`wait_for_modifier_release`].
const MODIFIER_RELEASE_POLL: std::time::Duration = std::time::Duration::from_millis(15);

/// Pause after a held modifier is released, before the first injected key,
/// so the compositor has processed the queued physical release by then.
/// Only paid when a modifier was actually seen held.
const MODIFIER_RELEASE_SETTLE: std::time::Duration = std::time::Duration::from_millis(25);

/// Names of our own uinput devices: the persistent keyboard
/// (`crates/xkb-type/src/keyboard.rs`) and the temporary one the selection
/// copy builds (`selection.rs`). Both are skipped by the probe: their
/// modifiers are the ones we press.
#[cfg(not(test))]
const OWN_DEVICE_NAMES: [&str; 2] = ["whisrs virtual keyboard", "whisrs command"];

/// Block until no physical keyboard holds a modifier, up to `cap`
/// (`[input] modifier_wait_ms`). The batch and command-mode wait: call before
/// sending any synthetic keystroke (#154), and send nothing on
/// [`ModifierWait::TimedOut`]. A zero `cap` means "don't wait": one reading,
/// and a held modifier is an immediate [`ModifierWait::TimedOut`]. Takes only
/// the keyboard-cache lock, never [`KEYBOARD`], so callers may hold
/// [`KEYBOARD`] around it.
pub(crate) fn wait_for_physical_modifier_release(cap: std::time::Duration) -> ModifierWait {
    wait_for_modifier_release(
        modifier_probe(),
        Some(cap),
        MODIFIER_RELEASE_POLL,
        || false,
        || {},
    )
}

/// The streaming wait (#154): block until no physical keyboard holds a
/// modifier, with no cap. `cancel` is checked on every poll, and a set flag
/// ends the wait with [`ModifierWait::Cancelled`]. A wait that actually
/// blocks is recorded in `tracker`.
fn wait_for_physical_modifier_release_until_cancelled(
    cancel: &AtomicBool,
    tracker: &ModifierWaitTracker,
) -> ModifierWait {
    let wait = wait_for_modifier_release(
        modifier_probe(),
        None,
        MODIFIER_RELEASE_POLL,
        || cancel.load(Ordering::SeqCst),
        || tracker.begin(),
    );
    if wait != ModifierWait::NotHeld {
        tracker.end();
    }
    wait
}

/// How [`wait_for_modifier_release`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModifierWait {
    /// Nothing was held; no wait at all.
    NotHeld,
    /// A modifier was held and released after this long.
    Released(std::time::Duration),
    /// A modifier was still held at the timeout.
    TimedOut,
    /// The wait was cancelled while a modifier was still held.
    Cancelled,
}

impl ModifierWait {
    /// Whether keys may be sent: nothing was held, or it was released.
    pub(crate) fn lets_keys_through(self) -> bool {
        matches!(self, Self::NotHeld | Self::Released(_))
    }
}

/// Block until `modifier_held` reports false, polling every `interval` for
/// at most `timeout` (`None`: no limit). Returns at once, without sleeping,
/// when nothing is held. Otherwise `on_hold` runs once, and `cancelled` is
/// checked before every poll. After a seen release it sleeps
/// [`MODIFIER_RELEASE_SETTLE`] more. The caller decides what a timeout or a
/// cancel means; it must not send keys.
fn wait_for_modifier_release(
    mut modifier_held: impl FnMut() -> bool,
    timeout: Option<std::time::Duration>,
    interval: std::time::Duration,
    mut cancelled: impl FnMut() -> bool,
    on_hold: impl FnOnce(),
) -> ModifierWait {
    if !modifier_held() {
        return ModifierWait::NotHeld;
    }
    on_hold();
    let start = std::time::Instant::now();
    loop {
        if cancelled() {
            debug!("modifier wait cancelled while a modifier key was held; sending no keys");
            return ModifierWait::Cancelled;
        }
        let elapsed = start.elapsed();
        let mut nap = interval;
        if let Some(timeout) = timeout {
            if elapsed >= timeout {
                debug!("a modifier key is still held after {timeout:?}; sending no keys");
                return ModifierWait::TimedOut;
            }
            nap = nap.min(timeout - elapsed);
        }
        std::thread::sleep(nap);
        if !modifier_held() {
            let waited = start.elapsed();
            debug!("waited {waited:?} for held modifiers to release before injecting");
            std::thread::sleep(MODIFIER_RELEASE_SETTLE);
            return ModifierWait::Released(waited);
        }
    }
}

/// Keyboards the modifier probe reads, cached across waits (#154): opening
/// every `/dev/input` node per injection costs 100+ ms, reading a cached
/// device's key state costs microseconds.
#[cfg(not(test))]
static KEYBOARD_CACHE: OnceLock<StdMutex<whisrs::hotkey::KeyboardCache>> = OnceLock::new();

#[cfg(not(test))]
fn keyboard_cache() -> &'static StdMutex<whisrs::hotkey::KeyboardCache> {
    KEYBOARD_CACHE
        .get_or_init(|| StdMutex::new(whisrs::hotkey::KeyboardCache::new(&OWN_DEVICE_NAMES)))
}

/// Sync [`KEYBOARD_CACHE`] with `/dev/input`, opening only new nodes.
#[cfg(not(test))]
fn refresh_keyboard_cache(cache: &mut whisrs::hotkey::KeyboardCache) {
    if let Err(e) = cache.refresh() {
        debug!("cannot list keyboards for modifier check: {e:#}");
    }
}

/// Fill the modifier probe's keyboard cache at startup, so the first
/// injection does not pay for opening every input device. Blocking; run it
/// off the async runtime.
#[cfg(not(test))]
pub(crate) fn prime_modifier_probe() {
    let Ok(mut cache) = keyboard_cache().lock() else {
        return;
    };
    let start = std::time::Instant::now();
    refresh_keyboard_cache(&mut cache);
    debug!(
        "modifier probe: {} keyboard(s) cached in {:?}",
        cache.keyboard_count(),
        start.elapsed()
    );
}

/// Test build: never touch the real `/dev/input`.
#[cfg(test)]
pub(crate) fn prime_modifier_probe() {}

/// The modifier probe for [`wait_for_modifier_release`]: true while any
/// physical keyboard reports a modifier down (`EVIOCGKEY`).
///
/// The keyboard cache is synced once per wait (new nodes opened, gone ones
/// dropped), not per poll. A device that cannot be opened or read counts as
/// not held, so without `/dev/input` access this degrades to no wait rather
/// than blocking.
#[cfg(not(test))]
fn modifier_probe() -> impl FnMut() -> bool {
    if let Ok(mut cache) = keyboard_cache().lock() {
        refresh_keyboard_cache(&mut cache);
    }
    || {
        keyboard_cache()
            .lock()
            .is_ok_and(|mut cache| cache.any_modifier_held())
    }
}

#[cfg(test)]
thread_local! {
    /// Test seam for [`modifier_probe`]: tests must never read the real
    /// `/dev/input`. Unset means "nothing held".
    static MODIFIER_PROBE: std::cell::RefCell<Option<Box<dyn FnMut() -> bool>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `f` with `probe` as this thread's modifier probe, then unset it.
#[cfg(test)]
pub(crate) fn with_modifier_probe(probe: impl FnMut() -> bool + 'static, f: impl FnOnce()) {
    MODIFIER_PROBE.with(|p| *p.borrow_mut() = Some(Box::new(probe)));
    f();
    MODIFIER_PROBE.with(|p| *p.borrow_mut() = None);
}

#[cfg(test)]
fn modifier_probe() -> impl FnMut() -> bool {
    || MODIFIER_PROBE.with(|probe| probe.borrow_mut().as_mut().is_some_and(|f| f()))
}

/// Inject `text` at the cursor, choosing keystrokes or clipboard paste.
///
/// With `paste = false` (default) this types via the virtual keyboard. With
/// `paste = true` it sets the clipboard, sends Ctrl+V (Ctrl+Shift+V for
/// terminals), then restores the previous clipboard — layout-independent
/// injection for compositors that lack the Wayland virtual-keyboard protocol
/// (see [`whisrs::InputConfig::paste`]). Runs in a blocking context (callers
/// wrap it in `spawn_blocking`), so the sleeps/restore use std threads.
///
/// `ClipboardBackend` is text-only, so a clipboard holding non-text content
/// (an image, a file list, ...) can't be captured and round-tripped at all.
/// If the pre-paste read fails, this falls back to typing instead of pasting
/// — overwriting the clipboard via `set_text` first and only then discovering
/// there's nothing valid to restore would destroy that content permanently
/// (see #69), which skipping the *restore* alone can't undo since the damage
/// already happened at `set_text`.
///
/// The restore is otherwise skipped, rather than clobbering the clipboard,
/// when the clipboard no longer holds the text we set — something else (the
/// user, another app) copied over it during the paste, and restoring would
/// race that copy and discard it.
///
/// With `clipboard_fallback` (see
/// [`whisrs::InputConfig::clipboard_fallback`]) the final text is left in
/// the clipboard as a manual-fix fallback for silent injection failures:
/// in typing mode it is copied after the keystrokes run, success or failure
/// (a `warn!` on copy error, never a change to the returned `Result`); in
/// paste mode the post-paste restore is skipped entirely, so the pasted text
/// simply stays in the clipboard. The unreadable-clipboard fallback to
/// typing above still applies in paste mode: it exists to protect non-text
/// content, and typing already delivered the text, so no clipboard write
/// happens on that degraded path.
///
/// With `clipboard_only` (see
/// [`whisrs::InputConfig::clipboard_only`]) the text is never injected at
/// all: it is written to the clipboard and the function returns. That mode
/// wins over `paste` and `clipboard_fallback` (both become no-ops), and a
/// copy failure is a hard error — the copy is the entire feature, there is
/// no injection to fall back to.
///
/// No keystroke is sent while a physical modifier is held (#154). If one is
/// still down after `keys.modifier_wait`, the text is copied to the clipboard
/// instead of typed or pasted, with a `warn!`, and the result is
/// [`Injection::CopiedForHeldModifier`] so the caller can notify. That copy is
/// a hard error on failure, like `clipboard_only`: it is the only place the
/// text went.
pub(crate) fn inject_text(
    text: &str,
    is_terminal: bool,
    keys: KeystrokeSettings,
    paste: bool,
    clipboard_fallback: bool,
    clipboard_only: bool,
) -> Result<Injection> {
    inject_text_with_clipboard(
        text,
        is_terminal,
        keys,
        paste,
        clipboard_fallback,
        clipboard_only,
        /* clear_line = */ false,
        CLIPBOARD_RESTORE_DELAY,
        Arc::from(xkb_type::default_clipboard()),
    )
}

/// Testable core of [`inject_text`]: the clipboard and the restore delay are
/// injected so unit tests can pin the clipboard-fallback and restore policy
/// without a real clipboard (or a 500 ms wait). `clipboard` is an `Arc`
/// because the post-paste restore runs on a spawned thread, which needs
/// `'static` access; the production entry point wraps
/// `xkb_type::default_clipboard()`. `clear_line` is command mode's terminal
/// line clear, sent under the same modifier wait as the text (see
/// [`clear_line_and_inject`]).
#[allow(clippy::too_many_arguments)]
fn inject_text_with_clipboard(
    text: &str,
    is_terminal: bool,
    keys: KeystrokeSettings,
    paste: bool,
    clipboard_fallback: bool,
    clipboard_only: bool,
    clear_line: bool,
    restore_delay: std::time::Duration,
    clipboard: Arc<dyn ClipboardBackend>,
) -> Result<Injection> {
    // Every mode, `clipboard_only` included: the clipboard text is pasted
    // somewhere eventually, and an Escape in it is as live there as here.
    let text = &*sanitize_for_injection(text);
    if clipboard_only {
        // The clipboard is the output, not the transport: nothing is
        // injected at the cursor, and a copy failure is a hard error —
        // there is no injection to fall back to.
        clipboard
            .set_text(text)
            .context("failed to set clipboard in clipboard-only mode")?;
        return Ok(Injection::Delivered);
    }

    if !paste {
        let result = type_text_with_clear(text, keys, clear_line);
        if let Ok(KeystrokeOutcome::ModifierHeld) = result {
            // Not typed: the copy is the delivery, and it already covers
            // what `clipboard_fallback` would have written.
            return copy_instead_of_keystrokes(clipboard.as_ref(), text, keys.modifier_wait);
        }
        if clipboard_fallback {
            // The copy is the fallback, so it must happen even when the
            // typing failed — and a copy error must never change the Result
            // the caller sees (the typing result is authoritative).
            if let Err(e) = clipboard.set_text(text) {
                warn!("failed to set clipboard fallback: {e}");
            }
        }
        return result.map(|_| Injection::Delivered);
    }

    let saved = match clipboard.get_text() {
        Ok(s) => s,
        Err(e) => {
            debug!("clipboard unreadable as text, typing instead of pasting: {e:#}");
            return match type_text_with_clear(text, keys, clear_line)? {
                KeystrokeOutcome::Sent => Ok(Injection::Delivered),
                // Overwrites the non-text clipboard (#69's concern), but the
                // alternative is dropping the dictation, which is worse.
                KeystrokeOutcome::ModifierHeld => {
                    copy_instead_of_keystrokes(clipboard.as_ref(), text, keys.modifier_wait)
                }
            };
        }
    };
    clipboard
        .set_text(text)
        .context("failed to set clipboard for paste injection")?;

    // Let the clipboard settle before the paste keystroke.
    std::thread::sleep(std::time::Duration::from_millis(50));

    let paste_result = match paste_via_keyboard(is_terminal, keys, clear_line) {
        Ok(KeystrokeOutcome::Sent) => Ok(Injection::Delivered),
        Ok(KeystrokeOutcome::ModifierHeld) => {
            // No Ctrl+V went out, and the text is already on the clipboard:
            // leave it there, skipping the restore that would take it away.
            warn_copied_instead(keys.modifier_wait, text.len());
            return Ok(Injection::CopiedForHeldModifier);
        }
        Err(e) => Err(e),
    };

    if clipboard_fallback {
        // The pasted text stays in the clipboard — that IS the fallback, so
        // there is nothing to restore. Skip the delayed read-back + restore
        // entirely (the read-back would also be wrong here: it restores
        // exactly the value we want to keep).
        return paste_result;
    }

    // Restore the user's clipboard after the paste has landed, regardless of
    // whether the keystroke succeeded — but only if it's safe to (see doc
    // comment above).
    let pasted_text = text.to_string();
    std::thread::spawn(move || {
        std::thread::sleep(restore_delay);
        match clipboard.get_text() {
            Ok(current) if current == pasted_text => {
                if let Err(e) = clipboard.set_text(&saved) {
                    warn!("failed to restore clipboard: {e}");
                }
            }
            Ok(_) => {
                debug!("clipboard changed during paste injection; skipping restore");
            }
            Err(e) => {
                warn!("failed to read clipboard before restore, skipping restore: {e}");
            }
        }
    });

    paste_result
}

/// Command mode at a terminal: clear the prompt line (Ctrl+A, Ctrl+K), then
/// inject `text` so it replaces the highlighted command instead of being
/// appended to it. `clipboard_only` is the caller's to rule out: that mode
/// must never clear.
///
/// The clear and the text go out under one modifier wait and one hold of the
/// keyboard lock (#154), so they land together or not at all: if a modifier
/// is still held at the cap, neither is sent and `text` is copied to the
/// clipboard instead, leaving the old line untouched. A clear that fails
/// outright is still best-effort: the text is injected anyway, because
/// appending beats losing it.
pub(crate) fn clear_line_and_inject(
    text: &str,
    keys: KeystrokeSettings,
    paste: bool,
    clipboard_fallback: bool,
) -> Result<Injection> {
    clear_line_and_inject_with_clipboard(
        text,
        keys,
        paste,
        clipboard_fallback,
        CLIPBOARD_RESTORE_DELAY,
        Arc::from(xkb_type::default_clipboard()),
    )
}

/// Testable core of [`clear_line_and_inject`], clipboard injected as in
/// [`inject_text_with_clipboard`].
fn clear_line_and_inject_with_clipboard(
    text: &str,
    keys: KeystrokeSettings,
    paste: bool,
    clipboard_fallback: bool,
    restore_delay: std::time::Duration,
    clipboard: Arc<dyn ClipboardBackend>,
) -> Result<Injection> {
    inject_text_with_clipboard(
        text,
        /* is_terminal = */ true,
        keys,
        paste,
        clipboard_fallback,
        /* clipboard_only = */ false,
        /* clear_line = */ true,
        restore_delay,
        clipboard,
    )
}

pub(crate) fn warm_keyboard(key_delay: std::time::Duration, backend: InjectorBackend) {
    let keyboard_slot = KEYBOARD.get_or_init(|| StdMutex::new(None));
    let Ok(mut keyboard_guard) = keyboard_slot.lock() else {
        warn!("failed to initialize virtual keyboard: keyboard mutex poisoned");
        return;
    };

    if keyboard_guard.is_some() {
        return;
    }

    // Startup path: long settle delay so X11 has time to process
    // MappingNotify and attach the device keymap before the first key.
    match new_keyboard(key_delay, /* prewarm = */ true, backend) {
        Ok(kb) => {
            *keyboard_guard = Some(kb);
            info!("virtual keyboard initialized");
        }
        Err(e) => {
            warn!("failed to initialize virtual keyboard: {e:#}");
        }
    }
}

/// Build the uinput (evdev) keyboard, mapping the common permission failure
/// to an actionable message. `prewarm` adds a settle delay so X11 attaches
/// the device keymap before the first keystroke.
fn new_uinput_keyboard(
    key_delay: std::time::Duration,
    prewarm: bool,
) -> Result<Box<dyn xkb_type::KeyInjector>> {
    let result = if prewarm {
        xkb_type::Keyboard::new_prewarm(key_delay)
    } else {
        xkb_type::Keyboard::new(key_delay)
    };
    match result {
        Ok(kb) => Ok(Box::new(kb)),
        Err(e) => {
            let msg = format!("{e:#}");
            if msg.contains("Permission denied") || msg.contains("permission") {
                anyhow::bail!(
                    "Cannot open /dev/uinput — permission denied.\n\
                     Fix: sudo usermod -aG input $USER"
                );
            }
            Err(e.context("failed to create virtual keyboard"))
        }
    }
}

/// Construct the configured keyboard-injection backend.
///
/// `Auto` prefers the layout-independent Wayland virtual keyboard when a
/// Wayland session is detected, falling back to uinput when the compositor
/// lacks `zwp_virtual_keyboard_v1`. `prewarm` only affects the uinput path
/// (the Wayland backend ships its own keymap, so no settle delay is needed).
fn new_keyboard(
    key_delay: std::time::Duration,
    prewarm: bool,
    backend: InjectorBackend,
) -> Result<Box<dyn xkb_type::KeyInjector>> {
    match backend {
        InjectorBackend::Uinput => new_uinput_keyboard(key_delay, prewarm),
        InjectorBackend::WaylandVk => {
            let kb = xkb_type::wayland_vk::WaylandVkKeyboard::new(key_delay)?;
            info!("using wayland virtual-keyboard injection backend");
            Ok(Box::new(kb))
        }
        InjectorBackend::Auto => {
            if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                match xkb_type::wayland_vk::WaylandVkKeyboard::new(key_delay) {
                    Ok(kb) => {
                        info!("using wayland virtual-keyboard injection backend");
                        return Ok(Box::new(kb));
                    }
                    Err(e) => {
                        warn!(
                            "wayland virtual-keyboard unavailable, falling back to uinput: {e:#}"
                        );
                    }
                }
            }
            new_uinput_keyboard(key_delay, prewarm)
        }
    }
}

/// Whole-identifier terminal matches, lowercased. Covers both the bare X11
/// WM_CLASS class form (`Alacritty`, `st-256color`) and the reverse-DNS
/// Wayland app_id form (`org.gnome.Terminal`).
///
/// Matching is exact, never substring: an unanchored `contains` on `"st"`
/// false-positived on `steam`, `Postman` and `systemsettings`, and command
/// mode then wiped the whole GUI text field with Ctrl+A/Ctrl+K (#70).
///
/// Provenance notes for the entries we could not capture at runtime:
/// - `waveterm`, `tabby` — source-derived only, no runtime capture.
/// - `dev.warp.warp` — unverified, from secondary sources only.
/// - `org.xfce.terminal`, `org.contourterminal.contour` — speculative and
///   defensive; the strings we did verify are `xfce4-terminal` and `contour`.
/// - st sets its class from `opt_class ? opt_class : termname`, and shipped
///   `config.def.h` sets `termname = "st-256color"` — so vanilla st reports
///   `st-256color`, and bare `st` needs a patch or `-c st`. The enumerated
///   `st-*` entries cover the common termnames (`st-mono` is defensive; it is
///   absent from local ncurses terminfo). A build with a custom `termname` or
///   `-c` class will not match and needs a user-level override.
const TERMINAL_CLASSES: &[&str] = &[
    "alacritty",
    "blackbox-terminal",
    "contour",
    "cool-retro-term",
    "deepin-terminal",
    "foot",
    "footclient",
    "ghostty",
    "ghostty-debug",
    "gnome-terminal",
    "gnome-terminal-server",
    "guake",
    // `hyper` is a generic word; safe only because matching is whole-string.
    "hyper",
    "kgx",
    "kitty",
    "koi8rxterm",
    "konsole",
    "lxterminal",
    "mate-terminal",
    "mlterm",
    "ptyxis",
    "qterminal",
    "rio",
    "roxterm",
    "rxvt",
    "rxvt-unicode",
    "sakura",
    "st",
    "st-16color",
    "st-256color",
    "st-direct",
    "st-mono",
    "tabby",
    "terminator",
    "terminology",
    "termite",
    "tilix",
    "urxvt",
    "urxvtc",
    "uxterm",
    "waveterm",
    "wezterm",
    "xfce4-terminal",
    "xterm",
    "yakuake",
    // reverse-DNS app_ids
    "com.gexperts.tilix",
    "com.mitchellh.ghostty",
    "com.mitchellh.ghostty-debug",
    "com.raggesilver.blackbox",
    "dev.warp.warp",
    // full entry, not a leaf: `terminal` is an excluded generic leaf
    "io.elementary.terminal",
    "org.contourterminal.contour",
    "org.gnome.console",
    "org.gnome.console.devel",
    "org.gnome.ptyxis",
    "org.gnome.terminal",
    "org.kde.konsole",
    "org.kde.yakuake",
    "org.wezfurlong.wezterm",
    "org.xfce.terminal",
];

/// Distinctive leaf names, matched after stripping a reverse-DNS prefix, so
/// repackaged/forked app_ids (e.g. `io.example.Ghostty`) still resolve.
///
/// Deliberately EXCLUDES generic leaves — `terminal`, `console`, `warp`,
/// `wave`, `rio`, `st`, `foot`, `tabby`, `blackbox`, `contour`. The exclusion
/// applies only to the dotted/leaf form; several of these still match as whole
/// identifiers at stage 1 (`st`, `foot`, `rio`, `tabby`, `contour`). One
/// collision is demonstrated: `app.drey.Warp` (leaf `warp`) is GNOME's Magic
/// Wormhole client, not Warp Terminal. The rest are precautionary, not against
/// a known clash: they are generic enough that any vendor could ship a
/// non-terminal whose app_id ends in `.Contour`, `.BlackBox`, `.Terminal` or
/// `.Console` — a payment terminal, a serial console, a web console.
const TERMINAL_LEAF_CLASSES: &[&str] = &[
    "alacritty",
    "cool-retro-term",
    "ghostty",
    "ghostty-debug",
    "guake",
    "kitty",
    "konsole",
    "ptyxis",
    "qterminal",
    "tilix",
    "waveterm",
    "wezterm",
    "yakuake",
];

/// Check if a window class corresponds to a terminal emulator.
///
/// A false positive here is destructive (command mode clears the line), while a
/// false negative merely degrades to plain injection — so every stage matches
/// on whole identifiers or whole dot-segments, never on substrings.
///
/// `user_classes` is `[input] terminal_classes`: the opt-in escape hatch for
/// the classes the built-in list cannot know about — an `st` build with a
/// custom `termname`, and scratchpad/dropdown classes like `Alacritty-float`
/// (#92). It is checked *alongside* the built-in list, and only as a whole
/// identifier:
///
/// - Case-insensitive, like the built-in path, because compositors disagree on
///   casing (`Alacritty` on X11, `alacritty` elsewhere) and a config entry
///   should not have to guess.
/// - **Not** run through the leaf stage below. The leaf stage exists to rescue
///   app_ids the user never had to think about; a user entry is already the
///   exact string they read off `hyprctl activewindow`. Leaf-matching it would
///   turn a one-word entry into a whole-namespace wildcard — listing `warp`
///   would then also match `app.drey.Warp`, GNOME's Magic Wormhole client,
///   which is the destructive direction. An entry that *is* a dotted app_id
///   still matches that app_id exactly, so nothing is out of reach: it just
///   has to be named.
/// - Free to name a class the leaf set deliberately excludes (`warp`, `st`,
///   `terminal`, ...). Whole-identifier matching keeps that scoped to the one
///   window class the user actually opted into, so honoring it costs nothing
///   the exclusions were protecting.
pub(crate) fn is_terminal_class(class: &str, user_classes: &[String]) -> bool {
    let lower = class.trim().to_ascii_lowercase();
    if lower.is_empty() {
        debug!("is_terminal_class({class:?}): false (empty class)");
        return false;
    }
    // Stage 1: whole-identifier exact match against the built-in list.
    if TERMINAL_CLASSES.contains(&lower.as_str()) {
        debug!("is_terminal_class({class:?}): true (built-in whole identifier)");
        return true;
    }
    // Stage 2: whole-identifier exact match against the user's list. Entries
    // are trimmed for the same reason the class is; a blank entry can never
    // match, because an empty class returned above.
    if user_classes
        .iter()
        .any(|entry| entry.trim().eq_ignore_ascii_case(&lower))
    {
        debug!("is_terminal_class({class:?}): true (user terminal_classes entry)");
        return true;
    }
    // Stage 3: exact match on the last dot-segment, so repackaged reverse-DNS
    // app_ids still resolve. Only applies to dotted identifiers; a bare class
    // must appear in TERMINAL_CLASSES verbatim. Built-in leaves only — see the
    // doc comment for why user entries stop at stage 2.
    if let Some((_, leaf)) = lower.rsplit_once('.') {
        if TERMINAL_LEAF_CLASSES.contains(&leaf) {
            debug!("is_terminal_class({class:?}): true (built-in leaf {leaf:?})");
            return true;
        }
    }
    debug!("is_terminal_class({class:?}): false (no match in built-in list, user list, or built-in leaves)");
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `[input] terminal_classes` as the daemon hands it over.
    fn user(classes: &[&str]) -> Vec<String> {
        classes.iter().map(|c| c.to_string()).collect()
    }

    const TERMINAL: bool = true;
    const NOT_A_TERMINAL: bool = false;

    /// `[input] clipboard_only`: the reply is copied, never typed or pasted.
    const CLIPBOARD_ONLY: bool = true;
    /// The default: the reply is typed (or pasted) into the focused window.
    const INJECTING: bool = false;

    /// The reported defect, end to end: the model wraps a one-line command in
    /// a fence, and it is unwrapped and injected at either target, in either
    /// mode.
    #[test]
    fn a_fenced_one_liner_is_unwrapped_and_injected_anywhere() {
        for target in [TERMINAL, NOT_A_TERMINAL] {
            for mode in [INJECTING, CLIPBOARD_ONLY] {
                assert_eq!(
                    prepare_llm_injection("```bash\nsudo pacman -S steam\n```", target, mode),
                    LlmInjection::Inject("sudo pacman -S steam".to_string()),
                    "is_terminal = {target}, clipboard_only = {mode}"
                );
            }
        }
    }

    /// Cleaning does not depend on the target or on `clipboard_only`: padding
    /// and a wrapping fence go in every case. Only the multi-line verdict is
    /// conditional.
    #[test]
    fn cleaning_is_unconditional() {
        for target in [TERMINAL, NOT_A_TERMINAL] {
            for mode in [INJECTING, CLIPBOARD_ONLY] {
                assert_eq!(
                    prepare_llm_injection("  \n  Wo ist der Bahnhof?  \n", target, mode),
                    LlmInjection::Inject("Wo ist der Bahnhof?".to_string()),
                    "is_terminal = {target}, clipboard_only = {mode}"
                );
                assert_eq!(
                    prepare_llm_injection("```\nsudo pacman -S steam\n```\n", target, mode),
                    LlmInjection::Inject("sudo pacman -S steam".to_string()),
                    "is_terminal = {target}, clipboard_only = {mode}"
                );
            }
        }
    }

    /// The hazard: a line break typed at a shell prompt is an Enter that runs
    /// a command the user has not read. Refused, with the text handed back for
    /// the history log rather than dropped.
    #[test]
    fn multi_line_output_is_refused_at_a_terminal() {
        assert_eq!(
            prepare_llm_injection("```sh\ncd /tmp\nrm -rf x\n```", TERMINAL, INJECTING),
            LlmInjection::RefusedMultiLine("cd /tmp\nrm -rf x".to_string()),
            "the refusal must carry the cleaned text, not drop it"
        );
    }

    /// #121: `[input] clipboard_only` sends no keystroke at all, so a line
    /// break has nothing to submit and refusing would only keep the reply out
    /// of the clipboard the user asked for. Not refused, at a terminal or
    /// anywhere else, and still cleaned: the fence goes and CRLF becomes `\n`
    /// before the text is copied.
    #[test]
    fn multi_line_output_is_not_refused_under_clipboard_only() {
        for target in [TERMINAL, NOT_A_TERMINAL] {
            assert_eq!(
                prepare_llm_injection("```sh\ncd /tmp\nrm -rf x\n```", target, CLIPBOARD_ONLY),
                LlmInjection::Inject("cd /tmp\nrm -rf x".to_string()),
                "a fenced multi-line reply is unwrapped, not refused: is_terminal = {target}"
            );
            assert_eq!(
                prepare_llm_injection("echo one\r\necho two", target, CLIPBOARD_ONLY),
                LlmInjection::Inject("echo one\necho two".to_string()),
                "a CRLF reply is normalized, not refused: is_terminal = {target}"
            );
        }
    }

    /// The correction that makes the gate shareable: refusing *all* multi-line
    /// output would break the llm-command uses that produce it on purpose — a
    /// translated paragraph, a drafted email, a reformatted list. Away from a
    /// terminal there is nothing to submit, so it is injected normally.
    #[test]
    fn multi_line_output_is_injected_when_the_target_is_not_a_terminal() {
        let email = "Hi Sam,\n\nThe kitchen tap is leaking.\n\nThanks,\nAlex";
        assert_eq!(
            prepare_llm_injection(email, NOT_A_TERMINAL, INJECTING),
            LlmInjection::Inject(email.to_string())
        );
        assert_eq!(
            prepare_llm_injection(
                "```python\ndef f():\n    return 1\n```",
                NOT_A_TERMINAL,
                INJECTING
            ),
            LlmInjection::Inject("def f():\n    return 1".to_string()),
            "the fence still goes; only the refusal is terminal-only"
        );
    }

    /// CRLF and a bare CR are normalized before the verdict, so a `\r` can
    /// never slip through to the keymap (which taps it as a real Enter). At a
    /// terminal that means refused, not silently typed.
    #[test]
    fn carriage_returns_are_normalized_before_the_verdict() {
        assert_eq!(
            prepare_llm_injection("echo one\r\necho two", TERMINAL, INJECTING),
            LlmInjection::RefusedMultiLine("echo one\necho two".to_string())
        );
        assert_eq!(
            prepare_llm_injection("echo one\recho two", TERMINAL, INJECTING),
            LlmInjection::RefusedMultiLine("echo one\necho two".to_string())
        );
        // A trailing CRLF is padding on a single-line answer, not a second line.
        assert_eq!(
            prepare_llm_injection("sudo pacman -S steam\r\n", TERMINAL, INJECTING),
            LlmInjection::Inject("sudo pacman -S steam".to_string())
        );
    }

    /// Content on the fence line is never dropped, so such a reply stays
    /// multi-line and a terminal target refuses it — the user is told rather
    /// than handed a different command from the one the model wrote.
    #[test]
    fn a_fence_line_carrying_content_is_refused_not_truncated() {
        let LlmInjection::RefusedMultiLine(text) =
            prepare_llm_injection("```bash echo hi\nrm -rf /tmp/x\n```", TERMINAL, INJECTING)
        else {
            panic!("a reply with content on the fence line is multi-line");
        };
        assert!(
            text.contains("echo hi"),
            "content on the fence line must survive, got {text:?}"
        );
    }

    /// Nothing usable came back. Reported as its own outcome so the caller
    /// toasts instead of injecting nothing and logging an empty entry. Also
    /// under `clipboard_only`, where the alternative is copying an empty
    /// string over whatever the user had in the clipboard.
    #[test]
    fn an_unusable_reply_is_reported_as_empty() {
        for reply in ["", "   ", "\n\t\n", "\r\n", "```\n```", "```bash\n\n```"] {
            for target in [TERMINAL, NOT_A_TERMINAL] {
                for mode in [INJECTING, CLIPBOARD_ONLY] {
                    assert_eq!(
                        prepare_llm_injection(reply, target, mode),
                        LlmInjection::Empty,
                        "{reply:?} at is_terminal = {target}, clipboard_only = {mode}"
                    );
                }
            }
        }
    }

    /// Real-world strings, in the casing a compositor hands them to us: bare
    /// X11 WM_CLASS classes and reverse-DNS Wayland app_ids.
    #[test]
    fn terminal_classes_match() {
        for class in [
            "st",
            "st-256color",
            "st-direct",
            "gnome-terminal",
            "gnome-terminal-server",
            "org.gnome.Terminal",
            "org.gnome.Console",
            "org.gnome.Console.Devel",
            "kgx",
            "footclient",
            "foot",
            "konsole",
            "org.kde.konsole",
            "org.kde.yakuake",
            "org.wezfurlong.wezterm",
            "org.xfce.terminal",
            "org.contourterminal.contour",
            "com.mitchellh.ghostty",
            "com.mitchellh.ghostty-debug",
            "com.raggesilver.blackbox",
            "dev.warp.warp",
            "ghostty",
            "blackbox-terminal",
            "contour",
            "guake",
            "kitty",
            "rio",
            "rxvt",
            "sakura",
            "tabby",
            "termite",
            "tilix",
            "urxvtc",
            "waveterm",
            "wezterm",
            "xterm",
            "yakuake",
            "qterminal",
            "Alacritty",
            "cool-retro-term",
            "xfce4-terminal",
            "urxvt",
            "rxvt-unicode",
            "URxvt",
            "io.example.Ghostty",
            "Terminator",
            "Com.gexperts.Tilix",
            // added after the #70 review
            "uxterm",
            "koi8rxterm",
            "lxterminal",
            "roxterm",
            "ptyxis",
            "org.gnome.Ptyxis",
            "mate-terminal",
            "deepin-terminal",
            "io.elementary.terminal",
            "terminology",
            "mlterm",
            "hyper",
        ] {
            assert!(
                is_terminal_class(class, &[]),
                "{class} is a terminal but is_terminal_class says it is not"
            );
        }
    }

    /// No entry may rot into dead weight: stage 1 must match every one of its
    /// own strings.
    #[test]
    fn every_terminal_class_entry_matches() {
        for class in TERMINAL_CLASSES {
            assert!(
                is_terminal_class(class, &[]),
                "{class} is in TERMINAL_CLASSES but is_terminal_class says it is not a terminal"
            );
        }
    }

    /// Same for stage 2, exercised through a reverse-DNS prefix.
    #[test]
    fn every_terminal_leaf_class_entry_matches() {
        for leaf in TERMINAL_LEAF_CLASSES {
            let class = format!("io.example.{leaf}");
            assert!(
                is_terminal_class(&class, &[]),
                "{class} should match via TERMINAL_LEAF_CLASSES but does not"
            );
            // Stage 2 only fires on dotted identifiers, so the bare class form
            // is matched solely by TERMINAL_CLASSES. A leaf-only entry would
            // leave bare `{leaf}` unmatched — a silent false negative.
            assert!(
                TERMINAL_CLASSES.contains(leaf),
                "{leaf} is in TERMINAL_LEAF_CLASSES but not TERMINAL_CLASSES, so the bare class form `{leaf}` would not match"
            );
        }
    }

    /// The generics the leaf set deliberately omits. Adding any of these to
    /// `TERMINAL_LEAF_CLASSES` is the most destructive edit possible here, so
    /// pin every one of them false.
    #[test]
    fn excluded_generic_leaves_do_not_match() {
        for class in [
            "io.example.Terminal",
            "com.foo.Console",
            "x.y.st",
            "a.b.foot",
            "x.y.tabby",
            "a.b.rio",
            "com.mapbox.contour",
            "com.example.BlackBox",
            "app.drey.Warp",
            "com.example.Wave",
        ] {
            assert!(
                !is_terminal_class(class, &[]),
                "{class} is not a terminal but is_terminal_class says it is"
            );
        }
    }

    /// The `<base>-<suffix>` rule is enumerated, not open-ended: only the st
    /// `termname` values we list and the ghostty debug build match.
    #[test]
    fn hyphen_suffixes_are_enumerated_not_open_ended() {
        for class in [
            "st-link",
            "st-lite",
            "st-jerry",
            "st-",
            "st--",
            "ghostty-foo",
        ] {
            assert!(
                !is_terminal_class(class, &[]),
                "{class} is not a known terminal class but is_terminal_class says it is"
            );
        }
        for class in [
            "st-256color",
            "st-direct",
            "st-16color",
            "st-mono",
            "ghostty-debug",
            "com.mitchellh.ghostty-debug",
        ] {
            assert!(
                is_terminal_class(class, &[]),
                "{class} is a terminal but is_terminal_class says it is not"
            );
        }
    }

    /// Stage 2 needs an actual dot, so the leaf set is not a second bare-string
    /// list: only `blackbox-terminal` is a real class, plain `blackbox` is not.
    #[test]
    fn leaf_set_requires_a_dot() {
        assert!(!is_terminal_class("blackbox", &[]));
        assert!(is_terminal_class("blackbox-terminal", &[]));
        assert!(is_terminal_class("io.example.Ghostty", &[]));
    }

    /// The destructive direction: anything matched here gets Ctrl+A/Ctrl+K sent
    /// into it by command mode, which empties a GUI text field.
    #[test]
    fn non_terminal_classes_do_not_match() {
        for class in [
            "steam",
            "Steam",
            "Postman",
            "systemsettings",
            "gnome-system-monitor",
            "com.obsproject.Studio",
            "libreoffice-startcenter",
            "org.gnome.Settings",
            "obsidian",
            "code",
            "firefox",
            "Gnome-terminal-preferences",
            "org.gnome.Terminal.Preferences",
            "jconsole",
            "foot-server",
            "kitty-open",
            "assistant",
            "linguist",
            "lstopo",
            "xfce4-terminal-emulator",
            "Stremio",
            "standardnotes",
            "",
            "   ",
        ] {
            assert!(
                !is_terminal_class(class, &[]),
                "{class:?} is not a terminal but is_terminal_class says it is"
            );
        }
    }

    /// Issue #70, first half: the old `lower.contains("st")` matched these.
    #[test]
    fn repro_st_substring_matches_non_terminals_issue70() {
        for class in [
            "steam",
            "com.obsproject.Studio",
            "systemsettings",
            "Postman",
            "libreoffice-startcenter",
        ] {
            assert!(
                !is_terminal_class(class, &[]),
                "{class} is not a terminal but is_terminal_class says it is"
            );
        }
    }

    /// Issue #70, second half: the old list held only bare X11 classes, so
    /// every reverse-DNS Wayland app_id was missed.
    #[test]
    fn repro_wayland_gnome_terminal_is_missed_issue70() {
        for class in [
            "org.gnome.Terminal",
            "org.gnome.Console",
            "kgx",
            "rio",
            "qterminal",
        ] {
            assert!(
                is_terminal_class(class, &[]),
                "{class} is a terminal but is_terminal_class says it is not"
            );
        }
    }

    /// `[input] terminal_classes` entries match the whole class, in either
    /// casing: the compositor's (X11 hands us `Alacritty-float`) and the
    /// user's (they may have typed it lowercase, or with stray spaces).
    #[test]
    fn user_terminal_classes_match_whole_identifiers_case_insensitively() {
        let extra = user(&["st-mytermname", "  Alacritty-float  ", "kitty-dropdown"]);
        for class in [
            "st-mytermname",
            "ST-MYTERMNAME",
            "Alacritty-float",
            "alacritty-float",
            "ALACRITTY-FLOAT",
            "  alacritty-float  ",
            "kitty-dropdown",
            "Kitty-Dropdown",
        ] {
            assert!(
                is_terminal_class(class, &extra),
                "{class:?} is listed in terminal_classes but is_terminal_class says it is not a terminal"
            );
        }
    }

    /// Issue #92, first case: st takes its class from `termname` in
    /// `config.h`, so a renamed build matches nothing built in.
    #[test]
    fn repro_renamed_st_needs_a_user_entry_issue92() {
        for class in ["st-mytermname", "st-solarized", "mysuckless-term"] {
            assert!(
                !is_terminal_class(class, &[]),
                "{class} is not a built-in class; the built-in list must stay conservative"
            );
            assert!(
                is_terminal_class(class, &user(&[class])),
                "{class} is listed in terminal_classes but is_terminal_class says it is not a terminal"
            );
        }
    }

    /// Issue #92, second case: scratchpad and dropdown setups rename the
    /// class. The pre-#70 substring match caught these incidentally; exact
    /// matching does not, so they need an explicit entry.
    #[test]
    fn repro_scratchpad_classes_need_a_user_entry_issue92() {
        for class in ["Alacritty-float", "kitty-dropdown", "wezterm-quake"] {
            assert!(
                !is_terminal_class(class, &[]),
                "{class} must not match on its own — exact matching is the #70 fix"
            );
            assert!(
                is_terminal_class(class, &user(&[class])),
                "{class} is listed in terminal_classes but is_terminal_class says it is not a terminal"
            );
        }
    }

    /// A user entry must not reintroduce the #70 substring bug. Someone who
    /// lists a short generic name gets exactly that window class, nothing
    /// that merely contains it.
    #[test]
    fn user_entries_are_never_substring_matched() {
        let extra = user(&["st", "float", "term"]);
        for class in [
            "steam",
            "Postman",
            "systemsettings",
            "com.obsproject.Studio",
            "libreoffice-startcenter",
            "floating-window",
            "Alacritty-float",
            "terminal-preferences",
        ] {
            assert!(
                !is_terminal_class(class, &extra),
                "{class} only contains a terminal_classes entry; it must not match"
            );
        }
        // The entries themselves still match, as whole identifiers.
        for class in ["st", "float", "term"] {
            assert!(is_terminal_class(class, &extra));
        }
    }

    /// User entries stop at the whole-identifier stage. They are never
    /// leaf-matched in either direction, so a one-word entry can never turn
    /// into a whole-namespace wildcard over the generic leaves the built-in
    /// set deliberately excludes.
    #[test]
    fn user_entries_do_not_go_through_the_leaf_stage() {
        // `warp` is an excluded leaf: `app.drey.Warp` is GNOME's Magic
        // Wormhole client. Listing the bare word must not drag it in.
        let bare = user(&["warp"]);
        assert!(is_terminal_class("warp", &bare));
        assert!(!is_terminal_class("app.drey.Warp", &bare));
        assert!(!is_terminal_class("dev.example.warp", &bare));

        // Naming the full app_id is honored: it is exact, and the user opted
        // into that one class explicitly.
        let dotted = user(&["dev.example.warp"]);
        assert!(is_terminal_class("dev.example.warp", &dotted));
        assert!(!is_terminal_class("app.drey.Warp", &dotted));

        // Same in the other direction: a dotted entry does not match the bare
        // leaf, and the other excluded generics behave identically.
        assert!(!is_terminal_class("myterm", &user(&["com.example.myterm"])));
        for (entry, class) in [
            ("terminal", "com.paymentco.Terminal"),
            ("console", "org.example.Console"),
            ("blackbox", "com.example.BlackBox"),
        ] {
            assert!(
                !is_terminal_class(class, &user(&[entry])),
                "{class} must not match on a bare `{entry}` entry"
            );
        }
    }

    /// The default: an empty list is exactly today's behavior, and a list
    /// that names something unrelated changes no built-in verdict — in
    /// either direction.
    #[test]
    fn user_list_never_changes_the_builtin_verdicts() {
        let unrelated = user(&["Alacritty-float", "st-mytermname"]);
        for class in [
            "alacritty",
            "org.gnome.Terminal",
            "st-256color",
            "io.example.Ghostty",
            "hyper",
        ] {
            assert!(is_terminal_class(class, &[]), "{class} regressed at &[]");
            assert!(
                is_terminal_class(class, &unrelated),
                "{class} regressed with an unrelated terminal_classes list"
            );
        }
        for class in [
            "steam",
            "Postman",
            "systemsettings",
            "com.obsproject.Studio",
            "app.drey.Warp",
            "st-link",
            "",
            "   ",
        ] {
            assert!(
                !is_terminal_class(class, &[]),
                "{class:?} must not match with no user classes"
            );
            assert!(
                !is_terminal_class(class, &unrelated),
                "{class:?} must not match because of an unrelated terminal_classes entry"
            );
        }
    }

    /// A blank or whitespace-only entry is inert. It must not match the empty
    /// class, and above all it must not match everything.
    #[test]
    fn blank_user_entries_are_inert() {
        let blanks = user(&["", "   ", "\t"]);
        for class in ["", "   ", "steam", "firefox", "org.gnome.Settings"] {
            assert!(
                !is_terminal_class(class, &blanks),
                "{class:?} matched a blank terminal_classes entry"
            );
        }
    }

    /// End to end through the config type: an `[input]` table written before
    /// the key existed still parses, and the parsed list is what the daemon
    /// hands to `is_terminal_class`.
    #[test]
    fn terminal_classes_parses_from_the_input_table() {
        let old: whisrs::InputConfig = toml::from_str("key_delay_ms = 2\npaste = true\n").unwrap();
        assert!(old.terminal_classes.is_empty());
        assert!(!is_terminal_class("Alacritty-float", &old.terminal_classes));

        let new: whisrs::InputConfig =
            toml::from_str("terminal_classes = [\"Alacritty-float\", \"st-mytermname\"]\n")
                .unwrap();
        assert_eq!(new.terminal_classes, ["Alacritty-float", "st-mytermname"]);
        assert!(is_terminal_class("alacritty-float", &new.terminal_classes));
        assert!(is_terminal_class("st-mytermname", &new.terminal_classes));
        assert!(!is_terminal_class("steam", &new.terminal_classes));
    }

    // ---------------------------------------------------------------------
    // `inject_text` clipboard-fallback policy
    // ---------------------------------------------------------------------

    /// Serializes the injection tests: they swap a mock keyboard into the
    /// process-global `KEYBOARD` slot, and two tests doing that concurrently
    /// could install over each other.
    static KEYBOARD_TEST_LOCK: StdMutex<()> = StdMutex::new(());

    /// Scripted clipboard double, following the pattern from the
    /// `selection.rs` tests: `texts` is the sequence of `get_text` results
    /// (each call consumes the front entry; the last entry is sticky),
    /// `writes` records every `set_text` so tests can assert the
    /// fallback/restore policy byte-for-byte — including that nothing is
    /// ever written. An empty `texts` list makes any `get_text` panic, which
    /// is itself an assertion in the typing tests below (typing mode must
    /// never read the clipboard).
    struct ScriptedClipboard {
        texts: StdMutex<Vec<Option<String>>>,
        writes: StdMutex<Vec<String>>,
        fail_writes: bool,
    }

    impl ScriptedClipboard {
        fn new(texts: &[Option<&str>]) -> Self {
            Self {
                texts: StdMutex::new(texts.iter().map(|t| t.map(String::from)).collect()),
                writes: StdMutex::new(Vec::new()),
                fail_writes: false,
            }
        }

        fn writes(&self) -> Vec<String> {
            self.writes.lock().unwrap().clone()
        }
    }

    impl ClipboardBackend for ScriptedClipboard {
        fn get_text(&self) -> anyhow::Result<String> {
            let mut texts = self.texts.lock().unwrap();
            assert!(!texts.is_empty(), "get_text called with no scripted result");
            let head = if texts.len() > 1 {
                texts.remove(0)
            } else {
                texts[0].clone()
            };
            head.ok_or_else(|| anyhow::anyhow!("clipboard holds non-text content"))
        }

        fn set_text(&self, text: &str) -> anyhow::Result<()> {
            if self.fail_writes {
                return Err(anyhow::anyhow!("mock clipboard write failure"));
            }
            self.writes.lock().unwrap().push(text.to_string());
            Ok(())
        }

        fn get_primary_selection(&self) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    /// Scripted keyboard double: records typed text and paste combos instead
    /// of opening /dev/uinput. `fail_typing` makes `type_text` error, to
    /// exercise the "copy even when typing fails" fallback contract.
    /// `events` logs every keystroke in order, shared with the modifier
    /// probe tests so they can see what ran first.
    #[derive(Default, Clone)]
    struct MockKeyboard {
        typed: Arc<StdMutex<Vec<String>>>,
        paste_combos: Arc<StdMutex<usize>>,
        events: Arc<StdMutex<Vec<&'static str>>>,
        fail_typing: bool,
    }

    impl xkb_type::KeyInjector for MockKeyboard {
        fn type_text(&mut self, text: &str) -> anyhow::Result<()> {
            if self.fail_typing {
                return Err(anyhow::anyhow!("mock typing failure"));
            }
            self.events.lock().unwrap().push("type");
            self.typed.lock().unwrap().push(text.to_string());
            Ok(())
        }

        fn backspace(&mut self, _count: usize) -> anyhow::Result<()> {
            Ok(())
        }

        fn send_combo(&mut self, _keys: &[evdev::Key]) -> anyhow::Result<()> {
            self.events.lock().unwrap().push("combo");
            *self.paste_combos.lock().unwrap() += 1;
            Ok(())
        }

        fn set_key_delay(&mut self, _delay: std::time::Duration) {}
    }

    /// Run `f` with `keyboard` installed as the persistent virtual keyboard,
    /// restoring whatever occupied the slot before (a previous test's mock,
    /// or the real device slot) afterwards.
    fn with_keyboard(keyboard: MockKeyboard, f: impl FnOnce()) {
        let slot = KEYBOARD.get_or_init(|| StdMutex::new(None));
        let mut guard = slot.lock().unwrap();
        let previous = guard.take();
        *guard = Some(Box::new(keyboard));
        drop(guard);
        f();
        let mut guard = slot.lock().unwrap();
        *guard = previous;
    }

    /// Keystroke settings for tests: a 1 ms key delay and a 200 ms modifier
    /// cap, so a "held forever" probe times out quickly while a probe held
    /// for a few 15 ms polls still releases well inside it.
    fn test_keys() -> KeystrokeSettings {
        test_keys_waiting(std::time::Duration::from_millis(200))
    }

    fn test_keys_waiting(modifier_wait: std::time::Duration) -> KeystrokeSettings {
        KeystrokeSettings {
            key_delay: std::time::Duration::from_millis(1),
            backend: InjectorBackend::Uinput,
            modifier_wait,
        }
    }

    /// Run the testable inject core with a mock keyboard installed and a
    /// near-zero restore delay, then give any spawned restore thread time to
    /// land before returning. Generic over the concrete clipboard type so
    /// tests keep typed access to the recording mock after the call.
    fn inject<C: ClipboardBackend + 'static>(
        keyboard: MockKeyboard,
        clipboard: Arc<C>,
        text: &str,
        paste: bool,
        clipboard_fallback: bool,
        clipboard_only: bool,
    ) -> anyhow::Result<Injection> {
        inject_with_keys(
            test_keys(),
            keyboard,
            clipboard,
            text,
            paste,
            clipboard_fallback,
            clipboard_only,
        )
    }

    /// [`inject`] with explicit keystroke settings (for the modifier cap).
    fn inject_with_keys<C: ClipboardBackend + 'static>(
        keys: KeystrokeSettings,
        keyboard: MockKeyboard,
        clipboard: Arc<C>,
        text: &str,
        paste: bool,
        clipboard_fallback: bool,
        clipboard_only: bool,
    ) -> anyhow::Result<Injection> {
        let mut result = None;
        with_keyboard(keyboard, || {
            result = Some(inject_text_with_clipboard(
                text,
                /* is_terminal = */ false,
                keys,
                paste,
                clipboard_fallback,
                clipboard_only,
                /* clear_line = */ false,
                std::time::Duration::from_millis(5),
                clipboard,
            ));
            // The restore thread (when spawned) sleeps its 5 ms delay, then
            // does a clipboard read + write; 100 ms is ample for it to land
            // before the test inspects the record.
            std::thread::sleep(std::time::Duration::from_millis(100));
        });
        result.unwrap()
    }

    #[test]
    fn paste_with_fallback_keeps_the_pasted_text_in_the_clipboard() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Same script as the restoring sibling test below: the pre-paste save
        // reads "original", and a read-back — if one happened — would see the
        // pasted text and therefore restore. With the fallback on, no
        // read-back happens at all, so the pasted text stays in the clipboard.
        // Scripting it this way is what makes the assertion load-bearing:
        // with a sticky "original" the read-back would take the "clipboard
        // changed, skip restore" arm and the test would pass even with the
        // fallback logic deleted.
        let clipboard = Arc::new(ScriptedClipboard::new(&[
            Some("original"),
            Some("hello world"),
        ]));
        let keyboard = MockKeyboard::default();
        let paste_combos = Arc::clone(&keyboard.paste_combos);

        let result = inject(
            keyboard,
            Arc::clone(&clipboard),
            "hello world",
            /* paste = */ true,
            /* clipboard_fallback = */ true,
            /* clipboard_only = */ false,
        );

        assert!(result.is_ok());
        assert_eq!(*paste_combos.lock().unwrap(), 1, "Ctrl+V must have fired");
        assert_eq!(
            clipboard.writes(),
            vec!["hello world".to_string()],
            "only the paste write — no restore may follow"
        );
    }

    #[test]
    fn paste_without_fallback_still_restores_the_previous_clipboard() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Reads: the pre-paste save ("original"), then the delayed restore
        // read-back (still the pasted text, so nothing else copied over) →
        // the original is written back exactly once.
        let clipboard = Arc::new(ScriptedClipboard::new(&[
            Some("original"),
            Some("hello world"),
        ]));
        let keyboard = MockKeyboard::default();

        let result = inject(
            keyboard,
            Arc::clone(&clipboard),
            "hello world",
            /* paste = */ true,
            /* clipboard_fallback = */ false,
            /* clipboard_only = */ false,
        );

        assert!(result.is_ok());
        assert_eq!(
            clipboard.writes(),
            vec!["hello world".to_string(), "original".to_string()]
        );
    }

    #[test]
    fn typing_with_fallback_copies_the_typed_text() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Typing mode never reads the clipboard (empty script would panic on
        // any read), and the fallback writes the typed text once.
        let clipboard = Arc::new(ScriptedClipboard::new(&[]));
        let keyboard = MockKeyboard::default();
        let typed = Arc::clone(&keyboard.typed);

        let result = inject(
            keyboard,
            Arc::clone(&clipboard),
            "hello world",
            /* paste = */ false,
            /* clipboard_fallback = */ true,
            /* clipboard_only = */ false,
        );

        assert!(result.is_ok());
        assert_eq!(*typed.lock().unwrap(), vec!["hello world".to_string()]);
        assert_eq!(clipboard.writes(), vec!["hello world".to_string()]);
    }

    #[test]
    fn typing_without_fallback_never_touches_the_clipboard() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let clipboard = Arc::new(ScriptedClipboard::new(&[]));
        let keyboard = MockKeyboard::default();
        let typed = Arc::clone(&keyboard.typed);

        let result = inject(
            keyboard,
            Arc::clone(&clipboard),
            "hello world",
            /* paste = */ false,
            /* clipboard_fallback = */ false,
            /* clipboard_only = */ false,
        );

        assert!(result.is_ok());
        assert_eq!(*typed.lock().unwrap(), vec!["hello world".to_string()]);
        assert!(
            clipboard.writes().is_empty(),
            "clipboard must stay untouched when the fallback is off"
        );
    }

    #[test]
    fn typing_failure_still_copies_with_fallback_on() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // The copy is the fallback for the failure case: it must happen even
        // when typing errors, and the typing error stays the returned Result.
        let clipboard = Arc::new(ScriptedClipboard::new(&[]));
        let keyboard = MockKeyboard {
            fail_typing: true,
            ..Default::default()
        };

        let result = inject(
            keyboard,
            Arc::clone(&clipboard),
            "hello world",
            /* paste = */ false,
            /* clipboard_fallback = */ true,
            /* clipboard_only = */ false,
        );

        assert!(result.is_err(), "the typing failure is authoritative");
        assert_eq!(
            clipboard.writes(),
            vec!["hello world".to_string()],
            "the fallback copy must happen even when typing failed"
        );
    }

    #[test]
    fn clipboard_only_copies_and_never_injects() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Copy-only mode must not read the clipboard (empty script panics on
        // any read), must not type, and must not fire Ctrl+V — even with
        // `paste = true` set, which it overrides.
        let clipboard = Arc::new(ScriptedClipboard::new(&[]));
        let keyboard = MockKeyboard::default();
        let typed = Arc::clone(&keyboard.typed);
        let paste_combos = Arc::clone(&keyboard.paste_combos);

        let result = inject(
            keyboard,
            Arc::clone(&clipboard),
            "hello world",
            /* paste = */ true,
            /* clipboard_fallback = */ false,
            /* clipboard_only = */ true,
        );

        assert!(result.is_ok());
        assert_eq!(
            clipboard.writes(),
            vec!["hello world".to_string()],
            "the clipboard write is the output"
        );
        assert!(typed.lock().unwrap().is_empty(), "nothing may be typed");
        assert_eq!(
            *paste_combos.lock().unwrap(),
            0,
            "no Ctrl+V in copy-only mode"
        );
    }

    #[test]
    fn clipboard_only_copy_failure_is_an_error() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Unlike the typing-mode fallback (best-effort warn), a copy failure
        // in copy-only mode must surface — the copy is the entire feature.
        let mut clipboard = ScriptedClipboard::new(&[]);
        clipboard.fail_writes = true;
        let clipboard = Arc::new(clipboard);
        let keyboard = MockKeyboard::default();

        let result = inject(
            keyboard,
            Arc::clone(&clipboard),
            "hello world",
            /* paste = */ false,
            /* clipboard_fallback = */ false,
            /* clipboard_only = */ true,
        );

        assert!(
            result.is_err(),
            "copy failure must surface in copy-only mode"
        );
    }

    // ---------------------------------------------------------------------
    // Modifier release before injection (#154)
    // ---------------------------------------------------------------------

    /// A probe that reports "held" for its first `held_for` calls, then
    /// "released", counting every call.
    fn scripted_probe(held_for: usize) -> (Arc<StdMutex<usize>>, impl FnMut() -> bool) {
        let calls = Arc::new(StdMutex::new(0));
        let counter = Arc::clone(&calls);
        let probe = move || {
            let mut n = counter.lock().unwrap();
            *n += 1;
            *n <= held_for
        };
        (calls, probe)
    }

    #[test]
    fn modifier_wait_returns_at_once_when_nothing_is_held() {
        let (calls, probe) = scripted_probe(0);
        let start = std::time::Instant::now();
        let outcome = wait_for_modifier_release(
            probe,
            Some(std::time::Duration::from_secs(5)),
            std::time::Duration::from_secs(5),
            || false,
            || panic!("nothing was held"),
        );
        assert_eq!(outcome, ModifierWait::NotHeld);
        assert_eq!(*calls.lock().unwrap(), 1, "one probe, no poll");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "no sleep when nothing is held"
        );
    }

    #[test]
    fn modifier_wait_polls_until_release() {
        let (calls, probe) = scripted_probe(3);
        let outcome = wait_for_modifier_release(
            probe,
            Some(std::time::Duration::from_secs(5)),
            std::time::Duration::from_millis(1),
            || false,
            || {},
        );
        assert!(matches!(outcome, ModifierWait::Released(_)), "{outcome:?}");
        assert_eq!(
            *calls.lock().unwrap(),
            4,
            "3 held readings, then 1 released"
        );
    }

    #[test]
    fn modifier_wait_settles_after_a_release() {
        let (_calls, probe) = scripted_probe(1);
        let start = std::time::Instant::now();
        let outcome = wait_for_modifier_release(
            probe,
            Some(std::time::Duration::from_secs(5)),
            std::time::Duration::from_millis(1),
            || false,
            || {},
        );
        assert!(matches!(outcome, ModifierWait::Released(_)), "{outcome:?}");
        assert!(
            start.elapsed() >= MODIFIER_RELEASE_SETTLE,
            "the compositor gets time to see the physical release"
        );
    }

    #[test]
    fn modifier_wait_gives_up_at_the_timeout() {
        let (calls, probe) = scripted_probe(usize::MAX);
        let timeout = std::time::Duration::from_millis(30);
        let start = std::time::Instant::now();
        let outcome = wait_for_modifier_release(
            probe,
            Some(timeout),
            std::time::Duration::from_millis(5),
            || false,
            || {},
        );
        assert_eq!(outcome, ModifierWait::TimedOut);
        assert!(start.elapsed() >= timeout);
        assert!(*calls.lock().unwrap() > 1, "must have polled");
    }

    #[test]
    fn modifier_probe_runs_before_any_keystroke() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let clipboard = Arc::new(ScriptedClipboard::new(&[]));
        let keyboard = MockKeyboard::default();
        let events = Arc::clone(&keyboard.events);

        // Held for two readings: the keystroke must wait out both.
        let probe_events = Arc::clone(&events);
        let mut readings = 0;
        let probe = move || {
            probe_events.lock().unwrap().push("probe");
            readings += 1;
            readings <= 2
        };

        let mut result = None;
        with_modifier_probe(probe, || {
            result = Some(inject(
                keyboard,
                Arc::clone(&clipboard),
                "hello world",
                /* paste = */ false,
                /* clipboard_fallback = */ false,
                /* clipboard_only = */ false,
            ));
        });

        assert!(result.unwrap().is_ok());
        assert_eq!(
            *events.lock().unwrap(),
            vec!["probe", "probe", "probe", "type"],
            "every probe reading precedes the first keystroke"
        );
    }

    #[test]
    fn clipboard_only_never_checks_modifiers() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let clipboard = Arc::new(ScriptedClipboard::new(&[]));
        let (calls, probe) = scripted_probe(0);

        let mut result = None;
        with_modifier_probe(probe, || {
            result = Some(inject(
                MockKeyboard::default(),
                Arc::clone(&clipboard),
                "hello world",
                /* paste = */ true,
                /* clipboard_fallback = */ false,
                /* clipboard_only = */ true,
            ));
        });

        assert!(result.unwrap().is_ok());
        assert_eq!(*calls.lock().unwrap(), 0, "no keystroke, so no wait");
    }

    /// Run `f` with a probe that reports "held" on every reading, counting
    /// the readings.
    fn with_modifier_held_forever(f: impl FnOnce()) -> usize {
        let (calls, probe) = scripted_probe(usize::MAX);
        with_modifier_probe(probe, f);
        let n = *calls.lock().unwrap();
        n
    }

    #[test]
    fn zero_modifier_wait_probes_once_and_copies_when_held() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let clipboard = Arc::new(ScriptedClipboard::new(&[]));
        let keyboard = MockKeyboard::default();
        let events = Arc::clone(&keyboard.events);

        let mut result = None;
        let probes = with_modifier_held_forever(|| {
            result = Some(inject_with_keys(
                test_keys_waiting(std::time::Duration::ZERO),
                keyboard,
                Arc::clone(&clipboard),
                "hello world",
                /* paste = */ false,
                /* clipboard_fallback = */ false,
                /* clipboard_only = */ false,
            ));
        });

        assert_eq!(result.unwrap().unwrap(), Injection::CopiedForHeldModifier);
        assert_eq!(probes, 1, "modifier_wait_ms = 0 reads once, no polling");
        assert!(events.lock().unwrap().is_empty(), "no keystroke at all");
        assert_eq!(clipboard.writes(), vec!["hello world".to_string()]);
    }

    #[test]
    fn zero_modifier_wait_types_when_nothing_is_held() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let clipboard = Arc::new(ScriptedClipboard::new(&[]));
        let keyboard = MockKeyboard::default();
        let typed = Arc::clone(&keyboard.typed);
        let (calls, probe) = scripted_probe(0);

        let mut result = None;
        with_modifier_probe(probe, || {
            result = Some(inject_with_keys(
                test_keys_waiting(std::time::Duration::ZERO),
                keyboard,
                Arc::clone(&clipboard),
                "hello world",
                /* paste = */ false,
                /* clipboard_fallback = */ false,
                /* clipboard_only = */ false,
            ));
        });

        assert_eq!(result.unwrap().unwrap(), Injection::Delivered);
        assert_eq!(*calls.lock().unwrap(), 1);
        assert_eq!(*typed.lock().unwrap(), vec!["hello world".to_string()]);
        assert!(clipboard.writes().is_empty());
    }

    #[test]
    fn held_modifier_copies_instead_of_typing() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Typing mode never reads the clipboard (empty script panics on a
        // read). With the fallback on too, the text is still written once.
        let clipboard = Arc::new(ScriptedClipboard::new(&[]));
        let keyboard = MockKeyboard::default();
        let events = Arc::clone(&keyboard.events);

        let mut result = None;
        let probes = with_modifier_held_forever(|| {
            result = Some(inject(
                keyboard,
                Arc::clone(&clipboard),
                "hello world",
                /* paste = */ false,
                /* clipboard_fallback = */ true,
                /* clipboard_only = */ false,
            ));
        });

        assert_eq!(result.unwrap().unwrap(), Injection::CopiedForHeldModifier);
        assert!(probes > 1, "must have polled until the cap");
        assert!(events.lock().unwrap().is_empty(), "no keystroke at all");
        assert_eq!(clipboard.writes(), vec!["hello world".to_string()]);
    }

    #[test]
    fn held_modifier_leaves_the_paste_text_in_the_clipboard() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Scripted so that a restore, if one ran, would write "original"
        // back: the read-back would see the pasted text.
        let clipboard = Arc::new(ScriptedClipboard::new(&[
            Some("original"),
            Some("hello world"),
        ]));
        let keyboard = MockKeyboard::default();
        let events = Arc::clone(&keyboard.events);

        let mut result = None;
        with_modifier_held_forever(|| {
            result = Some(inject(
                keyboard,
                Arc::clone(&clipboard),
                "hello world",
                /* paste = */ true,
                /* clipboard_fallback = */ false,
                /* clipboard_only = */ false,
            ));
        });

        assert_eq!(result.unwrap().unwrap(), Injection::CopiedForHeldModifier);
        assert!(events.lock().unwrap().is_empty(), "no Ctrl+V");
        assert_eq!(
            clipboard.writes(),
            vec!["hello world".to_string()],
            "the text stays; the previous clipboard is not restored"
        );
    }

    #[test]
    fn held_modifier_with_unreadable_clipboard_still_copies() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Paste mode, non-text clipboard: the path falls back to typing, and
        // a held modifier then withholds that too. The text is copied rather
        // than lost.
        let clipboard = Arc::new(ScriptedClipboard::new(&[None]));
        let keyboard = MockKeyboard::default();
        let events = Arc::clone(&keyboard.events);

        let mut result = None;
        with_modifier_held_forever(|| {
            result = Some(inject(
                keyboard,
                Arc::clone(&clipboard),
                "hello world",
                /* paste = */ true,
                /* clipboard_fallback = */ false,
                /* clipboard_only = */ false,
            ));
        });

        assert_eq!(result.unwrap().unwrap(), Injection::CopiedForHeldModifier);
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(clipboard.writes(), vec!["hello world".to_string()]);
    }

    #[test]
    fn held_modifier_copy_failure_is_an_error() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut clipboard = ScriptedClipboard::new(&[]);
        clipboard.fail_writes = true;
        let clipboard = Arc::new(clipboard);

        let mut result = None;
        with_modifier_held_forever(|| {
            result = Some(inject(
                MockKeyboard::default(),
                Arc::clone(&clipboard),
                "hello world",
                /* paste = */ false,
                /* clipboard_fallback = */ false,
                /* clipboard_only = */ false,
            ));
        });

        assert!(
            result.unwrap().is_err(),
            "the copy was the only delivery; its failure must surface"
        );
    }

    #[test]
    fn uncapped_modifier_wait_ends_only_on_release_or_cancel() {
        // Held for far longer than any batch cap would allow: still waits.
        let (calls, probe) = scripted_probe(200);
        let outcome =
            wait_for_modifier_release(probe, None, std::time::Duration::ZERO, || false, || {});
        assert!(matches!(outcome, ModifierWait::Released(_)), "{outcome:?}");
        assert_eq!(*calls.lock().unwrap(), 201);

        // Held forever: only the cancel check ends it.
        let (_calls, probe) = scripted_probe(usize::MAX);
        let mut checks = 0;
        let outcome = wait_for_modifier_release(
            probe,
            None,
            std::time::Duration::ZERO,
            || {
                checks += 1;
                checks > 50
            },
            || {},
        );
        assert_eq!(outcome, ModifierWait::Cancelled);
    }

    /// Streaming waits out a held modifier however long it takes (here far
    /// past both the 200 ms test cap and a zero cap), then types. There is
    /// no clipboard path at all: `deliver_streaming_delta` takes none.
    #[test]
    fn streaming_delta_waits_for_release_past_any_cap_then_types() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for cap in [
            std::time::Duration::ZERO,
            std::time::Duration::from_millis(200),
        ] {
            let keyboard = MockKeyboard::default();
            let typed = Arc::clone(&keyboard.typed);
            // 30 held polls at 15 ms: ~450 ms, more than twice the cap.
            let (calls, probe) = scripted_probe(30);
            let cancel = AtomicBool::new(false);
            let tracker = ModifierWaitTracker::default();

            let mut delivery = None;
            let start = std::time::Instant::now();
            with_keyboard(keyboard, || {
                with_modifier_probe(probe, || {
                    delivery = Some(
                        deliver_streaming_delta(
                            "hello",
                            false,
                            test_keys_waiting(cap),
                            &cancel,
                            &tracker,
                        )
                        .unwrap(),
                    );
                });
            });

            assert_eq!(delivery, Some(StreamingDelivery::Typed), "cap {cap:?}");
            assert!(start.elapsed() > std::time::Duration::from_millis(400));
            assert_eq!(*calls.lock().unwrap(), 31, "polled until the release");
            assert_eq!(*typed.lock().unwrap(), vec!["hello".to_string()]);
            assert!(
                tracker
                    .drain_deadline(start, std::time::Duration::ZERO)
                    .is_some_and(|deadline| deadline > start),
                "the finished wait was recorded"
            );
        }
    }

    /// `whisrs cancel` during the wait: the delta is discarded, nothing typed.
    #[test]
    fn cancel_aborts_a_streaming_delta_waiting_on_a_modifier() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let keyboard = MockKeyboard::default();
        let events = Arc::clone(&keyboard.events);
        let cancel = Arc::new(AtomicBool::new(false));
        let tracker = ModifierWaitTracker::default();

        // Held forever; the "user" cancels on the 10th reading.
        let probe_cancel = Arc::clone(&cancel);
        let mut readings = 0;
        let probe = move || {
            readings += 1;
            if readings == 10 {
                probe_cancel.store(true, Ordering::SeqCst);
            }
            true
        };

        let mut delivery = None;
        with_keyboard(keyboard, || {
            with_modifier_probe(probe, || {
                delivery = Some(
                    deliver_streaming_delta("secret", false, test_keys(), &cancel, &tracker)
                        .unwrap(),
                );
            });
        });

        assert_eq!(delivery, Some(StreamingDelivery::Cancelled));
        assert!(events.lock().unwrap().is_empty(), "nothing typed");
    }

    /// Deltas that arrive while one waits on a held modifier queue in the
    /// batcher and are typed in order once it is released.
    #[test]
    fn streaming_deltas_queued_during_a_modifier_wait_are_typed_in_order() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let keyboard = MockKeyboard::default();
        let typed = Arc::clone(&keyboard.typed);
        let (text_tx, text_rx) = tokio::sync::mpsc::channel::<String>(64);
        text_tx.try_send("one".to_string()).unwrap();

        // Held for the first 6 readings; the backend delivers two more
        // deltas mid-wait, then closes its end of the channel.
        let mut probe_tx = Some(text_tx);
        let mut readings = 0;
        let probe = move || {
            readings += 1;
            if readings == 2 {
                if let Some(tx) = &probe_tx {
                    tx.try_send(" two".to_string()).unwrap();
                }
            }
            if readings == 3 {
                if let Some(tx) = probe_tx.take() {
                    tx.try_send(" three".to_string()).unwrap();
                }
            }
            readings <= 6
        };

        let cancel = Arc::new(AtomicBool::new(false));
        let tracker = ModifierWaitTracker::default();
        let mut full_text = String::new();
        with_keyboard(keyboard, || {
            with_modifier_probe(probe, || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                    .unwrap();
                full_text = runtime.block_on(crate::pipeline::run_typing_batcher(
                    text_rx,
                    Arc::clone(&cancel),
                    None,
                    |text| {
                        // Same thread as the probe: deliver synchronously.
                        let delivery =
                            deliver_streaming_delta(&text, false, test_keys(), &cancel, &tracker)
                                .unwrap();
                        assert_eq!(delivery, StreamingDelivery::Typed);
                        async {}
                    },
                ));
            });
        });

        assert_eq!(
            *typed.lock().unwrap(),
            vec!["one".to_string(), " two three".to_string()]
        );
        assert_eq!(full_text, "one two three");
    }

    #[test]
    fn drain_deadline_pauses_while_a_delta_waits_on_a_modifier() {
        let budget = std::time::Duration::from_secs(5);
        let tracker = ModifierWaitTracker::default();
        let drain_start = std::time::Instant::now();
        assert_eq!(
            tracker.drain_deadline(drain_start, budget),
            Some(drain_start + budget),
            "no wait yet: the plain budget"
        );

        tracker.begin();
        assert_eq!(tracker.drain_deadline(drain_start, budget), None);

        tracker.end();
        let deadline = tracker.drain_deadline(drain_start, budget).unwrap();
        assert!(
            deadline >= drain_start + budget,
            "the budget restarts at the release"
        );
    }

    #[test]
    fn command_mode_held_modifier_skips_clear_and_injection() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let clipboard = Arc::new(ScriptedClipboard::new(&[]));
        let keyboard = MockKeyboard::default();
        let events = Arc::clone(&keyboard.events);

        let mut result = None;
        with_keyboard(keyboard, || {
            with_modifier_held_forever(|| {
                result = Some(clear_line_and_inject_with_clipboard(
                    "git status",
                    test_keys(),
                    /* paste = */ false,
                    /* clipboard_fallback = */ false,
                    std::time::Duration::from_millis(5),
                    Arc::clone(&clipboard) as Arc<dyn ClipboardBackend>,
                ));
            });
        });

        assert_eq!(result.unwrap().unwrap(), Injection::CopiedForHeldModifier);
        assert!(
            events.lock().unwrap().is_empty(),
            "no Ctrl+A/Ctrl+K and no typing"
        );
        assert_eq!(clipboard.writes(), vec!["git status".to_string()]);
    }

    #[test]
    fn command_mode_clears_then_types_once_released() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let clipboard = Arc::new(ScriptedClipboard::new(&[]));
        let keyboard = MockKeyboard::default();
        let events = Arc::clone(&keyboard.events);
        let typed = Arc::clone(&keyboard.typed);
        let (_calls, probe) = scripted_probe(2);

        let mut result = None;
        with_keyboard(keyboard, || {
            with_modifier_probe(probe, || {
                result = Some(clear_line_and_inject_with_clipboard(
                    "git status",
                    test_keys(),
                    /* paste = */ false,
                    /* clipboard_fallback = */ false,
                    std::time::Duration::from_millis(5),
                    Arc::clone(&clipboard) as Arc<dyn ClipboardBackend>,
                ));
            });
        });

        assert_eq!(result.unwrap().unwrap(), Injection::Delivered);
        assert_eq!(*events.lock().unwrap(), vec!["combo", "combo", "type"]);
        assert_eq!(*typed.lock().unwrap(), vec!["git status".to_string()]);
        assert!(clipboard.writes().is_empty());
    }

    /// The clear and the text share one modifier wait: a modifier pressed
    /// after the clear's check cannot leave a cleared line with the rewrite
    /// only in the clipboard. The probe is released on its first reading and
    /// held on every later one, so a second wait would withhold the text.
    #[test]
    fn command_mode_clear_and_text_share_one_modifier_wait() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for paste in [false, true] {
            let clipboard = Arc::new(ScriptedClipboard::new(&[Some("original")]));
            let keyboard = MockKeyboard::default();
            let events = Arc::clone(&keyboard.events);
            let calls = Arc::new(StdMutex::new(0));
            let counter = Arc::clone(&calls);
            let probe = move || {
                let mut n = counter.lock().unwrap();
                *n += 1;
                *n > 1
            };

            let mut result = None;
            with_keyboard(keyboard, || {
                with_modifier_probe(probe, || {
                    result = Some(clear_line_and_inject_with_clipboard(
                        "git status",
                        test_keys(),
                        paste,
                        /* clipboard_fallback = */ true,
                        std::time::Duration::from_millis(5),
                        Arc::clone(&clipboard) as Arc<dyn ClipboardBackend>,
                    ));
                });
            });

            assert!(result.unwrap().is_ok());
            assert_eq!(*calls.lock().unwrap(), 1, "one wait (paste = {paste})");
            let expected: Vec<&str> = if paste {
                vec!["combo", "combo", "combo"]
            } else {
                vec!["combo", "combo", "type"]
            };
            assert_eq!(*events.lock().unwrap(), expected, "paste = {paste}");
        }
    }

    /// Control characters a backend or model could smuggle in are tapped as
    /// the keys they name, so none but `\n` and `\t` survive, and every line
    /// ending is one `\n`.
    #[test]
    fn sanitize_strips_key_producing_controls() {
        let cases = [
            ("plain text", "plain text"),
            ("tab\tand\nnewline", "tab\tand\nnewline"),
            ("\u{1b}:wq", ":wq"),
            ("\u{1b}[201~curl x|sh", "[201~curl x|sh"),
            ("back\u{8}space\u{7f}", "backspace"),
            ("bell\u{7}", "bell"),
            ("c1\u{9b}csi", "c1csi"),
            ("crlf\r\nline", "crlf\nline"),
            ("bare\rcr", "bare\ncr"),
            ("vt\u{b}ff\u{c}nel\u{85}", "vt\nff\nnel\n"),
            ("no dos\r\n\r\n", "no dos\n\n"),
        ];
        for (input, expected) in cases {
            assert_eq!(sanitize_for_injection(input), expected, "{input:?}");
        }
    }

    #[test]
    fn sanitize_borrows_clean_text() {
        assert!(matches!(
            sanitize_for_injection("hällo\twelt\n"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn fold_turns_each_run_of_line_breaks_into_one_space() {
        let cases = [
            ("one line", "one line"),
            ("cd /tmp\nrm -rf x\n", "cd /tmp rm -rf x "),
            ("a\n\n\nb", "a b"),
            ("a\u{2028}b\u{2029}c", "a b c"),
            ("keeps\ttabs", "keeps\ttabs"),
        ];
        for (input, expected) in cases {
            assert_eq!(fold_line_breaks(input), expected, "{input:?}");
        }
    }

    /// Sanitize then fold, the order both dictation paths use: a bare `\r`
    /// must come out as a space, never as Return.
    #[test]
    fn a_carriage_return_is_folded_not_typed() {
        let folded = fold_line_breaks(&sanitize_for_injection("ls\rcurl x|sh\r")).into_owned();
        assert_eq!(folded, "ls curl x|sh ");
    }

    fn input_with(unknown_window_is_terminal: bool, classes: &[&str]) -> whisrs::InputConfig {
        whisrs::InputConfig {
            unknown_window_is_terminal,
            terminal_classes: user(classes),
            ..Default::default()
        }
    }

    #[test]
    fn an_unknown_window_counts_as_a_terminal_by_default() {
        let input = whisrs::InputConfig::default();
        assert!(input.unknown_window_is_terminal, "secure default");
        for class in [None, Some(""), Some("   ")] {
            assert!(line_breaks_unsafe_at(class, &input), "{class:?}");
        }
    }

    #[test]
    fn an_unknown_window_is_not_a_terminal_when_opted_out() {
        let input = input_with(false, &[]);
        assert!(!line_breaks_unsafe_at(None, &input));
        assert!(!line_breaks_unsafe_at(Some(""), &input));
        // A known terminal is still guarded: the switch is about unknowns.
        assert!(line_breaks_unsafe_at(Some("kitty"), &input));
    }

    #[test]
    fn a_known_class_is_judged_by_the_terminal_list() {
        for unknown in [true, false] {
            let input = input_with(unknown, &["alacritty-float"]);
            assert!(line_breaks_unsafe_at(Some("Alacritty"), &input));
            assert!(line_breaks_unsafe_at(Some("alacritty-float"), &input));
            assert!(!line_breaks_unsafe_at(Some("firefox"), &input));
            assert!(!line_breaks_unsafe_at(Some("steam"), &input));
        }
    }

    /// An Escape is not a line break, so the gate alone would let it through
    /// on one line. The reply is sanitized before the verdict.
    #[test]
    fn an_escape_in_an_llm_reply_is_stripped_before_the_verdict() {
        assert_eq!(
            prepare_llm_injection("\u{1b}ZZ", TERMINAL, INJECTING),
            LlmInjection::Inject("ZZ".to_string())
        );
        assert_eq!(
            prepare_llm_injection("ok\u{b}rm -rf x", TERMINAL, INJECTING),
            LlmInjection::RefusedMultiLine("ok\nrm -rf x".to_string()),
            "a vertical tab is a line break and must be refused like one"
        );
        assert_eq!(
            prepare_llm_injection("\u{1b}", NOT_A_TERMINAL, INJECTING),
            LlmInjection::Empty
        );
    }

    #[test]
    fn typed_text_is_sanitized() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let clipboard = Arc::new(ScriptedClipboard::new(&[]));
        let keyboard = MockKeyboard::default();
        let typed = Arc::clone(&keyboard.typed);

        let result = inject(
            keyboard,
            Arc::clone(&clipboard),
            "hello\u{1b}:q!\r",
            /* paste = */ false,
            /* clipboard_fallback = */ false,
            /* clipboard_only = */ false,
        );

        assert!(result.is_ok());
        assert_eq!(*typed.lock().unwrap(), vec!["hello:q!\n".to_string()]);
    }

    /// The clipboard is a paste away from a terminal, so it gets the same
    /// treatment: `\x1b[201~` would end a bracketed paste early.
    #[test]
    fn copied_text_is_sanitized() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let clipboard = Arc::new(ScriptedClipboard::new(&[]));

        let result = inject(
            MockKeyboard::default(),
            Arc::clone(&clipboard),
            "safe\u{1b}[201~",
            /* paste = */ false,
            /* clipboard_fallback = */ false,
            /* clipboard_only = */ true,
        );

        assert!(result.is_ok());
        assert_eq!(clipboard.writes(), vec!["safe[201~".to_string()]);
    }

    #[test]
    fn a_streaming_delta_is_sanitized_and_folded_on_request() {
        let _lock = KEYBOARD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for (fold, expected) in [(true, "ls curl x "), (false, "ls\ncurl x\n")] {
            let keyboard = MockKeyboard::default();
            let typed = Arc::clone(&keyboard.typed);
            let cancel = AtomicBool::new(false);
            let tracker = ModifierWaitTracker::default();
            let mut delivery = None;
            with_keyboard(keyboard, || {
                with_modifier_probe(
                    || false,
                    || {
                        delivery = Some(
                            deliver_streaming_delta(
                                "ls\r\u{1b}curl x\n",
                                fold,
                                test_keys(),
                                &cancel,
                                &tracker,
                            )
                            .unwrap(),
                        );
                    },
                );
            });
            assert_eq!(delivery, Some(StreamingDelivery::Typed));
            assert_eq!(
                *typed.lock().unwrap(),
                vec![expected.to_string()],
                "fold = {fold}"
            );
        }
    }
}
