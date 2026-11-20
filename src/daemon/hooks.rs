//! Recording-lifecycle hooks: MPRIS pause + shell commands.

use whisrs::hooks::{hook_event_for, run_hook, HookEvent, MediaPauseTracker};
use whisrs::{HooksConfig, State};

/// Watches daemon state broadcasts and fires recording-lifecycle hooks.
pub(crate) async fn hook_dispatch_loop(
    mut state_rx: tokio::sync::watch::Receiver<State>,
    hooks: HooksConfig,
) {
    let mut prev = *state_rx.borrow();
    // Bookkeeping lives in the lib and is unit-tested there; this loop only
    // supplies the D-Bus round-trips.
    let mut media = MediaPauseTracker::new();
    while state_rx.changed().await.is_ok() {
        let new = *state_rx.borrow();
        match hook_event_for(prev, new) {
            Some(HookEvent::RecordStart) => {
                if hooks.media_auto_pause {
                    let seen = whisrs::mpris::players().await;
                    let plan = media.plan(HookEvent::RecordStart, &seen);
                    // Only the pauses that succeeded are remembered, so the
                    // stop resumes exactly what this daemon stopped.
                    media.confirm_paused(&whisrs::mpris::pause(&plan.pause).await);
                }
                if let Some(cmd) = hooks.on_record_start.as_deref() {
                    run_hook(cmd);
                }
            }
            Some(HookEvent::RecordStop) => {
                if hooks.media_auto_pause {
                    let plan = media.plan(HookEvent::RecordStop, &[]);
                    whisrs::mpris::resume(&plan.resume).await;
                }
                if let Some(cmd) = hooks.on_record_stop.as_deref() {
                    run_hook(cmd);
                }
            }
            None => {}
        }
        prev = new;
    }
}
