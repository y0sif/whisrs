//! Config profiles in the daemon: the profile it runs on, and
//! `switch-profile`, which checks the target loads, writes the active name
//! and restarts the daemon on it. While a switch is under way, whatever would
//! start work the restart cuts off is refused ([`switching_refusal`]) and
//! agent replies wait.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tracing::{error, info, warn};

use whisrs::agent::AgentAction;
use whisrs::config::profile::{self, Check, Paths, BASE};
use whisrs::recording::RecordingAction;
use whisrs::{Command, Response, State};

use crate::context::{DaemonContext, DaemonState};
use crate::notify::send_notification;

/// Restarts the daemon given its context: called once, after a switch's
/// reply is out, with what to do when nothing restarted. Injected so tests
/// never restart.
pub(crate) type Restarter =
    Arc<dyn Fn(Arc<DaemonContext>, Box<dyn FnOnce(String) + Send>) + Send + Sync>;

/// How long a switch waits before the restart, so its reply is out.
const REPLY_GRACE: Duration = Duration::from_millis(200);

/// What a start gets while a switch is under way.
pub(crate) const SWITCHING: &str = "switching profile: try again in a moment";

/// What a second switch gets while one is under way.
const ALREADY_SWITCHING: &str = "a profile switch is already under way";

/// The busy reasons that pass on their own (a lock held for a moment): the
/// check before the restart waits them out a little.
const DICTATION_LOCKED: &str = "whisrs is busy, try again in a moment";
const AGENTS_LOCKED: &str = "agent mode is busy, try again in a moment";

/// How long, in steps, the check before the restart waits out a lock held
/// for a moment.
const SETTLE_STEP: Duration = Duration::from_millis(100);
const SETTLE_STEPS: u32 = 10;

/// What the daemon knows of profiles.
pub(crate) struct ProfileRuntime {
    /// The profile this daemon runs on: `None` is base.
    pub(crate) active: Option<String>,
    /// Why the active profile did not load, if it did not: the daemon runs
    /// on base. Cleared when a switch to base makes the state file agree.
    error: std::sync::Mutex<Option<String>>,
    paths: Paths,
    restarter: Restarter,
    /// A switch wrote the state file and its restart is on the way.
    switching: AtomicBool,
}

impl ProfileRuntime {
    /// This user's profiles, restarted through [`whisrs::service::restart_self`],
    /// whose re-exec shuts down as SIGTERM does first ([`crate::shut_down`]).
    /// Made inside the runtime, which that shutdown runs on.
    pub(crate) fn new(active: Option<String>, error: Option<String>) -> Self {
        let runtime = tokio::runtime::Handle::current();
        let restarter: Restarter = Arc::new(move |context, on_failure| {
            let runtime = runtime.clone();
            whisrs::service::restart_self(
                whisrs::socket_path(),
                move || runtime.block_on(crate::shut_down(&context)),
                on_failure,
            )
        });
        Self::with(Paths::current(), active, error, restarter)
    }

    pub(crate) fn with(
        paths: Paths,
        active: Option<String>,
        error: Option<String>,
        restarter: Restarter,
    ) -> Self {
        Self {
            active,
            error: std::sync::Mutex::new(error),
            paths,
            restarter,
            switching: AtomicBool::new(false),
        }
    }

    /// For a test context that never switches: base, profiles where none
    /// are, and a restarter that fails the test.
    #[cfg(test)]
    pub(crate) fn inert() -> Self {
        Self::with(
            Paths::under(std::path::Path::new("/nonexistent/whisrs-test")),
            None,
            None,
            Arc::new(|_, _| panic!("a test restarted the daemon")),
        )
    }

    /// Why the active profile did not load, for `info`.
    pub(crate) fn error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Whether a switch is under way: its restart is on the way.
    pub(crate) fn is_switching(&self) -> bool {
        self.switching.load(Ordering::SeqCst)
    }

    /// Restart the daemon (`context` is the one this runtime is in), as a
    /// switch does: the tray's Restart Daemon.
    pub(crate) fn restart(
        &self,
        context: Arc<DaemonContext>,
        on_failure: Box<dyn FnOnce(String) + Send>,
    ) {
        (self.restarter)(context, on_failure)
    }
}

