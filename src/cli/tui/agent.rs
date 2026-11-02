//! An agent's input, through taria, run as the keyboard's: an act on a node
//! of [`super::tree`] becomes the keys a person would press there (the
//! cursor moved onto the row, then `Enter`, `Space`, `x`), each through
//! [`App::act`]; a raw key goes through [`App::key`]; typed text goes only
//! where typing goes. No screen has a path of its own for an agent, so
//! whatever the keyboard checks, refuses or says, it does for an agent too.
//!
//! An act is refused (`Ignored`, nothing changed) unless the tree as it is
//! now has that node and advertises that action on it: an id from a tree
//! that moved on, an action behind a dialog, a row the profile cannot set.
//! What is the person's at the keyboard is refused with a status line that
//! says so: a key is never typed by an agent, `$EDITOR` takes the terminal
//! where an agent can neither see nor leave it, and setup's uinput fix asks
//! sudo for a password.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use taria_ratatui::taria::{Action, AgentInput};
use taria_ratatui::{to_crossterm_key, InputStatus};

use super::app::{App, Effect, Screen};
use super::keys::{self, Action as Key, Mode};
use super::schema::{self, Kind};
use super::screens::config::{ConfigScreen, Item};
use super::screens::home::SCREENS;
use super::screens::recordings::Naming;
use super::screens::setup::{Fix, Step};
use super::tree;
use super::widgets::hotkey::{KEY, RESULT, STOPS};
use super::widgets::picker::{Picker, Row as PickRow};
use super::widgets::{Notice, Tone};

/// The most characters a `set_value` or a `type_text` may carry: past it the
/// input is refused whole, before anything is typed. A config value is
/// never near it.
pub const MAX_TYPED: usize = 4096;

/// The status lines for what an agent is refused, as the person reads them.
pub const KEY_IS_HUMAN: &str = "a key is typed by the person at the keyboard, never by an agent";
pub const EDITOR_IS_HUMAN: &str =
    "$EDITOR is for the person at the keyboard: the TUI leaves the screen for it";
pub const SUDO_IS_HUMAN: &str =
    "the uinput fix runs sudo, which asks the person at the keyboard for a password";

/// Apply `input`, collecting what it asks the loop to run in `effects`.
/// `Ignored` for an input that did nothing.
#[warn(clippy::wildcard_enum_match_arm)]
pub fn apply(app: &mut App, input: AgentInput, effects: &mut Vec<Effect>) -> InputStatus {
    match input {
        AgentInput::Act {
            node,
            action,
            value,
            ..
        } => act(app, &node.0, &action, value.as_deref(), effects),
        AgentInput::Key { key, .. } => match to_crossterm_key(&key) {
            Some(key) => press(app, key, effects),
            // A key the grammar has and crossterm does not.
            None => InputStatus::Ignored,
        },
        // Typing, routed to what takes typing: never the key bindings.
        AgentInput::Text { text, .. } => type_text(app, &text, effects),
        AgentInput::Unknown => InputStatus::Ignored,
        // A kind a later taria adds: this build cannot do it.
        _ => InputStatus::Ignored,
    }
}

/// Run one keyboard action, keeping what it asks for.
fn key(app: &mut App, effects: &mut Vec<Effect>, action: Key) {
    effects.extend(app.act(action));
}

/// Say on the status line that this is the person's, and refuse it.
fn refuse(app: &mut App, why: &str) -> InputStatus {
    app.status = Some(Notice::new(Tone::Warn, why));
    InputStatus::Ignored
}

/// The Config rows the screen shows, if it shows them.
fn config_of(app: &App) -> Option<&ConfigScreen> {
    match app.screen() {
        Screen::Config | Screen::Mic => Some(&app.config),
        Screen::Profile => app.profile_config.as_ref(),
        Screen::Setup => app.setup.as_ref().map(|setup| &setup.editor),
        _ => None,
    }
}

/// Whether a key is being typed: a secret input has the keys.
fn typing_key(app: &App) -> bool {
    app.mode() == Mode::Text
        && config_of(app)
            .and_then(ConfigScreen::typed)
            .is_some_and(|typed| typed.input.is_secret())
}

/// Whether typed characters land somewhere now: a text input (not a key's)
/// or a picker's filter, with no dialog over it.
fn takes_typing(app: &App) -> bool {
    matches!(app.mode(), Mode::Text | Mode::Picker) && !typing_key(app)
}

/// Why `action` is the person's at the keyboard here, if it is.
fn human_only(app: &App, action: Key) -> Option<&'static str> {
    if typing_key(app) && !matches!(action, Key::Back | Key::Quit) {
        return Some(KEY_IS_HUMAN);
    }
    let mode = app.mode();
    match (&app.dialog, action) {
        (Some(_), Key::Type(c)) if app.answer_opens_editor(c) => return Some(EDITOR_IS_HUMAN),
        (Some(_), _) => return None,
        (None, Key::Editor) if mode == Mode::Profiles => return Some(EDITOR_IS_HUMAN),
        (None, Key::Editor) if mode == Mode::Browse && app.screen() != Screen::Setup => {
            return Some(EDITOR_IS_HUMAN)
        }
        _ => {}
    }
    let setup = app
        .setup
        .as_ref()
        .filter(|_| app.screen() == Screen::Setup)?;
    let sudo = setup.step == Step::Checks
        && mode == Mode::Browse
        && action == Key::Toggle
        && setup
            .check_rows(app.daemon_up() == Some(true))
            .into_iter()
            .nth(setup.cursor())
            .is_some_and(|row| row.fix == Some(Fix::Uinput));
    sudo.then_some(SUDO_IS_HUMAN)
}

/// A raw key: through the keyboard's own handler, wherever the keys are.
fn press(app: &mut App, event: KeyEvent, effects: &mut Vec<Effect>) -> InputStatus {
    let event = as_delivered(event);
    if idle_modifier(event, app.mode()) {
        return InputStatus::Ignored;
    }
    if let Some(action) = keys::action(event, app.mode()) {
        if let Some(why) = human_only(app, action) {
            return refuse(app, why);
        }
    }
    effects.extend(app.key(event));
    // A key bound to nothing here was still pressed.
    InputStatus::Delivered
}

/// An agent's key as a terminal delivers it: taria spells `shift+y` as a
/// lowercase `y` with Shift, and `Y` with none, but a person's Shift+Y
/// comes as `Y` with Shift. A chord with Ctrl or Alt is left as it came.
fn as_delivered(event: KeyEvent) -> KeyEvent {
    let KeyCode::Char(c) = event.code else {
        return event;
    };
    if !(event.modifiers - KeyModifiers::SHIFT).is_empty() {
        return event;
    }
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(capital), None) if capital != c && event.modifiers.contains(KeyModifiers::SHIFT) => {
            KeyEvent::new(KeyCode::Char(capital), event.modifiers)
        }
        _ if capital_of(c).is_some() => {
            KeyEvent::new(event.code, event.modifiers | KeyModifiers::SHIFT)
        }
        _ => event,
    }
}

/// The letter `c` is the capital of, when it is one.
fn capital_of(c: char) -> Option<char> {
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(letter), None) if letter != c => Some(letter),
        _ => None,
    }
}

/// The characters a US keyboard gives with Shift: Shift on one of them
/// is the character itself, not a modifier.
const SHIFTED: &str = "~!@#$%^&*()_+{}|:\"<>?";

/// Whether `event` (as delivered) carries a modifier its binding does not
/// read: `alt+y` answering a dialog as `y` would, `shift+Down` moving as
/// Down, a capital answering a dialog as its letter does, Shift on a key
/// that has no shifted character (`shift+1`). The keys a person presses
/// keep their bindings; an agent's chord means only what it says. A capital
/// whose binding is its own (`G`, `S`, typing), a shifted symbol (`?`),
/// BackTab, and a chord with a meaning of its own (`ctrl+c`, `ctrl+u`,
/// `shift+Left`) pass.
fn idle_modifier(event: KeyEvent, mode: Mode) -> bool {
    let mut plain = event.modifiers
        - (KeyModifiers::CONTROL
            | KeyModifiers::ALT
            | KeyModifiers::SUPER
            | KeyModifiers::HYPER
            | KeyModifiers::META);
    let mut code = event.code;
    match event.code {
        KeyCode::Char(c) if plain.contains(KeyModifiers::SHIFT) => {
            if let Some(letter) = capital_of(c) {
                // Compared with its letter: refused only where the binding
                // takes the letter in any case.
                code = KeyCode::Char(letter);
                plain.remove(KeyModifiers::SHIFT);
            } else if !SHIFTED.contains(c) {
                plain.remove(KeyModifiers::SHIFT);
            }
        }
        KeyCode::Char(_) | KeyCode::BackTab => {}
        _ => plain.remove(KeyModifiers::SHIFT),
    }
    (plain != event.modifiers || code != event.code)
        && keys::action(event, mode) == keys::action(KeyEvent::new(code, plain), mode)
}

