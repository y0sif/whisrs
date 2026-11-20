//! What the agent overlay shows: one row per active agent, composed from
//! what the runtime reports.
//!
//! [`Board`] keeps, per configured agent, its running turn (with the
//! activity line and whether it waits for you), how its last turn ended,
//! and whether a reply of its is queued or playing; plus the one agent the
//! voice belongs to right now (listening, transcribing or speaking).
//! [`Board::compose`] turns that into rows, in config order, each in the
//! state that wins: voice, then needs-you, then working, then done or error.
//!
//! A finished turn's Done or Error row holds while that agent's reply is
//! queued or playing, then for [`DONE_HOLD`], then fades over
//! [`DONE_FADE`] and is gone. The overlay has room for [`MAX_ROWS`]
//! ([`rows`] splits a composition into what fits and a "+N more" count).
//!
//! Pure, no clock: every time is passed in, so a test (or a renderer
//! animating a fade between two compositions) decides what "now" is.

use std::time::{Duration, Instant};

use crate::State;

/// How long a Done or Error row stays fully visible once its reply played.
pub const DONE_HOLD: Duration = Duration::from_millis(2500);

/// How long it then takes to fade out.
pub const DONE_FADE: Duration = Duration::from_millis(600);

/// The rows the overlay has room for. A composition with more shows
/// `MAX_ROWS - 1` agents and a "+N more" row ([`rows`]).
pub const MAX_ROWS: usize = 4;

/// The activity line of a turn that waits for you.
pub const NEEDS_YOU_ACTIVITY: &str = "Waiting for your approval";

/// What an agent's row shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentVisual {
    /// Its voice turn is recording.
    Listening,
    /// Its voice turn is being transcribed.
    Transcribing,
    /// Its turn runs.
    Working,
    /// Its reply, acknowledgement or notice is synthesized or played.
    Speaking,
    /// Its turn (or its session's own work) waits for a permission prompt.
    NeedsYou,
    /// Its last turn ended with a reply.
    Done,
    /// Its last turn failed.
    Error,
}

impl AgentVisual {
    /// The visual of the agent the dictation state belongs to: recording is
    /// listening, synthesizing shows as speaking. `None` for idle.
    pub fn from_state(state: State) -> Option<AgentVisual> {
        match state {
            State::Idle => None,
            State::Recording => Some(AgentVisual::Listening),
            State::Transcribing => Some(AgentVisual::Transcribing),
            State::Synthesizing | State::Speaking => Some(AgentVisual::Speaking),
        }
    }

    /// Wire name (the GNOME broadcast, logs).
    pub fn as_str(self) -> &'static str {
        match self {
            AgentVisual::Listening => "listening",
            AgentVisual::Transcribing => "transcribing",
            AgentVisual::Working => "working",
            AgentVisual::Speaking => "speaking",
            AgentVisual::NeedsYou => "needs-you",
            AgentVisual::Done => "done",
            AgentVisual::Error => "error",
        }
    }

    /// The state word on the card.
    pub fn word(self) -> &'static str {
        match self {
            AgentVisual::NeedsYou => "needs you",
            other => other.as_str(),
        }
    }
}

/// One row of the agent overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentOverlayEntry {
    pub name: String,
    pub visual: AgentVisual,
    /// The card's second line: what the turn does ("Running a command"),
    /// "Waiting for your approval", or "Finished in m:ss".
    pub activity: Option<String>,
    /// What the card's timer counts from: the turn's start while it runs
    /// (working, waiting for you, or speaking an acknowledgement) or has
    /// ended; the voice state's start while listening or transcribing, and
    /// while speaking with no turn running.
    pub since: Instant,
    /// When a Done or Error row starts fading (it can be later than the
    /// composition's `now`); [`DONE_FADE`] after it, the row is gone. `None`
    /// while it holds, and for every other visual.
    pub fading_from: Option<Instant>,
}

