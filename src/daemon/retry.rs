//! `whisrs transcribe-recovery`: transcribe the audio a failed batch
//! dictation saved for recovery (`whisrs::audio::recovery`), answer with the
//! text (copied to the clipboard and saved to history too), then remove the
//! file.
//!
//! The retry runs as `Transcribing`, entered straight from Idle
//! (`Action::RetryStart`), so nothing else starts while it waits on the
//! backend. Like command mode once it claimed its recording, it owns that
//! state until its own `TranscriptionDone`: `cancel` has no transition out
//! of `Transcribing`.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{error, info, warn};

use whisrs::audio::capture::SAMPLE_RATE;
use whisrs::audio::recovery;
use whisrs::state::Action;
use whisrs::{Response, State};

use crate::context::{DaemonContext, DaemonState};
use crate::dictation::{resolve_language, validate_toggle_language};
use crate::pipeline::{
    apply_llm_post_process, format_api_error, history_backend_tag, save_history_entry,
    transcribe_batch_audio, BatchOptions,
};

/// Where a transcript is copied: the system clipboard in the daemon, a fake
/// in tests, which must never touch the real one. Called off the async
/// runtime.
type Clipboard = Arc<dyn Fn(&str) -> anyhow::Result<()> + Send + Sync>;

/// `transcribe-recovery`: `file` is a recovery file's absolute path, `None`
/// the newest; `language` overrides `[general] language`.
pub(crate) async fn handle_transcribe_recovery(
    daemon_state: Arc<Mutex<DaemonState>>,
    context: Arc<DaemonContext>,
    file: Option<String>,
    language: Option<String>,
) -> Response {
    let clipboard: Clipboard = Arc::new(|text: &str| xkb_type::default_clipboard().set_text(text));
    retry(
        &daemon_state,
        &context,
        &recovery::recovery_dir(),
        file,
        language,
        clipboard,
    )
    .await
}

/// [`handle_transcribe_recovery`] with the recovery folder `dir` and the
/// clipboard given.
async fn retry(
    daemon_state: &Mutex<DaemonState>,
    context: &DaemonContext,
    dir: &Path,
    file: Option<String>,
    language: Option<String>,
    clipboard: Clipboard,
) -> Response {
    let language = match validate_toggle_language(language) {
        Ok(language) => language,
        Err(message) => return Response::Error { message },
    };

    // The file and its audio, read off the async runtime. A refusal here
    // has changed nothing.
    let dir = dir.to_path_buf();
    let loaded = tokio::task::spawn_blocking(move || {
        let path = recovery::resolve_recovery_file_in(&dir, file.as_deref().map(Path::new))?;
        let samples = recovery::load_recovery_audio(&path)?;
        anyhow::Ok((path, samples))
    })
    .await;
    let (path, samples) = match loaded {
        Ok(Ok(loaded)) => loaded,
        Ok(Err(e)) => {
            warn!("transcribe-recovery refused: {e:#}");
            return Response::Error {
                message: format!("{e:#}"),
            };
        }
        Err(e) => {
            return Response::Error {
                message: format!("failed to read the recovery audio: {e}"),
            }
        }
    };

    {
        let mut ds = daemon_state.lock().await;
        let state = ds.state_machine.state();
        if state != State::Idle {
            return Response::Error {
                message: format!("busy: the daemon is {state}; try again when it is idle"),
            };
        }
        // From Idle, RetryStart cannot fail.
        let _ = ds.state_machine.transition(Action::RetryStart);
        context.state_tx.publish(&ds);
    }

    let language = resolve_language(language, &context.config.general.language);
    info!(
        "transcribe-recovery: {} ({:.1} s, language {language})",
        path.display(),
        samples.len() as f64 / f64::from(SAMPLE_RATE)
    );
    let response = transcribe(&path, &samples, context, &language, clipboard).await;

    // Back to Idle on every path. The retry put the machine in
    // Transcribing, which `cancel` cannot leave and no other flow starts
    // from, so the state is still the retry's; from any other state this
    // transition is refused and changes nothing.
    let mut ds = daemon_state.lock().await;
    if ds
        .state_machine
        .transition(Action::TranscriptionDone)
        .is_ok()
    {
        context.state_tx.publish(&ds);
    }
    response
}