/// Typed text: one character at a time into what takes typing, as long as
/// it still does. A newline is `Enter`, and the typing stops after it, as
/// at any other control character; a carriage return is passed over, so
/// CRLF text types as LF does.
fn type_text(app: &mut App, text: &str, effects: &mut Vec<Effect>) -> InputStatus {
    if text.chars().count() > MAX_TYPED {
        return InputStatus::Ignored;
    }
    if typing_key(app) {
        return refuse(app, KEY_IS_HUMAN);
    }
    if !takes_typing(app) {
        return InputStatus::Ignored;
    }
    let mut typed = false;
    for c in text.chars().filter(|c| *c != '\r') {
        if !takes_typing(app) {
            break;
        }
        if c == '\n' {
            key(app, effects, Key::Enter);
            typed = true;
            break;
        }
        if c.is_control() {
            break;
        }
        key(app, effects, Key::Type(c));
        typed = true;
    }
    if typed {
        InputStatus::Delivered
    } else {
        InputStatus::Ignored
    }
}

/// What an act lowers to, decided before anything changes.
enum Plan {
    /// These keys, in turn.
    Keys(Vec<Key>),
    /// Onto row `row` of Config's rows (an ordinal past the headings), then
    /// what `then` says.
    Row { row: usize, then: Then },
    /// Onto section `at` of the shown sections, then `Enter` if `open`.
    Section { at: usize, open: bool },
    /// Onto row `at` of a plain list (Home, recordings, history,
    /// profiles, setup's checks), then these keys.
    List { at: usize, then: Vec<Key> },
    /// Onto part `to` of the hotkey dialog, then these keys.
    Part { to: usize, then: Vec<Key> },
    /// Onto row `at` of the open picker, then pick it if `pick`.
    Option { at: usize, pick: bool },
    /// Type `text` into the open input and keep it with `Enter`, after
    /// these keys open it.
    Fill { open: Vec<Key>, text: String },
    /// Type `text` as the open picker's filter.
    Filter(String),
    /// Open the row's picker with these keys and pick `value` in it.
    Pick { open: Vec<Key>, value: String },
}

/// After the cursor is on a Config row.
enum Then {
    Stay,
    Keys(Vec<Key>),
    Fill {
        open: Vec<Key>,
        text: String,
    },
    Pick {
        open: Vec<Key>,
        value: String,
    },
    /// Open the hotkey dialog, type the spec there.
    Hotkey(String),
}

fn act(
    app: &mut App,
    id: &str,
    action: &Action,
    value: Option<&str>,
    effects: &mut Vec<Effect>,
) -> InputStatus {
    if value.is_some_and(|value| value.chars().count() > MAX_TYPED) {
        return InputStatus::Ignored;
    }
    let nodes = tree::build(app);
    // The person's, advertised or not: said, not only refused.
    if let Some(why) = human_act(app, id, action) {
        return refuse(app, why);
    }
    let Some(node) = tree::find(&nodes, id) else {
        return InputStatus::Ignored;
    };
    if node.disabled || !node.actions.contains(action) {
        return InputStatus::Ignored;
    }
    if *action == Action::SetValue && value.is_none() {
        return InputStatus::Ignored;
    }
    let Some(plan) = plan(app, id, action, value) else {
        return InputStatus::Ignored;
    };
    // Any key stops a sample being said, and does only that.
    if let Some(stopped) = app.lead() {
        effects.extend(stopped);
        return InputStatus::Delivered;
    }
    run(app, plan, effects);
    InputStatus::Delivered
}

/// An act that is the person's at the keyboard: a key's value, an answer
/// that opens `$EDITOR`, sudo.
fn human_act(app: &App, id: &str, action: &Action) -> Option<&'static str> {
    if let Some(letter) = tree::DIALOG.key(id) {
        let mut chars = letter.chars();
        return match (chars.next(), chars.next()) {
            (Some(c), None) => human_only(app, Key::Type(c)),
            _ => None,
        };
    }
    if *action == Action::SetValue {
        if id == "input" && typing_key(app) {
            return Some(KEY_IS_HUMAN);
        }
        let path = tree::ROW.key(id)?;
        let secret = config_of(app)?.rows().into_iter().any(|item| {
            matches!(item, Item::Field { field, path: p } if p == path && matches!(field.kind, Kind::Secret))
        });
        return secret.then_some(KEY_IS_HUMAN);
    }
    let setup = app
        .setup
        .as_ref()
        .filter(|_| app.screen() == Screen::Setup)?;
    let slug = tree::CHECK.key(id)?;
    let sudo = *action == Action::Activate
        && setup
            .check_rows(app.daemon_up() == Some(true))
            .iter()
            .any(|row| tree::slug(row.label) == slug && row.fix == Some(Fix::Uinput));
    sudo.then_some(SUDO_IS_HUMAN)
}

/// The keys a custom action stands for.
fn custom_key(name: &str) -> Option<Key> {
    Some(match name {
        tree::RESET => Key::Reset,
        tree::TEST => Key::Test,
        tree::DELETE => Key::Delete,
        tree::RETRY => Key::Retry,
        tree::COPY => Key::Copy,
        tree::NAME => Key::Name,
        tree::SUMMARIZE => Key::Summarize,
        tree::OPEN => Key::Right,
        tree::UNSET => Key::Reset,
        _ => return None,
    })
}

/// A scroll's value: a page up or down.
fn page(value: Option<&str>) -> Option<Key> {
    match value? {
        "up" => Some(Key::PageUp),
        "down" => Some(Key::PageDown),
        _ => None,
    }
}

/// A value an agent can type: one line, no control characters.
fn typeable(value: &str) -> Option<String> {
    (!value.chars().any(char::is_control)).then(|| value.to_string())
}

