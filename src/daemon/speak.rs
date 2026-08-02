use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use whisrs::state::Action;
use whisrs::{Response, State};

use crate::context::{DaemonContext, DaemonState};
use crate::factory::resolve_tts_api_key;
use crate::notify::send_notification;
use crate::selection::capture_selection;

/// Read the selected text aloud via TTS.
///
/// FSM-authoritative: read-aloud has its own `Synthesizing`/`Speaking` states,
/// so "is read-aloud active" is determined by [`State`], not the `tts_stop`
/// flag (which now exists purely as the low-level playback interrupt). This
/// removes the race where a `Speak` press landing in the gap between playback
/// finishing and the cleanup task clearing `tts_stop` was swallowed as a stop.
///
/// The daemon mutex is never held across the synth or playback `.await`.
pub(crate) async fn handle_speak(
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
) -> Response {
    use std::sync::atomic::{AtomicBool, Ordering};

    // (a) Inspect the FSM. A second press while read-aloud is active is a
    // deterministic stop, regardless of playback timing.
    {
        let mut ds = daemon_state.lock().await;
        match ds.state_machine.state() {
            State::Speaking | State::Synthesizing => {
                if let Some(stop) = ds.tts_stop.take() {
                    stop.store(true, Ordering::Release);
                }
                let _ = ds.state_machine.transition(Action::Cancel);
                let new_state = ds.state_machine.state();
                let _ = context.state_tx.send(new_state);
                if let Some(level_tx) = &context.overlay_level_tx {
                    let _ = level_tx.send(0.0);
                }
                info!("speak: stopped in-progress read-aloud");
                return Response::Ok { state: new_state };
            }
            State::Recording | State::Transcribing => {
                return Response::Error {
                    message: "busy — finish or cancel recording first".to_string(),
                };
            }
            State::Idle => {
                // Fall through; do NOT transition yet.
            }
        }
    }

    // (b) Verify TTS is configured/enabled and build the backend. State is
    // still Idle on any error here — nothing to unwind.
    let tts_config = match &context.config.tts {
        Some(t) if t.enabled => t.clone(),
        Some(_) => {
            return Response::Error {
                message: "TTS is disabled — set [tts] enabled = true in config.toml".to_string(),
            };
        }
        None => {
            return Response::Error {
                message: "TTS is not configured — add a [tts] section to config.toml".to_string(),
            };
        }
    };

    let api_key = resolve_tts_api_key(&context.config);
    let backend = match whisrs::tts::create_backend(&tts_config, api_key) {
        Ok(b) => b,
        Err(e) => {
            return Response::Error {
                message: e.to_string(),
            };
        }
    };

    // (c) Capture the selection. On failure (incl. empty), surface an error
    // and — since this is hotkey-triggered — notify so the user sees why.
    info!("speak: getting selected text");
    let selected_text = match capture_selection(&context).await {
        Ok(text) => text,
        Err(message) => {
            if context.notify_error() {
                send_notification("whisrs", &format!("Read-aloud: {message}"));
            }
            return Response::Error { message };
        }
    };

    // (d) Re-lock; only begin synthesizing if still Idle (a concurrent command
    // may have intervened). Do NOT proceed otherwise.
    {
        let mut ds = daemon_state.lock().await;
        if ds.state_machine.state() != State::Idle {
            return Response::Ok {
                state: ds.state_machine.state(),
            };
        }
        let _ = ds.state_machine.transition(Action::SpeakStart);
        let _ = context.state_tx.send(ds.state_machine.state());
    }

    info!("speak: synthesizing {} chars", selected_text.len());
    if context.notify_state() {
        send_notification("whisrs", "Reading selection aloud...");
    }

    // (e) Synthesize (no lock held).
    let synth_result = backend.synthesize(&selected_text).await;

    // (f) Re-lock to decide whether to play.
    let final_state = {
        let mut ds = daemon_state.lock().await;

        // A second press during synthesis cancelled us — state is no longer
        // Synthesizing. Don't play; report the current state.
        if ds.state_machine.state() != State::Synthesizing {
            info!("speak: synthesis superseded by a concurrent command");
            return Response::Ok {
                state: ds.state_machine.state(),
            };
        }

        let wav_bytes = match synth_result {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("speak: synthesis failed: {e}");
                let _ = ds.state_machine.transition(Action::SpeakDone);
                let _ = context.state_tx.send(ds.state_machine.state());
                if let Some(level_tx) = &context.overlay_level_tx {
                    let _ = level_tx.send(0.0);
                }
                drop(ds);
                if context.notify_error() {
                    send_notification("whisrs", &format!("Read-aloud failed: {e}"));
                }
                return Response::Error {
                    message: format!("TTS synthesis failed: {e}"),
                };
            }
        };

        let stop = Arc::new(AtomicBool::new(false));
        ds.tts_stop = Some(Arc::clone(&stop));
        let _ = ds.state_machine.transition(Action::SpeakPlaying);
        let new_state = ds.state_machine.state();
        let _ = context.state_tx.send(new_state);

        // (g) Spawn interruptible playback feeding the speaking overlay.
        let ds_ref = Arc::clone(&daemon_state);
        let context_for_cleanup = Arc::clone(&context);
        let playback_stop = Arc::clone(&stop);
        let level_tx = context.overlay_level_tx.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                whisrs::audio::playback::play_wav(&wav_bytes, stop, level_tx)
            })
            .await;
            match result {
                Ok(Ok(())) => debug!("speak: playback finished"),
                Ok(Err(e)) => warn!("speak: playback failed: {e}"),
                Err(e) => warn!("speak: playback task panicked: {e}"),
            }

            // On finish/interrupt: only finalize if we still own playback and
            // the FSM is still Speaking. A second Speak / Cancel already
            // transitioned to Idle and replaced/cleared tts_stop.
            let mut ds = ds_ref.lock().await;
            let still_ours = ds
                .tts_stop
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &playback_stop));
            if ds.state_machine.state() == State::Speaking && still_ours {
                let _ = ds.state_machine.transition(Action::SpeakDone);
                ds.tts_stop = None;
                let _ = context_for_cleanup.state_tx.send(ds.state_machine.state());
                if let Some(level_tx) = &context_for_cleanup.overlay_level_tx {
                    let _ = level_tx.send(0.0);
                }
            }
        });

        new_state
    };

    Response::Ok { state: final_state }
}