impl AgentOverlayEntry {
    /// The row's opacity at `now`: 1 until it fades, then down to 0 over
    /// [`DONE_FADE`].
    pub fn alpha(&self, now: Instant) -> f32 {
        let Some(from) = self.fading_from else {
            return 1.0;
        };
        let faded = now.saturating_duration_since(from).as_secs_f32();
        (1.0 - faded / DONE_FADE.as_secs_f32()).clamp(0.0, 1.0)
    }
}

/// What of a composition fits the overlay: the rows to draw, and how many
/// agents a "+N more" row stands for after them (0: no such row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rows<'a> {
    pub shown: &'a [AgentOverlayEntry],
    pub more: usize,
}

/// Split a composition into what fits: every row when there are at most
/// [`MAX_ROWS`], else the first `MAX_ROWS - 1` (config order) and a count
/// for the "+N more" row in the last slot.
pub fn rows(entries: &[AgentOverlayEntry]) -> Rows<'_> {
    if entries.len() <= MAX_ROWS {
        return Rows {
            shown: entries,
            more: 0,
        };
    }
    let shown = &entries[..MAX_ROWS - 1];
    Rows {
        shown,
        more: entries.len() - shown.len(),
    }
}

/// The "+N more" row's text.
pub fn more_label(more: usize) -> String {
    format!("+{more} more")
}