fn plan(app: &App, id: &str, action: &Action, value: Option<&str>) -> Option<Plan> {
    let keys = |keys: Vec<Key>| Some(Plan::Keys(keys));
    match (id, action) {
        ("quit", Action::Activate) => return keys(vec![Key::Quit]),
        ("back" | "dialog" | "picker" | "download", Action::Activate | Action::Dismiss) => {
            return keys(vec![Key::Back])
        }
        ("save", _) => return keys(vec![Key::Save]),
        ("restart", _) => return keys(vec![Key::Restart]),
        ("add", _) => return keys(vec![Key::Add]),
        ("next", _) => return keys(vec![Key::Enter]),
        ("skip", _) => return keys(vec![Key::Skip]),
        ("previous", _) => return keys(vec![Key::Previous]),
        ("test", _) => return keys(vec![Key::Test]),
        ("new", _) => return keys(vec![Key::New]),
        ("clear", _) => return keys(vec![Key::ClearAll]),
        ("transcript" | "text" | "recordings" | "dictations", Action::Scroll) => {
            return keys(vec![page(value)?])
        }
        ("recording", Action::Custom(name)) => return keys(vec![custom_key(name)?]),
        ("input", Action::Activate) => return keys(vec![Key::Enter]),
        ("input" | "filter", Action::Dismiss) => return keys(vec![Key::Back]),
        ("input", Action::SetValue) => {
            return Some(Plan::Fill {
                open: Vec::new(),
                text: typeable(value?)?,
            })
        }
        ("filter", Action::Focus) => return keys(vec![Key::Filter]),
        ("filter", Action::Activate) => return keys(vec![Key::Enter]),
        ("filter", Action::SetValue) => {
            // From the list, `/` opens it with the filter kept.
            let open = if app.mode() == Mode::Text {
                Vec::new()
            } else {
                vec![Key::Filter]
            };
            return Some(Plan::Fill {
                open,
                text: typeable(value?)?,
            });
        }
        ("picker-filter", Action::SetValue) => return Some(Plan::Filter(typeable(value?)?)),
        ("model", Action::Activate) => return keys(vec![Key::Toggle]),
        ("model", Action::SetValue) => {
            let value = value?;
            whisrs::config::catalog::WHISPER_MODELS
                .iter()
                .any(|m| m.name == value)
                .then_some(())?;
            return Some(Plan::Pick {
                open: vec![Key::Toggle],
                value: value.to_string(),
            });
        }
        ("hotkey", Action::Dismiss) => return keys(vec![Key::Back]),
        ("hotkey", Action::Custom(name)) => return keys(vec![custom_key(name)?]),
        ("hotkey", Action::SetValue) => {
            let spec = typeable(value?)?;
            schema::check_hotkey(&spec).ok()?;
            return Some(Plan::Fill {
                open: vec![Key::Type('t')],
                text: spec,
            });
        }
        _ => {}
    }
    if let Some(letter) = tree::DIALOG.key(id) {
        let mut chars = letter.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return None;
        };
        return keys(vec![Key::Type(c)]);
    }
    if let Some(name) = tree::HOME.key(id) {
        let at = SCREENS
            .iter()
            .position(|(screen, _)| tree::screen_key(*screen) == name)?;
        return Some(Plan::List {
            at,
            then: opens(action, Key::Enter),
        });
    }
    if let Some(slug) = tree::SECTION.key(id) {
        let screen = config_of(app)?;
        let at = screen
            .shown_sections()
            .iter()
            .position(|&i| tree::slug(schema::SECTIONS[i].title) == slug)?;
        return Some(Plan::Section {
            at,
            open: *action == Action::Activate,
        });
    }
    if let Some(slug) = tree::CHECK.key(id) {
        let setup = app.setup.as_ref()?;
        let at = setup
            .check_rows(app.daemon_up() == Some(true))
            .iter()
            .position(|row| tree::slug(row.label) == slug)?;
        return Some(Plan::List {
            at,
            then: opens(action, Key::Toggle),
        });
    }
    if let Some(n) = tree::CHOICE.parse::<usize>(id) {
        // Up is the first row, Down the second.
        let mut then = vec![if n == 0 { Key::Up } else { Key::Down }];
        if *action == Action::Activate {
            then.push(Key::Enter);
        }
        return keys(then);
    }
    if let Some(serial) = tree::REC.parse::<u64>(id) {
        let at = app.recordings.find_serial(serial)?;
        return Some(Plan::List {
            at,
            then: match action {
                Action::Custom(name) => vec![custom_key(name)?],
                other => opens(other, Key::Enter),
            },
        });
    }
    if let Some(name) = tree::PROFILE.key(id) {
        let at = app.profiles.rows().iter().position(|row| row == name)?;
        return Some(Plan::List {
            at,
            then: match action {
                Action::Custom(name) => vec![custom_key(name)?],
                other => opens(other, Key::Enter),
            },
        });
    }
    if let Some(stamp) = tree::DICT.key(id) {
        let keys = tree::dictation_keys(app.history.all());
        let index = keys.iter().position(|key| key == stamp)?;
        let at = app.history.visible().iter().position(|&i| i == index)?;
        return Some(Plan::List {
            at,
            then: opens(action, Key::Enter),
        });
    }
    if let Some(part) = tree::MODIFIER.key(id) {
        let to = whisrs::hotkey::MODIFIER_NAMES
            .iter()
            .position(|name| name.to_ascii_lowercase() == part)?;
        return Some(Plan::Part {
            to,
            then: match action {
                Action::Toggle => vec![Key::Toggle],
                _ => Vec::new(),
            },
        });
    }
    if let Some(part) = tree::HOTKEY.key(id) {
        let to = match part {
            "key" => KEY,
            "result" => RESULT,
            _ => return None,
        };
        return Some(Plan::Part {
            to,
            then: opens(action, Key::Enter),
        });
    }
    if let Some(key) = tree::OPTION.key(id) {
        let picker = open_picker(app)?;
        let at = picker.rows().iter().position(|row| match row {
            PickRow::Unset => key == "unset",
            PickRow::Use(_) => key == "use",
            PickRow::Pick(n) => key.parse() == Ok(*n),
        })?;
        return Some(Plan::Option {
            at,
            pick: *action == Action::Activate,
        });
    }
    config_row(app, id, action, value)
}

/// `Select` moves only; `Activate` then presses `open`.
fn opens(action: &Action, open: Key) -> Vec<Key> {
    match action {
        Action::Activate => vec![open],
        _ => Vec::new(),
    }
}

/// An act on a Config row: the row found among the rows shown, and what
/// its own editor does with the action.
fn config_row(app: &App, id: &str, action: &Action, value: Option<&str>) -> Option<Plan> {
    let screen = config_of(app)?;
    // Space opens a row on setup, where Enter is the next step.
    let open = if app.screen() == Screen::Setup {
        Key::Toggle
    } else {
        Key::Enter
    };
    let rows = screen.rows();
    let (at, item) = rows.iter().enumerate().find(|(_, item)| match item {
        Item::Field { path, .. } => tree::ROW.key(id) == Some(path.as_str()),
        Item::Entry(i) => tree::ENTRY.parse(id) == Some(*i),
        Item::Pair(key) => tree::PAIR.key(id) == Some(key.as_str()),
        Item::Element(i) => {
            tree::ITEM.parse::<u64>(id).is_some()
                && tree::ITEM.parse::<u64>(id) == screen.item_serial(*i)
        }
        Item::Heading(_) => false,
    })?;
    let row = rows[..at]
        .iter()
        .filter(|item| !matches!(item, Item::Heading(_)))
        .count();
    let then = match (item, action) {
        (_, Action::Select) => Then::Stay,
        (_, Action::Custom(name)) => Then::Keys(vec![custom_key(name)?]),
        (Item::Field { field, .. }, Action::Toggle) => match field.kind {
            // A three-way toggle steps its ring, as h/l does on every screen.
            Kind::MaybeBool { .. } => Then::Keys(vec![Key::Right]),
            _ => Then::Keys(vec![Key::Toggle]),
        },
        (Item::Field { .. } | Item::Entry(_), Action::Activate) => Then::Keys(vec![open]),
        (Item::Pair(_) | Item::Element(_), Action::Activate) => Then::Keys(vec![Key::Enter]),
        (Item::Field { field, path }, Action::SetValue) => {
            let value = typeable(value?)?;
            if matches!(
                field.kind,
                Kind::Bool | Kind::MaybeBool { .. } | Kind::Secret | Kind::List { .. }
            ) {
                return None;
            }
            // Decided before an editor opens: one left open with an error
            // is not an answer.
            screen.takes(field, path, &value).then_some(())?;
            match &field.kind {
                Kind::Choice(_)
                | Kind::MaybeChoice { .. }
                | Kind::Suggest { .. }
                | Kind::Path { from: Some(_), .. } => Then::Pick {
                    open: vec![open],
                    value: value.trim().to_string(),
                },
                Kind::Hotkey { .. } => Then::Hotkey(value),
                _ => Then::Fill {
                    open: vec![open],
                    text: value,
                },
            }
        }
        (Item::Pair(_), Action::SetValue) => Then::Fill {
            open: vec![Key::Enter],
            text: typeable(value?)?,
        },
        (Item::Element(_), Action::SetValue) => {
            let text = typeable(value?)?;
            screen.takes_item(&text).then_some(())?;
            if screen.level_suggests() {
                Then::Pick {
                    open: vec![Key::Enter],
                    value: text.trim().to_string(),
                }
            } else {
                Then::Fill {
                    open: vec![Key::Enter],
                    text,
                }
            }
        }
        _ => return None,
    };
    Some(Plan::Row { row, then })
}

/// The picker that has the keys, on any screen.
fn open_picker(app: &App) -> Option<&Picker> {
    match app.screen() {
        Screen::Setup => {
            let setup = app.setup.as_ref()?;
            setup.model_picker().or_else(|| setup.editor.picking())
        }
        Screen::Recordings => match app.recordings.naming() {
            Some(Naming::Picking { picker, .. }) => Some(picker),
            _ => None,
        },
        _ => config_of(app)?.picking(),
    }
}