/// The refusal `cmd` gets now, if any: while a switch is under way, what
/// would start work the restart cuts off (a dictation, command mode, a
/// read-aloud, a recording or a job on one, an agent turn). Checked where
/// every command comes in, the socket and the hotkey and tray loop.
pub(crate) fn switching_refusal(context: &DaemonContext, cmd: &Command) -> Option<String> {
    (context.profile.is_switching() && starts_work(cmd)).then(|| SWITCHING.to_string())
}

/// Whether `cmd` starts work a restart would cut off. Every command is
/// named, so a new one has to be placed.
fn starts_work(cmd: &Command) -> bool {
    match cmd {
        Command::Toggle { .. }
        | Command::CommandMode
        | Command::LlmCommand { .. }
        | Command::Speak
        | Command::SpeakText { .. } => true,
        Command::Agent { action, .. } => match action {
            AgentAction::Toggle | AgentAction::Send => true,
            AgentAction::List | AgentAction::Reset | AgentAction::Stop => false,
        },
        Command::Recording { action, .. } => match action {
            RecordingAction::Start
            | RecordingAction::Toggle
            | RecordingAction::Retry
            | RecordingAction::Summarize
            | RecordingAction::Name
            | RecordingAction::Delete => true,
            RecordingAction::Stop
            | RecordingAction::Cancel
            | RecordingAction::Status
            | RecordingAction::Copy => false,
        },
        Command::Cancel
        | Command::Status
        | Command::Info
        | Command::Log { .. }
        | Command::ClearHistory
        | Command::SetLlmInstruction { .. }
        | Command::AgentEvent { .. }
        | Command::SwitchProfile { .. } => false,
    }
}

/// `switch-profile`: `name` is a profile, `base`, or `None` for the next.
pub(crate) async fn handle_switch_profile(
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
    name: Option<String>,
) -> Response {
    match switch(&daemon_state, &context, name.as_deref()) {
        Ok(response) => response,
        Err(message) => {
            info!("profile switch refused: {message}");
            Response::Error { message }
        }
    }
}

fn switch(
    daemon_state: &Arc<Mutex<DaemonState>>,
    context: &Arc<DaemonContext>,
    name: Option<&str>,
) -> Result<Response, String> {
    let rt = &context.profile;
    if rt.is_switching() {
        return Err(ALREADY_SWITCHING.to_string());
    }
    let listing = profile::list(&rt.paths.profiles)
        .map_err(|e| format!("cannot read {}: {e}", rt.paths.profiles.display()))?;
    // `next` passes over what would not load, so a broken profile never
    // stops it; a name asked for is checked below, and refused.
    let (target, skipped) = match name.map(str::trim) {
        None | Some("next") => profile::next_usable(
            &rt.paths,
            &listing.names,
            rt.active.as_deref(),
            Check::Daemon,
        )?,
        asked => (
            profile::resolve_target(&listing.names, rt.active.as_deref(), asked)?,
            Vec::new(),
        ),
    };
    let shown = target.as_deref().unwrap_or(BASE).to_string();

    if target == rt.active {
        // The state file can name another, one that did not load: it
        // follows the daemon, so the next start runs on this one too, and
        // that profile's error is moot.
        if profile::read_active(&rt.paths.state).ok().flatten() != target {
            match profile::set_active(&rt.paths.state, target.as_deref()) {
                Ok(()) => *rt.error.lock().unwrap_or_else(|e| e.into_inner()) = None,
                Err(e) => warn!("cannot write {}: {e}", rt.paths.state.display()),
            }
        }
        return Ok(Response::Profile {
            active: shown,
            switching: false,
            skipped,
        });
    }
    if let Some(why) = busy(daemon_state, context) {
        return Err(format!("cannot switch profile while {why}"));
    }
    profile::switch_check(&rt.paths, target.as_deref(), Check::Daemon)?;
    if rt.switching.swap(true, Ordering::SeqCst) {
        return Err(ALREADY_SWITCHING.to_string());
    }
    let old = profile::read_active(&rt.paths.state).ok().flatten();
    if let Err(e) = profile::set_active(&rt.paths.state, target.as_deref()) {
        end_switch(context);
        return Err(format!("cannot write {}: {e}", rt.paths.state.display()));
    }
    let line = profile::switching_line(&shown, &skipped);
    info!("{line}: restarting");
    if context.notify_error() {
        send_notification("whisrs", &line);
    }
    tokio::spawn(finish_switch(
        Arc::clone(daemon_state),
        Arc::clone(context),
        old,
        shown.clone(),
    ));
    Ok(Response::Profile {
        active: shown,
        switching: true,
        skipped,
    })
}