/// A duration as the card's timer reads it: `m:ss`, `h:mm:ss` from an hour.
pub fn clock(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// A running turn.
#[derive(Debug, Clone)]
struct Turn {
    since: Instant,
    activity: Option<String>,
}

/// How the last turn ended.
#[derive(Debug, Clone, Copy)]
struct Ended {
    ok: bool,
    /// When the turn started, and when it ended.
    since: Instant,
    at: Instant,
}

#[derive(Debug, Clone)]
struct Slot {
    name: String,
    turn: Option<Turn>,
    /// Since when the turn, or the session's own work with no turn running,
    /// waits for you.
    needs_you: Option<Instant>,
    ended: Option<Ended>,
    /// A reply (or acknowledgement, or notice) of this agent is queued or
    /// playing.
    pending: bool,
    /// When `pending` last went false.
    pending_cleared: Option<Instant>,
}

/// The agent the voice belongs to.
#[derive(Debug, Clone)]
struct Voice {
    name: String,
    visual: AgentVisual,
    since: Instant,
}

/// The agent overlay's model. Every method takes the agent's name; a name
/// that is not configured is ignored.
#[derive(Debug, Clone, Default)]
pub struct Board {
    /// Config order.
    slots: Vec<Slot>,
    voice: Option<Voice>,
}

impl Board {
    /// A board for the configured agents, in config order.
    pub fn new<S: Into<String>>(names: impl IntoIterator<Item = S>) -> Self {
        Self {
            slots: names
                .into_iter()
                .map(|name| Slot {
                    name: name.into(),
                    turn: None,
                    needs_you: None,
                    ended: None,
                    pending: false,
                    pending_cleared: None,
                })
                .collect(),
            voice: None,
        }
    }

    fn slot(&mut self, name: &str) -> Option<&mut Slot> {
        self.slots.iter_mut().find(|s| s.name == name)
    }

    /// The agent the voice belongs to now and what it does
    /// ([`AgentVisual::from_state`]), or `None`. Its own timer restarts when
    /// either changes, not for the same state again; speaking during a turn
    /// shows the turn's.
    pub fn set_voice(&mut self, voice: Option<(&str, AgentVisual)>, now: Instant) {
        self.voice = match voice {
            None => None,
            Some((name, visual)) => match &self.voice {
                Some(v) if v.name == name && v.visual == visual => return,
                _ => Some(Voice {
                    name: name.to_string(),
                    visual,
                    since: now,
                }),
            },
        };
    }

    /// A turn started at `since` (for a turn watched again after a restart,
    /// when it first started). Clears the last turn's end and any wait.
    pub fn turn_started(&mut self, name: &str, since: Instant) {
        if let Some(slot) = self.slot(name) {
            slot.turn = Some(Turn {
                since,
                activity: None,
            });
            slot.needs_you = None;
            slot.ended = None;
        }
    }

    /// What the running turn does now. Ignored with no turn running.
    pub fn activity(&mut self, name: &str, activity: &str) {
        if let Some(turn) = self.slot(name).and_then(|s| s.turn.as_mut()) {
            turn.activity = Some(activity.to_string());
        }
    }

    /// The turn, or with none running the session's own work, waits for a
    /// permission prompt.
    pub fn needs_you(&mut self, name: &str, now: Instant) {
        if let Some(slot) = self.slot(name) {
            slot.needs_you.get_or_insert(now);
        }
    }

    /// The turn runs again after a wait.
    pub fn working(&mut self, name: &str) {
        if let Some(slot) = self.slot(name) {
            slot.needs_you = None;
        }
    }

    /// The turn ended at `now`: with a reply (`ok`) or an error. With no
    /// turn running, a wait shown for the session's own work ends the same
    /// way (a `Stop` with a reply ended it), timed from the wait's start, so
    /// it ends Done whether or not that work showed as working first.
    /// Ignored with neither.
    pub fn turn_ended(&mut self, name: &str, ok: bool, now: Instant) {
        if let Some(slot) = self.slot(name) {
            let since = slot.turn.take().map(|turn| turn.since).or(slot.needs_you);
            if let Some(since) = since {
                slot.ended = Some(Ended { ok, since, at: now });
            }
            slot.needs_you = None;
        }
    }

    /// The agents with an utterance queued or playing, as of `now`. A Done
    /// or Error row holds while its agent is among them.
    pub fn set_pending<S: AsRef<str>>(&mut self, names: &[S], now: Instant) {
        for slot in &mut self.slots {
            let pending = names.iter().any(|n| n.as_ref() == slot.name);
            if slot.pending && !pending {
                slot.pending_cleared = Some(now);
            }
            slot.pending = pending;
        }
    }

    /// Forget the agent's turn, wait and last end (`--stop`, `--reset`, the
    /// end of its session's own work or wait): its row goes at once, unless
    /// the voice is the agent's (a reply playing, a recording), which shows
    /// until [`Board::set_voice`] says otherwise.
    pub fn clear(&mut self, name: &str) {
        if let Some(slot) = self.slot(name) {
            slot.turn = None;
            slot.needs_you = None;
            slot.ended = None;
        }
    }

    /// The rows at `now`, in config order: every agent with something to
    /// show, in the state that wins (voice > needs-you > working > done or
    /// error). Uncapped: [`rows`] decides what fits.
    pub fn compose(&self, now: Instant) -> Vec<AgentOverlayEntry> {
        self.slots
            .iter()
            .filter_map(|slot| self.entry(slot, now))
            .collect()
    }

    fn entry(&self, slot: &Slot, now: Instant) -> Option<AgentOverlayEntry> {
        let row = |visual, activity: Option<String>, since| AgentOverlayEntry {
            name: slot.name.clone(),
            visual,
            activity,
            since,
            fading_from: None,
        };
        if let Some(voice) = self.voice.as_ref().filter(|v| v.name == slot.name) {
            let activity = slot.turn.as_ref().and_then(|t| t.activity.clone());
            // An acknowledgement mid-turn keeps the turn's timer; a press
            // (listening, transcribing) times itself.
            let since = match (&slot.turn, voice.visual) {
                (Some(turn), AgentVisual::Speaking) => turn.since,
                _ => voice.since,
            };
            return Some(row(voice.visual, activity, since));
        }
        if let Some(waiting) = slot.needs_you {
            let since = slot.turn.as_ref().map_or(waiting, |t| t.since);
            return Some(row(
                AgentVisual::NeedsYou,
                Some(NEEDS_YOU_ACTIVITY.to_string()),
                since,
            ));
        }
        if let Some(turn) = &slot.turn {
            return Some(row(AgentVisual::Working, turn.activity.clone(), turn.since));
        }
        let ended = slot.ended?;
        let fading_from = (!slot.pending).then(|| fade_start(ended, slot.pending_cleared));
        if fading_from.is_some_and(|from| now >= from + DONE_FADE) {
            return None;
        }
        let took = clock(ended.at.saturating_duration_since(ended.since));
        let (visual, activity) = if ended.ok {
            (AgentVisual::Done, format!("Finished in {took}"))
        } else {
            (AgentVisual::Error, format!("Failed after {took}"))
        };
        Some(AgentOverlayEntry {
            fading_from,
            ..row(visual, Some(activity), ended.since)
        })
    }

    /// When the composition next changes with time alone (a fading row is
    /// gone), for a caller that recomposes on a timer. `None` when nothing
    /// fades.
    pub fn next_change(&self, now: Instant) -> Option<Instant> {
        self.compose(now)
            .iter()
            .filter_map(|e| e.fading_from)
            .map(|from| from + DONE_FADE)
            .min()
    }
}

/// When a Done or Error row starts fading once nothing of its agent is
/// pending: [`DONE_HOLD`] after the later of the turn's end and the last
/// utterance finishing.
fn fade_start(ended: Ended, pending_cleared: Option<Instant>) -> Instant {
    let released = pending_cleared.map_or(ended.at, |cleared| cleared.max(ended.at));
    released + DONE_HOLD
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    fn visuals(board: &Board, now: Instant) -> Vec<(String, AgentVisual)> {
        board
            .compose(now)
            .into_iter()
            .map(|e| (e.name, e.visual))
            .collect()
    }

    fn one(board: &Board, now: Instant) -> AgentOverlayEntry {
        let mut entries = board.compose(now);
        assert_eq!(entries.len(), 1, "{entries:?}");
        entries.remove(0)
    }

    #[test]
    fn rows_come_in_config_order() {
        let t0 = Instant::now();
        let mut board = Board::new(["a", "b", "c"]);
        board.turn_started("c", t0);
        board.turn_started("a", t0 + secs(1.0));
        board.turn_started("nobody", t0);
        assert_eq!(
            visuals(&board, t0 + secs(2.0)),
            [
                ("a".to_string(), AgentVisual::Working),
                ("c".to_string(), AgentVisual::Working)
            ]
        );
    }

    #[test]
    fn voice_beats_needs_you_beats_working() {
        let t0 = Instant::now();
        let mut board = Board::new(["jarvis"]);
        board.turn_started("jarvis", t0);
        board.activity("jarvis", "Running a command");
        let e = one(&board, t0);
        assert_eq!(e.visual, AgentVisual::Working);
        assert_eq!(e.activity.as_deref(), Some("Running a command"));
        assert_eq!(e.since, t0);

        board.needs_you("jarvis", t0 + secs(5.0));
        let e = one(&board, t0 + secs(6.0));
        assert_eq!(e.visual, AgentVisual::NeedsYou);
        assert_eq!(e.activity.as_deref(), Some(NEEDS_YOU_ACTIVITY));
        // The timer is the turn's.
        assert_eq!(e.since, t0);

        // Its acknowledgement plays: the voice wins, the activity and the
        // turn's timer stay.
        board.set_voice(Some(("jarvis", AgentVisual::Speaking)), t0 + secs(7.0));
        let e = one(&board, t0 + secs(7.5));
        assert_eq!(e.visual, AgentVisual::Speaking);
        assert_eq!(e.activity.as_deref(), Some("Running a command"));
        assert_eq!(e.since, t0);

        board.set_voice(None, t0 + secs(8.0));
        assert_eq!(one(&board, t0 + secs(8.0)).visual, AgentVisual::NeedsYou);
        board.working("jarvis");
        assert_eq!(one(&board, t0 + secs(9.0)).visual, AgentVisual::Working);
    }

    #[test]
    fn the_voice_timer_restarts_only_on_a_change() {
        let t0 = Instant::now();
        let mut board = Board::new(["jarvis", "friday"]);
        board.set_voice(Some(("jarvis", AgentVisual::Listening)), t0);
        board.set_voice(Some(("jarvis", AgentVisual::Listening)), t0 + secs(3.0));
        let e = one(&board, t0 + secs(4.0));
        assert_eq!((e.visual, e.since), (AgentVisual::Listening, t0));
        board.set_voice(Some(("jarvis", AgentVisual::Transcribing)), t0 + secs(5.0));
        assert_eq!(one(&board, t0 + secs(5.0)).since, t0 + secs(5.0));
        // Another agent's voice: only that one shows.
        board.set_voice(Some(("friday", AgentVisual::Speaking)), t0 + secs(6.0));
        assert_eq!(
            visuals(&board, t0 + secs(6.0)),
            [("friday".to_string(), AgentVisual::Speaking)]
        );
    }

    /// During a turn the timer is the turn's in every state but a press's
    /// own: listening and transcribing count from when they started. With
    /// no turn running, speaking times itself too.
    #[test]
    fn the_timer_follows_the_turn_through_voice_states() {
        let t0 = Instant::now();
        let at = |s: f64| t0 + secs(s);
        let mut board = Board::new(["jarvis"]);
        board.turn_started("jarvis", at(4.0));
        let timer = |board: &Board, now: f64| {
            let e = one(board, at(now));
            (e.visual, clock(at(now).saturating_duration_since(e.since)))
        };
        assert_eq!(timer(&board, 20.0), (AgentVisual::Working, "0:16".into()));
        // The acknowledgement and a needs-you notice play mid-turn: the
        // clock runs on.
        board.set_voice(Some(("jarvis", AgentVisual::Speaking)), at(21.0));
        assert_eq!(timer(&board, 21.0), (AgentVisual::Speaking, "0:17".into()));
        board.set_voice(None, at(23.0));
        board.needs_you("jarvis", at(30.0));
        assert_eq!(timer(&board, 31.0), (AgentVisual::NeedsYou, "0:27".into()));
        board.set_voice(Some(("jarvis", AgentVisual::Speaking)), at(32.0));
        assert_eq!(timer(&board, 33.0), (AgentVisual::Speaking, "0:29".into()));
        board.set_voice(None, at(35.0));
        // The answer is recorded: its own clock.
        board.set_voice(Some(("jarvis", AgentVisual::Listening)), at(40.0));
        assert_eq!(timer(&board, 42.0), (AgentVisual::Listening, "0:02".into()));
        board.set_voice(None, at(44.0));
        board.working("jarvis");
        assert_eq!(timer(&board, 50.0), (AgentVisual::Working, "0:46".into()));
        // The reply plays after the turn ended: the voice's own clock.
        board.turn_ended("jarvis", true, at(60.0));
        board.set_voice(Some(("jarvis", AgentVisual::Speaking)), at(61.0));
        assert_eq!(timer(&board, 63.0), (AgentVisual::Speaking, "0:02".into()));
    }

    /// `--stop` or `--reset` while the agent's reply plays (or its voice
    /// turn records) clears its turn, not its voice: the row shows until the
    /// voice moves on.
    #[test]
    fn a_clear_keeps_the_agents_voice() {
        let t0 = Instant::now();
        let mut board = Board::new(["jarvis"]);
        board.turn_started("jarvis", t0);
        board.set_voice(Some(("jarvis", AgentVisual::Speaking)), t0);
        board.clear("jarvis");
        let e = one(&board, t0 + secs(1.0));
        assert_eq!(
            (e.visual, e.activity, e.since),
            (AgentVisual::Speaking, None, t0)
        );
        board.set_voice(None, t0 + secs(2.0));
        assert!(board.compose(t0 + secs(2.0)).is_empty());
        board.set_voice(Some(("jarvis", AgentVisual::Listening)), t0 + secs(3.0));
        board.clear("jarvis");
        assert_eq!(one(&board, t0 + secs(4.0)).visual, AgentVisual::Listening);
    }

    #[test]
    fn idle_needs_you_shows_without_a_turn() {
        let t0 = Instant::now();
        let mut board = Board::new(["jarvis"]);
        board.needs_you("jarvis", t0);
        // A second notice keeps the first wait's start.
        board.needs_you("jarvis", t0 + secs(4.0));
        let e = one(&board, t0 + secs(5.0));
        assert_eq!((e.visual, e.since), (AgentVisual::NeedsYou, t0));
        board.clear("jarvis");
        assert!(board.compose(t0 + secs(5.0)).is_empty());
    }

    /// A wait shown with no turn running, ended by a reply, ends Done like
    /// the same work shown working first: held while the reply is pending,
    /// timed from the wait. An end with neither a turn nor a wait shows
    /// nothing.
    #[test]
    fn an_idle_wait_that_replies_ends_done() {
        let t0 = Instant::now();
        let at = |s: f64| t0 + secs(s);
        let mut board = Board::new(["jarvis", "friday"]);
        board.needs_you("jarvis", t0);
        board.turn_started("friday", t0);
        board.needs_you("friday", t0);
        for name in ["jarvis", "friday"] {
            board.turn_ended(name, true, at(5.0));
        }
        board.set_pending(&["jarvis", "friday"], at(5.0));
        let rows = board.compose(at(30.0));
        assert_eq!(rows.len(), 2, "{rows:?}");
        for e in &rows {
            assert_eq!(e.visual, AgentVisual::Done, "{e:?}");
            assert_eq!(e.activity.as_deref(), Some("Finished in 0:05"));
            assert_eq!(e.fading_from, None, "faded while its reply waits");
        }

        let mut board = Board::new(["jarvis"]);
        board.turn_ended("jarvis", true, t0);
        assert!(board.compose(t0).is_empty());
    }

    #[test]
    fn done_holds_while_its_reply_is_pending_then_fades() {
        let t0 = Instant::now();
        let at = |s: f64| t0 + secs(s);
        let mut board = Board::new(["jarvis"]);
        board.turn_started("jarvis", t0);
        board.turn_ended("jarvis", true, at(65.0));
        board.set_pending(&["jarvis"], at(65.0));
        // Queued for a long while (a recording holds it): no fade.
        let e = one(&board, at(200.0));
        assert_eq!(e.visual, AgentVisual::Done);
        assert_eq!(e.activity.as_deref(), Some("Finished in 1:05"));
        assert_eq!(e.fading_from, None);
        assert_eq!(e.alpha(at(200.0)), 1.0);
        assert_eq!(board.next_change(at(200.0)), None);
        // It plays: the voice shows.
        board.set_voice(Some(("jarvis", AgentVisual::Speaking)), at(201.0));
        assert_eq!(one(&board, at(202.0)).visual, AgentVisual::Speaking);
        // It played: Done again, holding for DONE_HOLD, then fading.
        board.set_voice(None, at(205.0));
        board.set_pending::<&str>(&[], at(205.0));
        let e = one(&board, at(205.0));
        assert_eq!(e.visual, AgentVisual::Done);
        let from = at(205.0) + DONE_HOLD;
        assert_eq!(e.fading_from, Some(from));
        assert_eq!(e.alpha(at(206.0)), 1.0);
        let half = e.alpha(from + DONE_FADE / 2);
        assert!((half - 0.5).abs() < 0.01, "{half}");
        assert_eq!(board.next_change(at(206.0)), Some(from + DONE_FADE));
        // Still there a moment before the fade ends, gone at its end.
        assert_eq!(
            board.compose(from + DONE_FADE - secs(0.01)).len(),
            1,
            "fading"
        );
        assert!(board.compose(from + DONE_FADE).is_empty());
        assert!(board.compose(from + secs(60.0)).is_empty());
    }

    #[test]
    fn done_with_nothing_pending_fades_from_the_turn_end() {
        let t0 = Instant::now();
        let mut board = Board::new(["jarvis"]);
        // An earlier utterance finished before the turn ended.
        board.set_pending(&["jarvis"], t0);
        board.turn_started("jarvis", t0);
        board.set_pending::<String>(&[], t0 + secs(3.0));
        board.turn_ended("jarvis", false, t0 + secs(10.0));
        let e = one(&board, t0 + secs(10.0));
        assert_eq!(e.visual, AgentVisual::Error);
        assert_eq!(e.activity.as_deref(), Some("Failed after 0:10"));
        assert_eq!(e.fading_from, Some(t0 + secs(10.0) + DONE_HOLD));
        assert!(board
            .compose(t0 + secs(10.0) + DONE_HOLD + DONE_FADE)
            .is_empty());
    }

    #[test]
    fn a_new_turn_or_a_clear_replaces_the_last_end() {
        let t0 = Instant::now();
        let mut board = Board::new(["jarvis"]);
        board.turn_started("jarvis", t0);
        board.needs_you("jarvis", t0);
        board.turn_ended("jarvis", true, t0 + secs(1.0));
        assert_eq!(one(&board, t0 + secs(1.0)).visual, AgentVisual::Done);
        board.turn_started("jarvis", t0 + secs(2.0));
        let e = one(&board, t0 + secs(2.0));
        assert_eq!((e.visual, e.activity), (AgentVisual::Working, None));
        board.activity("jarvis", "Reading files");
        board.clear("jarvis");
        assert!(board.compose(t0 + secs(3.0)).is_empty());
        // An activity with no turn running shows nothing.
        board.activity("jarvis", "Reading files");
        assert!(board.compose(t0 + secs(3.0)).is_empty());
    }

    #[test]
    fn a_fifth_agent_collapses_into_more() {
        let t0 = Instant::now();
        let names = ["a", "b", "c", "d", "e", "f"];
        let mut board = Board::new(names);
        for name in &names[..4] {
            board.turn_started(name, t0);
        }
        let entries = board.compose(t0);
        let fit = rows(&entries);
        assert_eq!((fit.shown.len(), fit.more), (4, 0));
        for name in &names[4..] {
            board.turn_started(name, t0);
        }
        let entries = board.compose(t0);
        assert_eq!(entries.len(), 6);
        let fit = rows(&entries);
        assert_eq!(fit.shown.len(), MAX_ROWS - 1);
        assert_eq!(fit.shown[2].name, "c");
        assert_eq!(fit.more, 3);
        assert_eq!(more_label(fit.more), "+3 more");
    }

    #[test]
    fn visuals_map_from_the_dictation_state() {
        assert_eq!(AgentVisual::from_state(State::Idle), None);
        assert_eq!(
            AgentVisual::from_state(State::Recording),
            Some(AgentVisual::Listening)
        );
        assert_eq!(
            AgentVisual::from_state(State::Transcribing),
            Some(AgentVisual::Transcribing)
        );
        assert_eq!(
            AgentVisual::from_state(State::Synthesizing),
            Some(AgentVisual::Speaking)
        );
        assert_eq!(
            AgentVisual::from_state(State::Speaking),
            Some(AgentVisual::Speaking)
        );
        assert_eq!(AgentVisual::NeedsYou.as_str(), "needs-you");
        assert_eq!(AgentVisual::NeedsYou.word(), "needs you");
        assert_eq!(AgentVisual::Working.word(), "working");
    }

    #[test]
    fn the_clock_reads_minutes_and_seconds() {
        assert_eq!(clock(Duration::ZERO), "0:00");
        assert_eq!(clock(secs(9.9)), "0:09");
        assert_eq!(clock(secs(65.0)), "1:05");
        assert_eq!(clock(secs(600.0)), "10:00");
        assert_eq!(clock(secs(3723.0)), "1:02:03");
    }
}