/// Transcribe `samples`, the audio of the recovery file `path`, and deliver
/// the text: `[general] llm_post_process`, history, the clipboard. The file
/// is removed once the backend answered, and kept when it failed (nothing
/// is saved again: [`BatchOptions::recovery_retry`]). Touches no daemon
/// state.
async fn transcribe(
    path: &Path,
    samples: &[i16],
    context: &DaemonContext,
    language: &str,
    clipboard: Clipboard,
) -> Response {
    let file = path.display().to_string();
    let text =
        match transcribe_batch_audio(samples, context, language, &BatchOptions::recovery_retry())
            .await
        {
            Ok(text) => text,
            Err(e) => {
                error!("transcribe-recovery: transcription failed: {e:#}");
                return Response::Error {
                    message: format!(
                        "transcription failed: {}. The audio is kept at {file}",
                        format_api_error(&e)
                    ),
                };
            }
        };

    let text = if text.is_empty() {
        info!("transcribe-recovery: nothing transcribed in {file}");
        text
    } else {
        // As a batch dictation does, so the retry gives what the
        // dictation would have typed.
        let outcome = apply_llm_post_process(text, context).await;
        save_history_entry(
            context.config.general.history,
            &outcome.text,
            &history_backend_tag(&context.config.general.backend, outcome.post_processed),
            language,
            samples.len() as f64 / f64::from(SAMPLE_RATE),
        );
        let copy = outcome.text.clone();
        match tokio::task::spawn_blocking(move || clipboard(&copy)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("transcribe-recovery: failed to copy the transcript: {e:#}"),
            Err(e) => warn!("transcribe-recovery: clipboard task failed: {e}"),
        }
        info!(
            "transcribe-recovery: {} chars from {file}",
            outcome.text.len()
        );
        outcome.text
    };

    match tokio::fs::remove_file(path).await {
        Ok(()) => info!("transcribe-recovery: removed {file}"),
        Err(e) => warn!("transcribe-recovery: failed to remove {file}: {e}"),
    }
    Response::Transcript { text, file }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;

    use whisrs::audio::wav::encode_wav;

    use crate::pipeline::take_saved_history;
    use crate::pipeline::tests::{
        audio_that_passes_the_gate, context_with_backend, FailingBackend, StubBackend,
    };

    /// A clipboard that keeps what it was given.
    fn fake_clipboard() -> (Clipboard, Arc<StdMutex<Vec<String>>>) {
        let copied = Arc::new(StdMutex::new(Vec::new()));
        let sink = Arc::clone(&copied);
        let clipboard: Clipboard = Arc::new(move |text: &str| {
            sink.lock().unwrap().push(text.to_string());
            Ok(())
        });
        (clipboard, copied)
    }

    /// A recovery file in `dir` with audio the gate passes.
    fn saved(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, encode_wav(&audio_that_passes_the_gate()).unwrap()).unwrap();
        path
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn refusal(response: Response) -> String {
        match response {
            Response::Error { message } => message,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    async fn state(daemon_state: &Mutex<DaemonState>) -> State {
        daemon_state.lock().await.state_machine.state()
    }

    /// The newest file is transcribed, its text answered, copied and saved
    /// to history, the file removed, and the daemon is Idle again.
    #[tokio::test]
    async fn the_newest_file_is_transcribed_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let older = saved(dir.path(), "recovery_2026-11-01T09-00-00.wav");
        let newest = saved(dir.path(), "recovery_2026-11-02T09-00-00.wav");
        let file = std::fs::File::options().write(true).open(&older).unwrap();
        file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60))
            .unwrap();
        let context = context_with_backend(StubBackend {
            text: "hello from the past",
            sends_prompt: false,
        });
        let daemon_state = Mutex::new(DaemonState::new());
        let (clipboard, copied) = fake_clipboard();
        take_saved_history();

        let response = retry(&daemon_state, &context, dir.path(), None, None, clipboard).await;
        let Response::Transcript { text, file } = response else {
            panic!("expected a transcript, got {response:?}");
        };
        assert_eq!(text, "hello from the past");
        assert_eq!(PathBuf::from(&file), newest);
        assert!(!newest.exists());
        assert!(older.exists());
        assert_eq!(*copied.lock().unwrap(), ["hello from the past"]);
        assert_eq!(take_saved_history(), ["hello from the past"]);
        assert_eq!(state(&daemon_state).await, State::Idle);
        assert_eq!(context.state_tx.raw(), State::Idle);
        assert!(crate::pipeline::take_saved_recoveries().is_empty());
    }

    /// Empty text still removes the file: the backend answered.
    #[tokio::test]
    async fn nothing_transcribed_removes_the_file_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = saved(dir.path(), "recovery_a.wav");
        let context = context_with_backend(StubBackend {
            text: "",
            sends_prompt: false,
        });
        let daemon_state = Mutex::new(DaemonState::new());
        let (clipboard, copied) = fake_clipboard();
        take_saved_history();

        let arg = Some(path.display().to_string());
        let response = retry(&daemon_state, &context, dir.path(), arg, None, clipboard).await;
        assert!(
            matches!(&response, Response::Transcript { text, .. } if text.is_empty()),
            "{response:?}"
        );
        assert!(!path.exists());
        assert!(copied.lock().unwrap().is_empty());
        assert!(take_saved_history().is_empty());
        assert_eq!(state(&daemon_state).await, State::Idle);
    }

    /// A backend failure keeps the file, saves no second copy, names the
    /// file in the error, and returns to Idle.
    #[tokio::test]
    async fn a_failure_keeps_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = saved(dir.path(), "recovery_a.wav");
        let context = context_with_backend(FailingBackend);
        let daemon_state = Mutex::new(DaemonState::new());
        let (clipboard, copied) = fake_clipboard();
        crate::pipeline::take_saved_recoveries();

        let message =
            refusal(retry(&daemon_state, &context, dir.path(), None, None, clipboard).await);
        assert_eq!(
            message,
            format!(
                "transcription failed: Cannot reach API — check your internet connection. The \
                 audio is kept at {}",
                path.display()
            )
        );
        assert_eq!(names(dir.path()), ["recovery_a.wav"]);
        assert!(crate::pipeline::take_saved_recoveries().is_empty());
        assert!(copied.lock().unwrap().is_empty());
        assert_eq!(state(&daemon_state).await, State::Idle);
        assert_eq!(context.state_tx.raw(), State::Idle);
    }

    /// Refused unless Idle, before anything changes.
    #[tokio::test]
    async fn refused_while_busy() {
        let dir = tempfile::tempdir().unwrap();
        let path = saved(dir.path(), "recovery_a.wav");
        let context = context_with_backend(StubBackend {
            text: "never",
            sends_prompt: false,
        });
        let mut ds = DaemonState::new();
        ds.state_machine.transition(Action::Toggle).unwrap();
        let daemon_state = Mutex::new(ds);
        let (clipboard, copied) = fake_clipboard();

        let message =
            refusal(retry(&daemon_state, &context, dir.path(), None, None, clipboard).await);
        assert_eq!(
            message,
            "busy: the daemon is recording; try again when it is idle"
        );
        assert_eq!(state(&daemon_state).await, State::Recording);
        assert!(path.exists());
        assert!(copied.lock().unwrap().is_empty());
    }

    /// A file outside the recovery folder is refused and left alone.
    #[tokio::test]
    async fn a_file_elsewhere_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("recovery");
        std::fs::create_dir(&dir).unwrap();
        let outside = saved(root.path(), "recovery_a.wav");
        let before = std::fs::read(&outside).unwrap();
        let context = context_with_backend(StubBackend {
            text: "never",
            sends_prompt: false,
        });
        let daemon_state = Mutex::new(DaemonState::new());
        let (clipboard, _copied) = fake_clipboard();

        let arg = Some(outside.display().to_string());
        let message = refusal(retry(&daemon_state, &context, &dir, arg, None, clipboard).await);
        assert_eq!(
            message,
            format!(
                "not a recovery file: {} (recovery audio lives in {})",
                outside.display(),
                dir.display()
            )
        );
        assert_eq!(std::fs::read(&outside).unwrap(), before);
        assert_eq!(state(&daemon_state).await, State::Idle);
    }

    /// Audio in another format than a recovery file's is refused.
    #[tokio::test]
    async fn another_format_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recovery_a.wav");
        let spec = hound::WavSpec {
            sample_rate: 48_000,
            ..whisrs::audio::wav::WAV_SPEC
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        writer.write_sample(0i16).unwrap();
        writer.finalize().unwrap();
        let context = context_with_backend(StubBackend {
            text: "never",
            sends_prompt: false,
        });
        let daemon_state = Mutex::new(DaemonState::new());
        let (clipboard, _copied) = fake_clipboard();

        let message =
            refusal(retry(&daemon_state, &context, dir.path(), None, None, clipboard).await);
        assert!(message.contains("is not recovery audio"), "{message}");
        assert!(path.exists());
        assert_eq!(state(&daemon_state).await, State::Idle);
    }

    #[tokio::test]
    async fn no_recovery_audio_and_a_bad_language_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let context = context_with_backend(StubBackend {
            text: "never",
            sends_prompt: false,
        });
        let daemon_state = Mutex::new(DaemonState::new());
        let (clipboard, _copied) = fake_clipboard();

        let message = refusal(
            retry(
                &daemon_state,
                &context,
                dir.path(),
                None,
                None,
                Arc::clone(&clipboard),
            )
            .await,
        );
        assert_eq!(
            message,
            format!("no recovery audio in {}", dir.path().display())
        );

        saved(dir.path(), "recovery_a.wav");
        let language = Some("english".to_string());
        let message = refusal(
            retry(
                &daemon_state,
                &context,
                dir.path(),
                None,
                language,
                clipboard,
            )
            .await,
        );
        assert!(message.starts_with("invalid language"), "{message}");
        assert_eq!(names(dir.path()), ["recovery_a.wav"]);
        assert_eq!(state(&daemon_state).await, State::Idle);
    }
}