/// Once the reply is out: restart, unless something started meanwhile (it
/// came in before the switch was marked), which rolls the switch back.
async fn finish_switch(
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
    old: Option<String>,
    shown: String,
) {
    tokio::time::sleep(REPLY_GRACE).await;
    if let Some(why) = settled_busy(&daemon_state, &context).await {
        let why = format!("whisrs is no longer idle ({why})");
        restart_failed(&context, old.as_deref(), &shown, &why);
        return;
    }
    let failed = Arc::clone(&context);
    context.profile.restart(
        Arc::clone(&context),
        Box::new(move |why| restart_failed(&failed, old.as_deref(), &shown, &why)),
    );
}

/// Nothing restarted: the state file names the profile in use again, the
/// next switch may run, held replies play, and the user is told why.
fn restart_failed(context: &DaemonContext, old: Option<&str>, shown: &str, why: &str) {
    error!("could not restart whisrsd on profile {shown}: {why}");
    if let Err(e) = profile::set_active(&context.profile.paths.state, old) {
        error!(
            "cannot put back {}: {e}",
            context.profile.paths.state.display()
        );
    }
    end_switch(context);
    if context.notify_error() {
        send_notification("whisrs", &format!("could not switch to {shown}: {why}"));
    }
}

/// The switch is over without a restart: starts are taken again and the
/// replies it held may play.
fn end_switch(context: &DaemonContext) {
    context.profile.switching.store(false, Ordering::SeqCst);
    context.agents.wake_replies();
}

/// What a restart would cut off now, if anything: the checks `info` makes
/// for `busy` and `agent_turn`, each naming its reason.
fn busy(daemon_state: &Mutex<DaemonState>, context: &DaemonContext) -> Option<&'static str> {
    let Ok(ds) = daemon_state.try_lock() else {
        return Some(DICTATION_LOCKED);
    };
    match ds.state_machine.state() {
        State::Idle => {}
        State::Recording => return Some("dictating"),
        State::Transcribing => return Some("a dictation is being transcribed"),
        State::Synthesizing | State::Speaking => return Some("reading aloud"),
    }
    drop(ds);
    #[cfg(feature = "recording")]
    {
        use whisrs::recording::RecordingPhase;
        match context.recording.phase() {
            RecordingPhase::Idle => {}
            RecordingPhase::Recording => return Some("a recording runs"),
            RecordingPhase::Processing => return Some("a recording is being transcribed"),
        }
        if context.recording.session_busy() {
            return Some("a recording starts or stops");
        }
    }
    match context.agents.any_turn_running() {
        Some(false) => None,
        Some(true) => Some("an agent turn runs"),
        None => Some(AGENTS_LOCKED),
    }
}