fn run(app: &mut App, plan: Plan, effects: &mut Vec<Effect>) {
    match plan {
        Plan::Keys(keys) => {
            for action in keys {
                key(app, effects, action);
            }
        }
        Plan::Section { at, open } => {
            if !config_of(app).is_some_and(ConfigScreen::on_sections) {
                key(app, effects, Key::Pane);
            }
            move_to(app, effects, at);
            if open {
                key(app, effects, Key::Enter);
            }
        }
        Plan::List { at, then } => {
            move_to(app, effects, at);
            for action in then {
                key(app, effects, action);
            }
        }
        Plan::Part { to, then } => {
            let from = config_of(app)
                .and_then(ConfigScreen::hotkeying)
                .map_or(to, |dialog| dialog.focus());
            for _ in 0..(to + STOPS - from) % STOPS {
                key(app, effects, Key::Down);
            }
            for action in then {
                key(app, effects, action);
            }
        }
        Plan::Option { at, pick } => {
            move_to(app, effects, at);
            if pick {
                key(app, effects, Key::Enter);
            }
        }
        Plan::Fill { open, text } => {
            for action in open {
                key(app, effects, action);
            }
            fill(app, effects, &text);
        }
        Plan::Filter(text) => {
            if app.mode() != Mode::Picker {
                return;
            }
            key(app, effects, Key::Clear);
            for c in text.chars() {
                key(app, effects, Key::Type(c));
            }
        }
        Plan::Pick { open, value } => {
            for action in open {
                key(app, effects, action);
            }
            pick(app, effects, &value);
        }
        Plan::Row { row, then } => {
            if config_of(app).is_some_and(ConfigScreen::on_sections) {
                key(app, effects, Key::Right);
            }
            move_to(app, effects, row);
            match then {
                Then::Stay => {}
                Then::Keys(keys) => {
                    for action in keys {
                        key(app, effects, action);
                    }
                }
                Then::Fill { open, text } => {
                    for action in open {
                        key(app, effects, action);
                    }
                    fill(app, effects, &text);
                }
                Then::Pick { open, value } => {
                    for action in open {
                        key(app, effects, action);
                    }
                    pick(app, effects, &value);
                }
                Then::Hotkey(spec) => {
                    let open = if app.screen() == Screen::Setup {
                        Key::Toggle
                    } else {
                        Key::Enter
                    };
                    key(app, effects, open);
                    // A value that does not parse opens as typing at once.
                    if app.mode() == Mode::Hotkey {
                        key(app, effects, Key::Type('t'));
                    }
                    fill(app, effects, &spec);
                }
            }
        }
    }
}

/// The cursor to row `at` of the list with the keys, a row at a time as
/// `j` and `k` move it: on it already, no key at all.
fn move_to(app: &mut App, effects: &mut Vec<Effect>, at: usize) {
    let from = position(app);
    let (step, n) = if at >= from {
        (Key::Down, at - from)
    } else {
        (Key::Up, from - at)
    };
    for _ in 0..n {
        key(app, effects, step);
    }
}

/// Where the cursor of the list with the keys is: a picker's, Config's
/// sections or rows (past the headings), a screen's own list.
fn position(app: &App) -> usize {
    if app.mode() == Mode::Picker {
        return open_picker(app).map_or(0, Picker::cursor);
    }
    match app.screen() {
        Screen::Home => return app.home.cursor(),
        Screen::Recordings => return app.recordings.cursor(),
        Screen::History => return app.history.cursor(),
        Screen::Profiles => return app.profiles.cursor(),
        Screen::Setup => {
            if let Some(setup) = app.setup.as_ref().filter(|s| s.step == Step::Checks) {
                return setup.cursor();
            }
        }
        _ => {}
    }
    match config_of(app) {
        Some(screen) if screen.on_sections() => screen
            .shown_sections()
            .iter()
            .position(|&i| i == screen.section())
            .unwrap_or(0),
        Some(screen) => screen.rows()[..screen.cursor().min(screen.rows().len())]
            .iter()
            .filter(|item| !matches!(item, Item::Heading(_)))
            .count(),
        None => 0,
    }
}

/// Type `text` over what the open input holds and keep it, as the
/// keyboard would: `Ctrl+U`, the characters, `Enter`. Nothing is typed
/// unless an input has the keys.
fn fill(app: &mut App, effects: &mut Vec<Effect>, text: &str) {
    if app.mode() != Mode::Text || typing_key(app) {
        return;
    }
    key(app, effects, Key::Clear);
    for c in text.chars() {
        if app.mode() != Mode::Text {
            return;
        }
        key(app, effects, Key::Type(c));
    }
    key(app, effects, Key::Enter);
}