/// [`busy`], with a lock held for a moment waited out for up to a second:
/// a status or a broadcast that holds one must not cancel a switch.
async fn settled_busy(
    daemon_state: &Mutex<DaemonState>,
    context: &DaemonContext,
) -> Option<&'static str> {
    for _ in 0..SETTLE_STEPS {
        match busy(daemon_state, context) {
            Some(why) if why == DICTATION_LOCKED || why == AGENTS_LOCKED => {
                tokio::time::sleep(SETTLE_STEP).await;
            }
            settled => return settled,
        }
    }
    busy(daemon_state, context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use whisrs::state::Action;

    /// What the injected restarter saw: one call per restart.
    struct Restarts {
        rx: std::sync::mpsc::Receiver<Box<dyn FnOnce(String) + Send>>,
    }

    impl Restarts {
        /// The next restart, within a few seconds of virtual time.
        async fn next(&self) -> Box<dyn FnOnce(String) + Send> {
            for _ in 0..50 {
                if let Ok(on_failure) = self.rx.try_recv() {
                    return on_failure;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            panic!("no restart");
        }

        fn none(&self) -> bool {
            self.rx.try_recv().is_err()
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        paths: Paths,
        daemon_state: Arc<Mutex<DaemonState>>,
        context: Arc<DaemonContext>,
        restarts: Restarts,
    }

    impl Fixture {
        async fn switch(&self, name: Option<&str>) -> Response {
            handle_switch_profile(
                Arc::clone(&self.daemon_state),
                Arc::clone(&self.context),
                name.map(str::to_string),
            )
            .await
        }

        fn state_file(&self) -> Option<String> {
            std::fs::read_to_string(&self.paths.state).ok()
        }
    }

    /// A daemon on `active`, with a valid config.toml (asr-sidecar needs no
    /// key, so the environment does not matter) and the profiles `offline`,
    /// `work` and `broken`.
    fn fixture(active: Option<&str>) -> Fixture {
        fixture_on(active, None)
    }

    /// [`fixture`], its profile's error `error`.
    fn fixture_on(active: Option<&str>, error: Option<&str>) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::write(
            &paths.config,
            "[general]\nbackend = \"asr-sidecar\"\n[asr-sidecar]\nurl = \"http://127.0.0.1:1\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(&paths.profiles).unwrap();
        std::fs::write(paths.profile("offline"), "[general]\nlanguage = \"de\"\n").unwrap();
        std::fs::write(paths.profile("work"), "").unwrap();
        std::fs::write(paths.profile("broken"), "[general]\nbackend = \"nope\"\n").unwrap();
        if let Some(active) = active {
            profile::set_active(&paths.state, Some(active)).unwrap();
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let restarter: Restarter = Arc::new(move |_context, on_failure| {
            tx.lock().unwrap().send(on_failure).unwrap();
        });
        let context = Arc::new(DaemonContext {
            config: toml::from_str("").unwrap(),
            window_tracker: Arc::new(whisrs::window::NoopTracker),
            // Never called: nothing is transcribed.
            transcription_backend: Arc::new(whisrs::transcription::groq::GroqBackend::new(
                String::new(),
            )),
            notify: false,
            state_tx: crate::context::StateBus::new(),
            overlay_level_tx: None,
            overlay_enabled: false,
            #[cfg(feature = "recording")]
            recording: crate::recording::RecordingRuntime::new().0,
            agents: crate::agent::AgentRuntime::disabled(),
            agent_board: tokio::sync::watch::channel(Vec::new()).0,
            agent_overlay_shown: AtomicBool::new(false),
            started_at: 0,
            profile: ProfileRuntime::with(
                paths.clone(),
                active.map(str::to_string),
                error.map(str::to_string),
                restarter,
            ),
        });
        Fixture {
            _dir: dir,
            paths,
            daemon_state: Arc::new(Mutex::new(DaemonState::new())),
            context,
            restarts: Restarts { rx },
        }
    }

    fn refused(response: Response) -> String {
        match response {
            Response::Error { message } => message,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn switching_to(response: &Response, name: &str) -> bool {
        matches!(response, Response::Profile { active, switching: true, .. } if active == name)
    }

    /// A switch writes the state file, answers, then restarts once the
    /// answer is out.
    #[tokio::test(start_paused = true)]
    async fn a_switch_writes_the_state_file_then_restarts() {
        let f = fixture(None);
        let response = f.switch(Some("offline")).await;
        assert!(switching_to(&response, "offline"), "{response:?}");
        assert_eq!(f.state_file().as_deref(), Some("offline\n"));
        assert!(f.restarts.none(), "not before the reply is out");
        let _on_failure = f.restarts.next().await;

        // One switch at a time.
        let message = refused(f.switch(Some("work")).await);
        assert_eq!(message, "a profile switch is already under way");
    }

    /// While a switch is under way every start is refused with one plain
    /// message, before anything runs, and agent replies wait; what starts
    /// nothing goes through.
    #[tokio::test(start_paused = true)]
    async fn starts_are_refused_while_a_switch_is_under_way() {
        use whisrs::agent::AgentAction;
        use whisrs::recording::RecordingAction;

        let f = fixture(None);
        assert!(!crate::agent::replies_held(&f.context));
        assert!(switching_to(&f.switch(Some("offline")).await, "offline"));
        assert!(crate::agent::replies_held(&f.context), "replies wait");

        let agent = |action| Command::Agent {
            action,
            name: Some("jarvis".into()),
            text: Some("hi".into()),
            new: false,
        };
        let recording = |action| Command::Recording {
            action,
            title: None,
            id: Some("2026-10-31_1200".into()),
            options: Default::default(),
        };
        let starts = [
            Command::Toggle { language: None },
            Command::CommandMode,
            Command::LlmCommand { name: "x".into() },
            Command::Speak,
            Command::SpeakText { text: "hi".into() },
            agent(AgentAction::Toggle),
            agent(AgentAction::Send),
            recording(RecordingAction::Start),
            recording(RecordingAction::Toggle),
            recording(RecordingAction::Retry),
            recording(RecordingAction::Summarize),
            recording(RecordingAction::Name),
            recording(RecordingAction::Delete),
        ];
        for cmd in starts {
            let summary = cmd.log_summary();
            let response =
                crate::handle_command(cmd, Arc::clone(&f.daemon_state), Arc::clone(&f.context))
                    .await;
            assert_eq!(refused(response), SWITCHING, "{summary}");
        }
        assert_eq!(
            f.daemon_state.lock().await.state_machine.state(),
            State::Idle
        );
        for cmd in [
            Command::Cancel,
            Command::Status,
            Command::Info,
            Command::Log { limit: 1 },
            Command::ClearHistory,
            Command::SetLlmInstruction { name: "x".into() },
            Command::AgentEvent {
                payload: serde_json::json!({}),
            },
            Command::SwitchProfile { name: None },
            agent(AgentAction::List),
            agent(AgentAction::Reset),
            agent(AgentAction::Stop),
            recording(RecordingAction::Stop),
            recording(RecordingAction::Cancel),
            recording(RecordingAction::Status),
            recording(RecordingAction::Copy),
        ] {
            assert_eq!(
                switching_refusal(&f.context, &cmd),
                None,
                "{}",
                cmd.log_summary()
            );
        }
        let _ = f.restarts.next().await;

        // Nothing is refused without a switch under way.
        let f = fixture(None);
        assert_eq!(
            switching_refusal(&f.context, &Command::Toggle { language: None }),
            None
        );
    }

    /// Something that started before the switch was marked rolls it back
    /// when the grace ends: no restart, the state file as it was, starts
    /// and replies taken again.
    #[tokio::test(start_paused = true)]
    async fn a_start_that_slipped_in_rolls_the_switch_back() {
        let f = fixture(Some("work"));
        assert!(switching_to(&f.switch(Some("offline")).await, "offline"));
        assert_eq!(f.state_file().as_deref(), Some("offline\n"));
        f.daemon_state
            .lock()
            .await
            .state_machine
            .transition(Action::Toggle)
            .unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(f.restarts.none(), "no restart");
        assert_eq!(f.state_file().as_deref(), Some("work\n"));
        assert!(!f.context.profile.is_switching());
        assert!(!crate::agent::replies_held(&f.context));
    }

    /// A lock held for a moment when the grace ends (a status, a broadcast)
    /// is waited out, not taken for a start.
    #[tokio::test(start_paused = true)]
    async fn a_lock_held_for_a_moment_does_not_cancel_the_switch() {
        let f = fixture(None);
        assert!(switching_to(&f.switch(Some("offline")).await, "offline"));
        let daemon_state = Arc::clone(&f.daemon_state);
        tokio::spawn(async move {
            let _held = daemon_state.lock().await;
            tokio::time::sleep(Duration::from_millis(500)).await;
        });
        let _ = f.restarts.next().await;
        assert_eq!(f.state_file().as_deref(), Some("offline\n"));
    }

    /// A switch to base, or `next` wrapping to it, is refused while
    /// config.toml does not load: the restart would run on the defaults.
    /// `next` finds nothing else loading either, and says so.
    #[tokio::test(start_paused = true)]
    async fn a_switch_is_refused_while_config_toml_does_not_load() {
        for (asked, refusal) in [
            (
                Some("base"),
                "cannot switch to base: config.toml is not valid TOML (line 1, column 9)",
            ),
            (
                None,
                "no other profile loads: base (config.toml is not valid TOML), broken \
                 (config.toml is not valid TOML), offline (config.toml is not valid TOML)",
            ),
        ] {
            let f = fixture(Some("work"));
            std::fs::write(&f.paths.config, "[general\n").unwrap();
            let message = refused(f.switch(asked).await);
            assert_eq!(message, refusal, "{asked:?}");
            let message = refused(f.switch(Some("offline")).await);
            assert!(
                message.starts_with("cannot switch to offline: config.toml is not valid TOML"),
                "{message}"
            );
            assert_eq!(f.state_file().as_deref(), Some("work\n"));
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert!(f.restarts.none());
        }
    }

    /// `next` from base passes over `broken` (the names sorted: broken,
    /// offline, work), says so, and switches to `offline`; with no other
    /// profile loading it refuses, naming why.
    #[tokio::test(start_paused = true)]
    async fn next_passes_over_a_profile_that_does_not_load() {
        let f = fixture(None);
        let response = f.switch(None).await;
        assert!(
            matches!(
                &response,
                Response::Profile { active, switching: true, skipped }
                    if active == "offline" && skipped == &["broken"]
            ),
            "{response:?}"
        );
        assert_eq!(f.state_file().as_deref(), Some("offline\n"));
        let _ = f.restarts.next().await;

        // An explicit name is still refused.
        let f = fixture(None);
        assert!(refused(f.switch(Some("broken")).await).starts_with("cannot switch to broken"));

        // config.toml broken: base fails, and every profile over it.
        let f = fixture(Some("work"));
        std::fs::write(&f.paths.config, "[general]\nnotify = \"loud\"\n").unwrap();
        let message = refused(f.switch(None).await);
        assert!(
            message.starts_with("no other profile loads: base (config.toml does not load: "),
            "{message}"
        );
        assert!(
            message.contains(", broken (config.toml does not load"),
            "{message}"
        );
        assert_eq!(f.state_file().as_deref(), Some("work\n"));
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(f.restarts.none());
    }

    /// `next` goes base, offline (the names sorted: broken, offline, work),
    /// and from the last name back to base.
    #[tokio::test(start_paused = true)]
    async fn next_follows_the_order_and_wraps() {
        let f = fixture(Some("offline"));
        assert!(switching_to(&f.switch(None).await, "work"));
        let _ = f.restarts.next().await;

        let f = fixture(Some("work"));
        assert!(switching_to(&f.switch(Some("next")).await, "base"));
        assert_eq!(f.state_file().as_deref(), Some("base\n"));
        let _ = f.restarts.next().await;
    }

    /// The profile in use answers at once, with no restart; the state file
    /// follows it when it named another.
    #[tokio::test(start_paused = true)]
    async fn the_active_profile_answers_without_a_restart() {
        let f = fixture(Some("offline"));
        let response = f.switch(Some("offline")).await;
        assert!(matches!(
            &response,
            Response::Profile { active, switching: false, .. } if active == "offline"
        ));

        // The daemon runs on base (its profile did not load); the state
        // file still names it.
        let f = fixture_on(None, Some("profile broken did not load, using base: x"));
        profile::set_active(&f.paths.state, Some("broken")).unwrap();
        let response = f.switch(Some("base")).await;
        assert!(matches!(
            &response,
            Response::Profile {
                switching: false,
                ..
            }
        ));
        assert_eq!(f.state_file().as_deref(), Some("base\n"));
        assert_eq!(f.context.profile.error(), None, "moot now");
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(f.restarts.none());
    }

    /// A target that would not load is refused before anything is written.
    #[tokio::test(start_paused = true)]
    async fn a_broken_or_missing_target_is_refused() {
        let f = fixture(None);
        let message = refused(f.switch(Some("broken")).await);
        assert!(
            message.starts_with("cannot switch to broken: the config with profiles/broken.toml"),
            "{message}"
        );
        let message = refused(f.switch(Some("gone")).await);
        assert_eq!(
            message,
            "no profile named gone: profiles/gone.toml does not exist"
        );
        let message = refused(f.switch(Some("Bad Name")).await);
        assert!(message.contains("not a profile name"), "{message}");
        assert_eq!(f.state_file(), None);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(f.restarts.none());
    }

    /// `next` with no profile at all says how to add one.
    #[tokio::test(start_paused = true)]
    async fn next_without_profiles_says_how_to_add_one() {
        let f = fixture(None);
        for name in ["offline", "work", "broken"] {
            std::fs::remove_file(f.paths.profile(name)).unwrap();
        }
        assert_eq!(refused(f.switch(None).await), profile::NO_PROFILES);
    }

    /// Each dictation state refuses, naming itself.
    #[tokio::test(start_paused = true)]
    async fn a_switch_is_refused_unless_dictation_is_idle() {
        for (actions, why) in [
            (&[Action::Toggle][..], "dictating"),
            (
                &[Action::Toggle, Action::Toggle][..],
                "a dictation is being transcribed",
            ),
            (&[Action::SpeakStart][..], "reading aloud"),
            (
                &[Action::SpeakStart, Action::SpeakPlaying][..],
                "reading aloud",
            ),
        ] {
            let f = fixture(None);
            {
                let mut ds = f.daemon_state.lock().await;
                for action in actions {
                    ds.state_machine.transition(*action).unwrap();
                }
            }
            let message = refused(f.switch(Some("offline")).await);
            assert_eq!(message, format!("cannot switch profile while {why}"));
            assert_eq!(f.state_file(), None, "{why}");
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert!(f.restarts.none(), "{why}");
        }
    }

    /// The dictation lock held (a dictation starting, a command running)
    /// refuses without waiting for it.
    #[tokio::test(start_paused = true)]
    async fn a_switch_is_refused_while_the_dictation_lock_is_held() {
        let f = fixture(None);
        let _held = f.daemon_state.lock().await;
        let message = refused(f.switch(Some("offline")).await);
        assert_eq!(
            message,
            "cannot switch profile while whisrs is busy, try again in a moment"
        );
    }

    /// An agent turn running, or on its way, refuses.
    #[tokio::test(start_paused = true)]
    async fn a_switch_is_refused_while_an_agent_turn_runs() {
        let f = fixture(None);
        let turn = f.context.agents.begin_turn();
        let message = refused(f.switch(Some("offline")).await);
        assert_eq!(message, "cannot switch profile while an agent turn runs");
        drop(turn);

        let _held = f.context.agents.hold_for_test();
        let message = refused(f.switch(Some("offline")).await);
        assert_eq!(
            message,
            "cannot switch profile while agent mode is busy, try again in a moment"
        );
    }

    /// A recording capturing, processed, or starting or stopping refuses.
    #[cfg(feature = "recording")]
    #[tokio::test(start_paused = true)]
    async fn a_switch_is_refused_while_a_recording_is_busy() {
        use whisrs::recording::RecordingPhase;

        for (phase, why) in [
            (RecordingPhase::Recording, "a recording runs"),
            (
                RecordingPhase::Processing,
                "a recording is being transcribed",
            ),
        ] {
            let f = fixture(None);
            f.context.recording.show_for_test(phase);
            let message = refused(f.switch(Some("offline")).await);
            assert_eq!(message, format!("cannot switch profile while {why}"));
        }

        let f = fixture(None);
        let _held = f.context.recording.hold_session_for_test();
        let message = refused(f.switch(Some("offline")).await);
        assert_eq!(
            message,
            "cannot switch profile while a recording starts or stops"
        );
    }

    /// A restart that did not happen puts the state file back and lets the
    /// next switch run.
    #[tokio::test(start_paused = true)]
    async fn a_failed_restart_puts_the_state_file_back() {
        let f = fixture(Some("offline"));
        assert!(switching_to(&f.switch(Some("work")).await, "work"));
        assert_eq!(f.state_file().as_deref(), Some("work\n"));
        let on_failure = f.restarts.next().await;
        on_failure("`systemctl --user restart whisrs.service` failed".to_string());
        assert_eq!(f.state_file().as_deref(), Some("offline\n"));

        assert!(switching_to(&f.switch(Some("base")).await, "base"));
        let _ = f.restarts.next().await;
    }
}