/// Pick `value` in the open picker: its row with nothing typed (`unset`
/// for the unset row), else typed, for a picker that takes any value.
fn pick(app: &mut App, effects: &mut Vec<Effect>, value: &str) {
    if app.mode() != Mode::Picker {
        return;
    }
    let find = |app: &App| {
        let picker = open_picker(app)?;
        picker.rows().iter().position(|row| match row {
            PickRow::Unset => value == "unset",
            PickRow::Pick(n) => picker.options.picks[*n].value == value,
            PickRow::Use(text) => text == value,
        })
    };
    key(app, effects, Key::Clear);
    let at = match find(app) {
        Some(at) => at,
        None => {
            for c in value.chars() {
                key(app, effects, Key::Type(c));
            }
            match find(app) {
                Some(at) => at,
                // Not a value it offers after all: no picker is left open.
                None => return key(app, effects, Key::Back),
            }
        }
    };
    move_to(app, effects, at);
    key(app, effects, Key::Enter);
    // Refused at the pick: the picker stays open, and Esc closes it.
    if app.mode() == Mode::Picker {
        key(app, effects, Key::Back);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    use taria_ratatui::taria::NodeId;

    use super::*;
    use crate::tui::app::{After, Dialog, Msg};
    use crate::tui::testing::{
        self, chord, dictation, mockup, mockup_profiles, open_history, open_profiles, open_section,
        press, setup_on, state, to_row, type_text, Fixture, PROFILES,
    };

    type Make = fn() -> Fixture;

    fn act(
        fixture: &mut Fixture,
        id: &str,
        action: Action,
        value: Option<&str>,
    ) -> (InputStatus, Vec<Effect>) {
        let mut effects = Vec::new();
        let input = AgentInput::act(NodeId(id.into()), action, value.map(String::from));
        (apply(&mut fixture.app, input, &mut effects), effects)
    }

    fn text(fixture: &mut Fixture, text: &str) -> (InputStatus, Vec<Effect>) {
        let mut effects = Vec::new();
        (
            apply(&mut fixture.app, AgentInput::text(text), &mut effects),
            effects,
        )
    }

    fn raw(fixture: &mut Fixture, key: &str) -> (InputStatus, Vec<Effect>) {
        let mut effects = Vec::new();
        (
            apply(&mut fixture.app, AgentInput::key(key), &mut effects),
            effects,
        )
    }

    fn custom(name: &str) -> Action {
        Action::Custom(name.to_string())
    }

    /// The agent's input and the keys, each from the same start, end the
    /// same way and ask for the same effects.
    fn same(
        make: Make,
        agent: impl FnOnce(&mut Fixture) -> (InputStatus, Vec<Effect>),
        keys: impl FnOnce(&mut Fixture) -> Vec<Effect>,
    ) -> Fixture {
        let (mut by_agent, mut by_keys) = (make(), make());
        let (status, effects) = agent(&mut by_agent);
        assert_eq!(status, InputStatus::Delivered);
        assert_eq!(effects, keys(&mut by_keys), "the effects differ");
        assert_eq!(state(&by_agent), state(&by_keys));
        by_agent
    }

    /// Refused, and nothing changed.
    fn refused(
        fixture: &mut Fixture,
        input: impl FnOnce(&mut Fixture) -> (InputStatus, Vec<Effect>),
    ) {
        let before = state(fixture);
        let (status, effects) = input(fixture);
        assert_eq!(status, InputStatus::Ignored);
        assert!(effects.is_empty());
        assert_eq!(state(fixture), before);
    }

    /// Refused as the person's, with a status line saying so.
    fn human(
        fixture: &mut Fixture,
        why: &str,
        input: impl FnOnce(&mut Fixture) -> (InputStatus, Vec<Effect>),
    ) {
        let (status, effects) = input(fixture);
        assert_eq!(status, InputStatus::Ignored);
        assert!(effects.is_empty(), "{effects:?}");
        assert_eq!(
            fixture.app.status.as_ref().map(|s| s.text.as_str()),
            Some(why)
        );
    }

    fn home() -> Fixture {
        let mut fixture = mockup();
        press(&mut fixture.app, "\x1b");
        fixture
    }

    fn general() -> Fixture {
        let mut fixture = mockup();
        press(&mut fixture.app, "\n");
        fixture
    }

    fn hotkey() -> Fixture {
        let mut fixture = mockup();
        open_section(&mut fixture.app, "Hotkeys");
        press(&mut fixture.app, "\n");
        fixture
    }

    fn key_entry() -> Fixture {
        let mut fixture = mockup();
        press(&mut fixture.app, "j\nj\n");
        type_text(&mut fixture.app, "dg_typed");
        fixture
    }

    fn profiles() -> Fixture {
        let mut fixture = mockup_profiles(PROFILES, None);
        open_profiles(&mut fixture);
        fixture
    }

    fn profile() -> Fixture {
        let mut fixture = profiles();
        testing::open_profile(&mut fixture, "offline");
        fixture
    }

    fn history() -> Fixture {
        let mut fixture = mockup();
        open_history(
            &mut fixture,
            vec![
                dictation("2026-10-30 14:02:11", "git push origin main"),
                dictation("2026-10-29 09:00:00", "an older one"),
            ],
        );
        fixture
    }

    fn checks() -> Fixture {
        use std::path::PathBuf;

        use whisrs::config::setup::keybind::{Binding, Compositor};
        use whisrs::config::setup::uinput::Uinput;
        use whisrs::config::setup::unit::UnitFile;
        use whisrs::service::ServiceManager;
        let mut fixture = setup_on(Some(testing::MOCKUP), &[]);
        press(&mut fixture.app, "j\n");
        fixture
            .app
            .update(Msg::Checked(Box::new(crate::tui::screens::setup::Checks {
                uinput: Uinput::Denied,
                manager: ServiceManager::Systemd,
                unit: UnitFile::Missing,
                unit_path: Some(PathBuf::from("/u/whisrs.service")),
                enabled: false,
                active: false,
                whisrsd: "/bin/whisrsd".into(),
                whisrs: "/bin/whisrs".into(),
                compositor: Some(Compositor::Hyprland),
                binding: Some(Binding::Unbound(PathBuf::from("/h/hyprland.conf"))),
                gnome: None,
            })));
        fixture
    }

    #[test]
    fn home_opens_a_screen_as_enter_does() {
        let fixture = same(
            home,
            |f| act(f, "home-mic", Action::Activate, None),
            |f| press(&mut f.app, "j\n"),
        );
        assert_eq!(fixture.app.screen(), Screen::Mic);
        same(
            home,
            |f| act(f, "home-history", Action::Select, None),
            |f| press(&mut f.app, "jjjj"),
        );
    }

    #[test]
    fn config_rows_change_as_their_keys_change_them() {
        // A toggle is Space.
        same(
            general,
            |f| act(f, "row-general.notify", Action::Toggle, None),
            |f| {
                to_row(&mut f.app, "notifications");
                press(&mut f.app, " ")
            },
        );
        // A number is typed into its input.
        let fixture = same(
            general,
            |f| {
                act(
                    f,
                    "row-general.silence_timeout_ms",
                    Action::SetValue,
                    Some("3000"),
                )
            },
            |f| {
                to_row(&mut f.app, "silence timeout");
                let mut effects = press(&mut f.app, "\n");
                effects.extend(chord(&mut f.app, KeyCode::Char('u'), KeyModifiers::CONTROL));
                effects.extend(press(&mut f.app, "3000\n"));
                effects
            },
        );
        assert_eq!(
            fixture.app.config.form.config().general.silence_timeout_ms,
            3000
        );
        assert!(fixture.app.config.form.is_dirty());
        // A suggested value is picked in its picker.
        let fixture = same(
            general,
            |f| act(f, "row-general.language", Action::SetValue, Some("de")),
            |f| press(&mut f.app, "\nger\n"),
        );
        assert_eq!(fixture.app.config.form.config().general.language, "de");
        // A value out of range is no value: nothing is typed.
        refused(&mut general(), |f| {
            act(
                f,
                "row-general.silence_timeout_ms",
                Action::SetValue,
                Some("-5"),
            )
        });
        // x is reset.
        same(
            general,
            |f| act(f, "row-general.language", custom(tree::RESET), None),
            |f| press(&mut f.app, "x"),
        );
    }

    #[test]
    fn sections_move_as_j_and_k_do() {
        let fixture = same(
            mockup,
            |f| act(f, "section-audio", Action::Select, None),
            |f| press(&mut f.app, "jj"),
        );
        assert_eq!(
            schema::SECTIONS[fixture.app.config.section()].title,
            "Audio"
        );
        same(
            general,
            |f| act(f, "section-hotkeys", Action::Activate, None),
            |f| press(&mut f.app, "\tjjjjjj\n"),
        );
    }

    #[test]
    fn profile_mode_sets_a_row_as_its_editor_does() {
        let fixture = same(
            profile,
            |f| act(f, "row-general.language", Action::SetValue, Some("fr")),
            |f| press(&mut f.app, "\n\nfr\n"),
        );
        let screen = fixture.app.profile_config.as_ref().unwrap();
        assert_eq!(screen.form.config().general.language, "fr");
        assert!(screen.is_dirty());
    }

    #[test]
    fn the_mic_row_opens_its_picker() {
        let mic = || {
            let mut fixture = home();
            press(&mut fixture.app, "j\n");
            fixture
        };
        same(
            mic,
            |f| act(f, "row-audio.device", Action::Activate, None),
            |f| press(&mut f.app, "\n"),
        );
        same(
            mic,
            |f| act(f, "back", Action::Activate, None),
            |f| press(&mut f.app, "\x1b"),
        );
    }

    #[test]
    fn setup_goes_on_as_its_keys_do() {
        let fresh = || setup_on(None, &[]);
        same(
            fresh,
            |f| act(f, "next", Action::Activate, None),
            |f| press(&mut f.app, "\n"),
        );
        // A row opens with Space on setup, where Enter is the next step.
        same(
            fresh,
            |f| act(f, "row-general.backend", Action::Activate, None),
            |f| press(&mut f.app, " "),
        );
        let fixture = same(
            fresh,
            |f| act(f, "row-general.backend", Action::SetValue, Some("deepgram")),
            |f| press(&mut f.app, " deepgram\n"),
        );
        let setup = fixture.app.setup.as_ref().unwrap();
        assert_eq!(setup.editor.form.config().general.backend, "deepgram");
        assert_eq!(fixture.app.mode(), Mode::Browse);
        let existing = || setup_on(Some(testing::MOCKUP), &[]);
        same(
            existing,
            |f| act(f, "choice-1", Action::Activate, None),
            |f| press(&mut f.app, "j\n"),
        );
        let mut by_agent = checks();
        let (_, effects) = act(&mut by_agent, "check-service", Action::Activate, None);
        assert_eq!(
            effects,
            [Effect::System(
                crate::tui::screens::setup::SystemAction::InstallService
            )]
        );
        same(
            checks,
            |f| act(f, "check-service", Action::Activate, None),
            |f| press(&mut f.app, "j "),
        );
    }

    #[test]
    fn the_hotkey_dialog_moves_toggles_and_takes_a_spec() {
        same(
            hotkey,
            |f| act(f, "modifier-ctrl", Action::Toggle, None),
            |f| press(&mut f.app, "j "),
        );
        let fixture = same(
            hotkey,
            |f| act(f, "hotkey", Action::SetValue, Some("Super+Alt+D")),
            |f| {
                let mut effects = press(&mut f.app, "t");
                effects.extend(chord(&mut f.app, KeyCode::Char('u'), KeyModifiers::CONTROL));
                effects.extend(press(&mut f.app, "Super+Alt+D\n"));
                effects
            },
        );
        let hotkeys = fixture.app.config.form.config().hotkeys.clone().unwrap();
        assert_eq!(hotkeys.toggle.as_deref(), Some("Super+Alt+D"));
        refused(&mut hotkey(), |f| {
            act(f, "hotkey", Action::SetValue, Some("Alt+Nope"))
        });
    }

    #[test]
    fn a_picker_option_is_picked_as_enter_picks_it() {
        let picker = || {
            let mut fixture = mockup();
            press(&mut fixture.app, "\n\n");
            type_text(&mut fixture.app, "ger");
            fixture
        };
        let first = tree::find(&testing::nodes(&picker().app), "picker")
            .unwrap()
            .children
            .iter()
            .find(|n| n.role == taria_ratatui::taria::Role::Option)
            .unwrap()
            .id
            .0
            .clone();
        same(
            picker,
            |f| act(f, &first, Action::Activate, None),
            |f| press(&mut f.app, "\n"),
        );
        same(
            picker,
            |f| act(f, "picker-filter", Action::SetValue, Some("fr")),
            |f| {
                let mut effects = chord(&mut f.app, KeyCode::Char('u'), KeyModifiers::CONTROL)
                    .into_iter()
                    .collect::<Vec<_>>();
                effects.extend(press(&mut f.app, "fr"));
                effects
            },
        );
        same(
            picker,
            |f| act(f, "picker", Action::Dismiss, None),
            |f| press(&mut f.app, "\x1b"),
        );
    }

    #[test]
    fn profiles_switch_open_and_delete_as_their_keys_do() {
        let fixture = same(
            profiles,
            |f| act(f, "profile-work", Action::Activate, None),
            |f| press(&mut f.app, "jjj\n"),
        );
        assert!(fixture.app.profiles.asked.as_deref() == Some("work"));
        let fixture = same(
            profiles,
            |f| act(f, "profile-offline", custom(tree::DELETE), None),
            |f| press(&mut f.app, "jjd"),
        );
        assert_eq!(
            fixture.app.dialog,
            Some(Dialog::DeleteProfile {
                name: "offline".into()
            })
        );
        same(
            profiles,
            |f| act(f, "new", Action::Activate, None),
            |f| press(&mut f.app, "n"),
        );
    }

    #[test]
    fn history_opens_a_dictation_as_enter_does() {
        let fixture = same(
            history,
            |f| act(f, "dict-20261029090000", Action::Activate, None),
            |f| press(&mut f.app, "j\n"),
        );
        assert_eq!(fixture.app.history.opened_at(), Some(1));
        same(
            history,
            |f| act(f, "clear", Action::Activate, None),
            |f| press(&mut f.app, "C"),
        );
    }

    #[cfg(feature = "recording")]
    #[test]
    fn recordings_open_and_act_as_their_keys_do() {
        use crate::tui::screens::recordings::Phase;
        use crate::tui::testing::{open_recordings, recording};
        let list = || {
            let mut fixture = mockup();
            open_recordings(
                &mut fixture,
                vec![
                    recording("2026-10-27_1402", Some("Standup"), Phase::Done),
                    recording("2026-10-26_1015", None, Phase::Failed),
                ],
            );
            fixture
        };
        let fixture = same(
            list,
            |f| act(f, "rec-2", Action::Activate, None),
            |f| press(&mut f.app, "j\n"),
        );
        let (open, _) = fixture.app.recordings.reading().unwrap();
        assert_eq!(open.id, "2026-10-26_1015");
        let fixture = same(
            list,
            |f| act(f, "rec-1", custom(tree::DELETE), None),
            |f| press(&mut f.app, "d"),
        );
        assert!(matches!(
            fixture.app.dialog,
            Some(Dialog::DeleteRecording { .. })
        ));
        same(
            list,
            |f| act(f, "rec-1", custom(tree::NAME), None),
            |f| press(&mut f.app, "n"),
        );
        // A word typed on the list is no command: "deploy" deletes nothing.
        refused(&mut list(), |f| text(f, "deploy"));
        refused(&mut list(), |f| act(f, "rec-9", Action::Activate, None));
    }

    #[test]
    fn a_dialog_is_answered_as_its_letter_answers_it() {
        let unsaved = || {
            let mut fixture = general();
            press(&mut fixture.app, "jj q");
            fixture
        };
        let mut by_agent = unsaved();
        assert_eq!(
            act(&mut by_agent, "dialog-d", Action::Activate, None),
            (InputStatus::Delivered, vec![Effect::Quit])
        );
        let fixture = same(
            unsaved,
            |f| act(f, "dialog-d", Action::Activate, None),
            |f| press(&mut f.app, "d"),
        );
        assert!(!fixture.app.config.form.is_dirty());
        same(
            unsaved,
            |f| act(f, "dialog", Action::Dismiss, None),
            |f| press(&mut f.app, "\x1b"),
        );
        // Behind it, nothing acts.
        refused(&mut unsaved(), |f| {
            act(f, "row-general.notify", Action::Toggle, None)
        });
        refused(&mut unsaved(), |f| act(f, "quit", Action::Activate, None));
        refused(&mut unsaved(), |f| text(f, "wd"));
    }

    #[test]
    fn a_stale_or_unadvertised_act_is_refused_and_changes_nothing() {
        refused(&mut general(), |f| {
            act(f, "row-nope.nope", Action::Activate, None)
        });
        refused(&mut general(), |f| {
            act(f, "row-general.language", Action::Toggle, None)
        });
        refused(&mut general(), |f| {
            act(f, "row-general.language", Action::SetValue, None)
        });
        // Nothing to save: save is disabled.
        refused(&mut general(), |f| act(f, "save", Action::Activate, None));
        // A row of another section is not in the tree.
        refused(&mut general(), |f| {
            act(f, "row-audio.device", Action::Activate, None)
        });
        // A picker's option once the picker is gone.
        let mut fixture = general();
        press(&mut fixture.app, "\n");
        let option = tree::find(&testing::nodes(&fixture.app), "picker")
            .unwrap()
            .children
            .iter()
            .find(|n| n.role == taria_ratatui::taria::Role::Option)
            .unwrap()
            .id
            .0
            .clone();
        press(&mut fixture.app, "\x1b");
        refused(&mut fixture, |f| act(f, &option, Action::Activate, None));
    }

    /// Text lands only where typing goes: an input, a picker's filter.
    #[test]
    fn text_types_only_where_typing_goes() {
        for make in [home, general, hotkey, profiles, history, checks as Make] {
            refused(&mut make(), |f| text(f, "deploy"));
        }
        // Into the filter, the characters one by one; Enter keeps it, and
        // what follows the newline is not typed anywhere.
        let filter = || {
            let mut fixture = mockup();
            press(&mut fixture.app, "/");
            fixture
        };
        let fixture = same(
            filter,
            |f| text(f, "lang\nxd"),
            |f| press(&mut f.app, "lang\n"),
        );
        assert_eq!(fixture.app.config.filter(), "lang");
        // Into a picker's filter.
        let picker = || {
            let mut fixture = mockup();
            press(&mut fixture.app, "\n\n");
            fixture
        };
        same(picker, |f| text(f, "ger"), |f| press(&mut f.app, "ger"));
        // A carriage return is passed over.
        same(filter, |f| text(f, "a\r\n"), |f| press(&mut f.app, "a\n"));
    }

    /// What is the person's at the keyboard is refused with a line saying so.
    #[test]
    fn what_is_the_persons_is_refused_with_a_status_line() {
        // A key's value, by set_value, by text, by a printable key.
        let mut fixture = mockup();
        press(&mut fixture.app, "j\n");
        human(&mut fixture, KEY_IS_HUMAN, |f| {
            act(
                f,
                "row-deepgram.api_key",
                Action::SetValue,
                Some("dg_agent"),
            )
        });
        let mut fixture = key_entry();
        let before = fixture.app.config.typed().unwrap().input.count();
        human(&mut fixture, KEY_IS_HUMAN, |f| text(f, "abc"));
        human(&mut fixture, KEY_IS_HUMAN, |f| raw(f, "a"));
        human(&mut fixture, KEY_IS_HUMAN, |f| raw(f, "backspace"));
        human(&mut fixture, KEY_IS_HUMAN, |f| raw(f, "enter"));
        human(&mut fixture, KEY_IS_HUMAN, |f| {
            act(f, "input", Action::SetValue, Some("x"))
        });
        assert_eq!(fixture.app.config.typed().unwrap().input.count(), before);
        // Esc is the way out, as it is for the person.
        assert_eq!(
            act(&mut fixture, "input", Action::Dismiss, None).0,
            InputStatus::Delivered
        );
        assert!(fixture.app.config.typed().is_none());
        // $EDITOR: e on Config, Home and Profiles, and the answers that lead
        // to it.
        for make in [general, home, profiles as Make] {
            human(&mut make(), EDITOR_IS_HUMAN, |f| raw(f, "e"));
        }
        let mut fixture = general();
        press(&mut fixture.app, "jj e");
        assert_eq!(fixture.app.dialog, Some(Dialog::Unsaved(After::Editor)));
        human(&mut fixture, EDITOR_IS_HUMAN, |f| raw(f, "d"));
        human(&mut fixture, EDITOR_IS_HUMAN, |f| {
            act(f, "dialog-w", Action::Activate, None)
        });
        assert_eq!(fixture.app.dialog, Some(Dialog::Unsaved(After::Editor)));
        // Setup's uinput fix runs sudo.
        let mut fixture = checks();
        human(&mut fixture, SUDO_IS_HUMAN, |f| {
            act(f, "check-uinput", Action::Activate, None)
        });
        human(&mut fixture, SUDO_IS_HUMAN, |f| raw(f, "space"));
    }

    /// A raw key goes through the keyboard's handler; one with no crossterm
    /// counterpart, or an input kind this build cannot read, is refused.
    #[test]
    fn raw_keys_and_unknown_inputs() {
        let mut fixture = general();
        assert_eq!(
            raw(&mut fixture, "ctrl+c"),
            (InputStatus::Delivered, vec![Effect::Quit])
        );
        refused(&mut general(), |f| raw(f, "meta+x"));
        refused(&mut general(), |f| {
            let mut effects = Vec::new();
            (
                apply(&mut f.app, AgentInput::Unknown, &mut effects),
                effects,
            )
        });
        // A key bound to nothing is still pressed.
        assert_eq!(raw(&mut general(), "f5").0, InputStatus::Delivered);
        same(general, |f| raw(f, "j"), |f| press(&mut f.app, "j"));
    }

    /// The ids of the rows of the list open, in order.
    fn item_ids(fixture: &Fixture) -> Vec<String> {
        tree::find(&testing::nodes(&fixture.app), "rows")
            .unwrap()
            .children
            .iter()
            .map(|n| n.id.0.clone())
            .collect()
    }

    fn words(fixture: &Fixture) -> Vec<String> {
        fixture.app.config.form.config().general.vocabulary.clone()
    }

    /// A list item's id is a serial: two deletes from one read delete the
    /// two items read, and the id of one gone is refused.
    #[test]
    fn a_list_item_id_outlives_a_delete_before_it() {
        let mut fixture =
            testing::app("[general]\nvocabulary = [\"PKGBUILD\", \"whisrs\", \"taria\"]\n");
        open_section(&mut fixture.app, "Vocabulary");
        to_row(&mut fixture.app, "vocabulary");
        press(&mut fixture.app, "\n");
        let read = item_ids(&fixture);
        assert_eq!(read.len(), 3);
        let delete = || custom(tree::DELETE);
        assert_eq!(
            act(&mut fixture, &read[0], delete(), None).0,
            InputStatus::Delivered
        );
        assert_eq!(
            act(&mut fixture, &read[1], delete(), None).0,
            InputStatus::Delivered
        );
        assert_eq!(words(&fixture), ["taria"], "the second delete took whisrs");
        refused(&mut fixture, |f| act(f, &read[0], delete(), None));
        refused(&mut fixture, |f| act(f, &read[1], delete(), None));
        // An edit keeps its id; an item added gets one not seen before.
        assert_eq!(
            act(&mut fixture, &read[2], Action::SetValue, Some("taria-mcp")).0,
            InputStatus::Delivered
        );
        assert_eq!(words(&fixture), ["taria-mcp"]);
        assert_eq!(item_ids(&fixture), [read[2].clone()]);
        act(&mut fixture, "add", Action::Activate, None);
        act(&mut fixture, "input", Action::SetValue, Some("ratatui"));
        assert_eq!(words(&fixture), ["taria-mcp", "ratatui"]);
        let now = item_ids(&fixture);
        assert!(!read.contains(&now[1]));
        // Open again: every id new.
        press(&mut fixture.app, "\x1b\n");
        let again = item_ids(&fixture);
        assert!(again
            .iter()
            .all(|id| !now.contains(id) && !read.contains(id)));
    }

    /// The overwrite of a file changed on disk, while a save for `e` was
    /// asked, opens $EDITOR after the save: the person's, as `e` is.
    #[test]
    fn an_overwrite_that_ends_in_the_editor_is_the_persons() {
        use crate::tui::screens::config::SaveError;
        let changed = |e: bool| {
            let mut fixture = general();
            press(&mut fixture.app, "jj ");
            let effects = press(&mut fixture.app, if e { "ew" } else { "w" });
            assert_eq!(effects, [Effect::Save]);
            fixture
                .app
                .update(Msg::Saved(Err(SaveError::Changed("config.toml"))));
            assert_eq!(fixture.app.dialog, Some(Dialog::Changed("config.toml")));
            fixture
        };
        let mut fixture = changed(true);
        let nodes = testing::nodes(&fixture.app);
        assert!(tree::find(&nodes, "dialog-o").unwrap().actions.is_empty());
        assert_eq!(
            tree::find(&nodes, "dialog-l").unwrap().actions,
            [Action::Activate]
        );
        human(&mut fixture, EDITOR_IS_HUMAN, |f| {
            act(f, "dialog-o", Action::Activate, None)
        });
        human(&mut fixture, EDITOR_IS_HUMAN, |f| raw(f, "o"));
        assert_eq!(fixture.app.dialog, Some(Dialog::Changed("config.toml")));
        // Loading it instead opens nothing.
        assert_eq!(
            act(&mut fixture, "dialog-l", Action::Activate, None),
            (InputStatus::Delivered, vec![Effect::Reload])
        );
        // A plain save's overwrite is the agent's to answer.
        let mut fixture = changed(false);
        assert_eq!(
            act(&mut fixture, "dialog-o", Action::Activate, None),
            (InputStatus::Delivered, vec![Effect::SaveOver])
        );
    }

    /// A raw key's modifier its binding does not read is refused: `alt+y`
    /// is no `y` on a dialog, `shift+Down` no Down. A capital, and a chord
    /// with its own meaning, pass.
    #[test]
    fn a_chord_the_binding_ignores_is_refused() {
        let mut fixture = profiles();
        press(&mut fixture.app, "jjd");
        assert!(matches!(
            fixture.app.dialog,
            Some(Dialog::DeleteProfile { .. })
        ));
        refused(&mut fixture, |f| raw(f, "alt+y"));
        // Bound to nothing, so not taken as `y`: pressed, and nothing done.
        assert_eq!(
            raw(&mut fixture, "ctrl+y"),
            (InputStatus::Delivered, Vec::new())
        );
        assert!(matches!(
            fixture.app.dialog,
            Some(Dialog::DeleteProfile { .. })
        ));
        refused(&mut general(), |f| raw(f, "shift+down"));
        refused(&mut general(), |f| raw(f, "alt+j"));
        // Shift on a character is the character.
        same(general, |f| raw(f, "G"), |f| press(&mut f.app, "G"));
        same(
            general,
            |f| raw(f, "shift+tab"),
            |f| {
                chord(&mut f.app, KeyCode::BackTab, KeyModifiers::SHIFT)
                    .into_iter()
                    .collect()
            },
        );
        // A chord that means something of its own.
        assert_eq!(
            raw(&mut general(), "ctrl+c"),
            (InputStatus::Delivered, vec![Effect::Quit])
        );
        let filter = || {
            let mut fixture = mockup();
            press(&mut fixture.app, "/");
            type_text(&mut fixture.app, "lang");
            fixture
        };
        let fixture = same(
            filter,
            |f| raw(f, "ctrl+u"),
            |f| {
                chord(&mut f.app, KeyCode::Char('u'), KeyModifiers::CONTROL)
                    .into_iter()
                    .collect()
            },
        );
        assert_eq!(fixture.app.config.typed().unwrap().input.text(), "");
        // A capital typed by key into an input.
        let fixture = same(filter, |f| raw(f, "A"), |f| press(&mut f.app, "A"));
        assert_eq!(fixture.app.config.typed().unwrap().input.text(), "langA");
        // `shift+a` is the capital, as a terminal delivers it.
        let fixture = same(filter, |f| raw(f, "shift+a"), |f| press(&mut f.app, "A"));
        assert_eq!(fixture.app.config.typed().unwrap().input.text(), "langA");
        // Shift on a key with no shifted character reaches the plain one.
        refused(&mut filter(), |f| raw(f, "shift+1"));
        refused(&mut general(), |f| raw(f, "shift+1"));
        // A shifted symbol is its own character.
        let fixture = same(general, |f| raw(f, "shift+?"), |f| press(&mut f.app, "?"));
        assert!(matches!(fixture.app.dialog, Some(Dialog::Help(_))));
        // A capital bound as its own: `shift+g` is `G`.
        let fixture = same(general, |f| raw(f, "shift+g"), |f| press(&mut f.app, "G"));
        assert_ne!(fixture.app.config.cursor(), 0);
    }

    /// A dialog's answer is its letter alone: the letter with Shift, or its
    /// capital, answers nothing, though a person's capital does (a dialog
    /// takes its letters in any case).
    #[test]
    fn a_shifted_letter_answers_no_dialog() {
        let delete = || {
            let mut fixture = profiles();
            press(&mut fixture.app, "jjd");
            assert!(matches!(
                fixture.app.dialog,
                Some(Dialog::DeleteProfile { .. })
            ));
            fixture
        };
        for key in ["shift+y", "Y", "shift+n", "N"] {
            refused(&mut delete(), |f| raw(f, key));
        }
        // The plain letter answers it.
        assert_eq!(
            raw(&mut delete(), "y"),
            (
                InputStatus::Delivered,
                vec![Effect::DeleteProfile("offline".into())]
            )
        );
        let unsaved = || {
            let mut fixture = general();
            press(&mut fixture.app, "jj q");
            assert_eq!(fixture.app.dialog, Some(Dialog::Unsaved(After::Quit)));
            fixture
        };
        refused(&mut unsaved(), |f| raw(f, "shift+d"));
        refused(&mut unsaved(), |f| raw(f, "shift+w"));
        assert_eq!(
            raw(&mut unsaved(), "d"),
            (InputStatus::Delivered, vec![Effect::Quit])
        );
    }

    /// On Recordings: `shift+y` on the delete question deletes nothing, and
    /// `shift+s` summarizes as `S` does, S being bound as a capital.
    #[cfg(feature = "recording")]
    #[test]
    fn shift_on_recordings_does_what_the_capital_does() {
        use crate::tui::screens::recordings::Phase;
        use crate::tui::testing::{open_recordings, recording};
        let list = || {
            let mut fixture = mockup();
            open_recordings(
                &mut fixture,
                vec![recording("2026-10-27_1402", Some("Standup"), Phase::Done)],
            );
            fixture
        };
        let mut fixture = list();
        press(&mut fixture.app, "d");
        assert!(matches!(
            fixture.app.dialog,
            Some(Dialog::DeleteRecording { .. })
        ));
        for key in ["shift+y", "Y", "alt+y"] {
            refused(&mut fixture, |f| raw(f, key));
        }
        assert_eq!(fixture.app.recordings.entries().len(), 1);
        assert!(!fixture.app.recordings.is_busy("2026-10-27_1402"));
        let fixture = same(list, |f| raw(f, "shift+s"), |f| press(&mut f.app, "S"));
        assert!(fixture.app.recordings.is_busy("2026-10-27_1402"));
    }

    /// A value or a text over the cap is refused whole, before anything is
    /// typed.
    #[test]
    fn a_value_over_the_cap_is_refused() {
        let long = "a".repeat(MAX_TYPED + 1);
        let prompt = || {
            let mut fixture = mockup();
            open_section(&mut fixture.app, "Vocabulary");
            fixture
        };
        refused(&mut prompt(), |f| {
            act(f, "row-general.prompt", Action::SetValue, Some(&long))
        });
        // The row takes a value under it.
        let mut fixture = prompt();
        let (status, _) = act(
            &mut fixture,
            "row-general.prompt",
            Action::SetValue,
            Some("short"),
        );
        assert_eq!(status, InputStatus::Delivered);
        assert_eq!(
            fixture.app.config.form.config().general.prompt.as_deref(),
            Some("short")
        );
        let mut fixture = mockup();
        press(&mut fixture.app, "/");
        refused(&mut fixture, |f| text(f, &long));
        let most = "a".repeat(MAX_TYPED);
        assert_eq!(text(&mut fixture, &most).0, InputStatus::Delivered);
        assert_eq!(fixture.app.config.typed().unwrap().input.count(), MAX_TYPED);
    }

    /// A value that would not land is refused before an editor opens, and
    /// the state stays as it was: no picker or input left open.
    #[test]
    fn a_value_that_cannot_land_opens_nothing() {
        // A listed value in another case: the picker would pick that one.
        refused(&mut general(), |f| {
            act(f, "row-general.language", Action::SetValue, Some("DE"))
        });
        // Not a language.
        refused(&mut general(), |f| {
            act(f, "row-general.language", Action::SetValue, Some("zz9q"))
        });
        // A backend this build refuses.
        let mut keys = mockup();
        open_section(&mut keys.app, "Backend & keys");
        refused(&mut keys, |f| {
            act(
                f,
                "row-general.backend",
                Action::SetValue,
                Some("local-vosk"),
            )
        });
        // A path is taken however it is typed, as a person can type one.
        let fixture = same(
            || {
                let mut fixture = mockup();
                open_section(&mut fixture.app, "Backend & keys");
                fixture
            },
            |f| {
                act(
                    f,
                    "row-local-whisper.model_path",
                    Action::SetValue,
                    Some("/models/ggml-tiny.bin"),
                )
            },
            |f| {
                let mut effects = to_row(&mut f.app, "whisper model");
                effects.extend(press(&mut f.app, "\n"));
                effects.extend(press(&mut f.app, "/models/ggml-tiny.bin\n"));
                effects
            },
        );
        let path = fixture
            .app
            .config
            .form
            .config()
            .local_whisper
            .as_ref()
            .map(|l| l.model_path.clone());
        assert_eq!(path.as_deref(), Some("/models/ggml-tiny.bin"));
        assert_eq!(fixture.app.mode(), Mode::Browse);
    }

    /// Recordings and History publish the rows drawn; scroll pages through
    /// the rest as PgUp and PgDn do.
    #[test]
    fn a_long_list_scrolls_as_its_page_keys_do() {
        let history = || {
            let mut fixture = mockup();
            let entries = (0..25)
                .map(|i| dictation(&format!("2026-10-29 09:{i:02}:00"), "a dictation"))
                .collect();
            open_history(&mut fixture, entries);
            fixture
        };
        let fixture = same(
            history,
            |f| act(f, "dictations", Action::Scroll, Some("down")),
            |f| {
                testing::key(&mut f.app, KeyCode::PageDown)
                    .into_iter()
                    .collect()
            },
        );
        assert_eq!(fixture.app.history.cursor(), 10);
        same(
            history,
            |f| act(f, "dictations", Action::Scroll, Some("up")),
            |f| {
                testing::key(&mut f.app, KeyCode::PageUp)
                    .into_iter()
                    .collect()
            },
        );
        refused(&mut history(), |f| {
            act(f, "dictations", Action::Scroll, Some("sideways"))
        });
        #[cfg(feature = "recording")]
        {
            use crate::tui::screens::recordings::Phase;
            use crate::tui::testing::{open_recordings, recording};
            let list = || {
                let mut fixture = mockup();
                let items = (0..25)
                    .map(|i| recording(&format!("2026-10-{i:02}_1402"), None, Phase::Failed))
                    .collect();
                open_recordings(&mut fixture, items);
                fixture
            };
            let fixture = same(
                list,
                |f| act(f, "recordings", Action::Scroll, Some("down")),
                |f| {
                    testing::key(&mut f.app, KeyCode::PageDown)
                        .into_iter()
                        .collect()
                },
            );
            assert_eq!(fixture.app.recordings.cursor(), 10);
        }
    }
}
