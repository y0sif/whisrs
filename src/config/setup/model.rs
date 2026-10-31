//! Downloading a whisper.cpp model from Hugging Face into the models folder.
//!
//! The file is written as a `.part` of its own (`ggml-<name>.bin.<pid>-<id>.part`,
//! so two downloads of one model never share one) and renamed only once it
//! is whole: a cancelled, cut or short download never leaves a model that
//! looks installed, and each removes its own `.part` on every way out but
//! success.

use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Where whisper.cpp's models are, by file name.
pub const BASE_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";

/// How long a read may stall, and the answer take, before the download
/// fails.
pub const STALL: Duration = Duration::from_secs(10);

/// A new download's id, for its `.part`.
pub fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Where download `id` of model `name` writes, in `dir`, until it is whole.
pub fn part_path(dir: &Path, name: &str, id: u64) -> PathBuf {
    dir.join(format!(
        "{}.{}-{id}.part",
        file_name(name),
        std::process::id()
    ))
}

/// The file a model is saved as: `ggml-<name>.bin`.
pub fn file_name(name: &str) -> String {
    format!("ggml-{name}.bin")
}

/// Where model `name` goes in `dir`.
pub fn path(dir: &Path, name: &str) -> PathBuf {
    dir.join(file_name(name))
}

/// Why a download did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadError {
    Cancelled,
    Failed(String),
}

impl fmt::Display for DownloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DownloadError::Cancelled => f.write_str("download cancelled"),
            DownloadError::Failed(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for DownloadError {}

/// Download model `name` (`base.en`) into `dir`; the file it wrote.
/// `on_progress` hears the bytes so far and the size, when the server said
/// it; `cancel` stops it between reads.
pub fn download(
    name: &str,
    dir: &Path,
    on_progress: impl FnMut(u64, Option<u64>),
    cancel: &AtomicBool,
) -> Result<PathBuf, DownloadError> {
    download_from(BASE_URL, name, dir, next_id(), on_progress, cancel)
}

/// [`download`] from `base_url` instead of Hugging Face, as download `id`
/// (its `.part` is [`part_path`]).
pub fn download_from(
    base_url: &str,
    name: &str,
    dir: &Path,
    id: u64,
    on_progress: impl FnMut(u64, Option<u64>),
    cancel: &AtomicBool,
) -> Result<PathBuf, DownloadError> {
    fetch((base_url, STALL), name, dir, id, on_progress, cancel)
}

/// [`download_from`], a read stalling `from.1` failing it.
fn fetch(
    from: (&str, Duration),
    name: &str,
    dir: &Path,
    id: u64,
    mut on_progress: impl FnMut(u64, Option<u64>),
    cancel: &AtomicBool,
) -> Result<PathBuf, DownloadError> {
    let (base_url, stall) = from;
    let failed = |why: String| DownloadError::Failed(why);
    // A name is a file name, never a path.
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        || name.starts_with('.')
    {
        return Err(failed(format!("{name:?} is not a model name")));
    }
    fs::create_dir_all(dir)
        .map_err(|e| failed(format!("failed to create {}: {e}", dir.display())))?;
    let url = format!("{}/{}", base_url.trim_end_matches('/'), file_name(name));
    let client = reqwest::blocking::Client::builder()
        .user_agent("whisrs")
        .connect_timeout(stall)
        .timeout(stall)
        .build()
        .map_err(|e| failed(format!("failed to build an HTTP client: {e}")))?;
    let mut response = client
        .get(&url)
        .send()
        .map_err(|e| failed(format!("cannot reach {url}: {e}")))?;
    if !response.status().is_success() {
        return Err(failed(format!(
            "download failed: HTTP {} from {url}",
            response.status()
        )));
    }
    let total = response.content_length();

    let part = part_path(dir, name, id);
    let dest = path(dir, name);
    let result = (|| {
        let mut file = File::create(&part)
            .map_err(|e| failed(format!("failed to create {}: {e}", part.display())))?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut done: u64 = 0;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(DownloadError::Cancelled);
            }
            let n = response
                .read(&mut buf)
                .map_err(|e| failed(format!("download interrupted: {e}")))?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])
                .map_err(|e| failed(format!("failed to write the model: {e}")))?;
            done += n as u64;
            on_progress(done, total);
        }
        match total {
            Some(total) if total != done => {
                return Err(failed(format!("download ended at {done} of {total} bytes")))
            }
            _ if done == 0 => return Err(failed("the download was empty".to_string())),
            _ => {}
        }
        file.sync_all()
            .map_err(|e| failed(format!("failed to write the model: {e}")))?;
        fs::rename(&part, &dest)
            .map_err(|e| failed(format!("failed to save {}: {e}", dest.display())))?;
        Ok(dest.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::mpsc;

    use super::*;

    /// How the fake server answers its one request.
    enum Answer {
        /// 200 with the body, and its length as Content-Length.
        Whole(Vec<u8>),
        /// 200 saying `length`, sending `body`, then closing.
        Short {
            length: usize,
            body: Vec<u8>,
        },
        /// 200 saying `length`, sending `body`, then holding the connection
        /// until the test says so.
        Hold {
            length: usize,
            body: Vec<u8>,
        },
        NotFound,
    }

    /// A server on a free local port for one request; its base URL, what
    /// path it was asked for, and a sender that lets a held answer go.
    fn serve(answer: Answer) -> (String, mpsc::Receiver<String>, mpsc::Sender<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (asked_tx, asked) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            let _ = asked_tx.send(first.split_whitespace().nth(1).unwrap_or("").to_string());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
            }
            let head = |length: usize| {
                format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n")
            };
            match answer {
                Answer::Whole(body) => {
                    stream.write_all(head(body.len()).as_bytes()).unwrap();
                    stream.write_all(&body).unwrap();
                }
                Answer::Short { length, body } => {
                    stream.write_all(head(length).as_bytes()).unwrap();
                    stream.write_all(&body).unwrap();
                }
                Answer::Hold { length, body } => {
                    stream.write_all(head(length).as_bytes()).unwrap();
                    stream.write_all(&body).unwrap();
                    stream.flush().unwrap();
                    let _ = released.recv_timeout(Duration::from_secs(10));
                }
                Answer::NotFound => {
                    stream
                        .write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                }
            }
        });
        (base, asked, release)
    }

    /// The files left in `dir`.
    fn left(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn a_whole_download_is_renamed_into_place() {
        let body: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        let (base, asked, _release) = serve(Answer::Whole(body.clone()));
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models");
        let mut seen = Vec::new();
        let saved = download_from(
            &base,
            "tiny.en",
            &models,
            1,
            |done, total| seen.push((done, total)),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(saved, models.join("ggml-tiny.en.bin"));
        assert_eq!(asked.recv().unwrap(), "/ggml-tiny.en.bin");
        assert_eq!(fs::read(&saved).unwrap(), body);
        assert_eq!(left(&models), ["ggml-tiny.en.bin"]);
        assert_eq!(seen.last(), Some(&(200_000, Some(200_000))));
    }

    /// Esc mid-body: nothing is left, neither the model nor its `.part`.
    #[test]
    fn a_cancel_mid_body_leaves_nothing() {
        let (base, _asked, release) = serve(Answer::Hold {
            length: 5_000_000,
            body: vec![7u8; 100_000],
        });
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        let result = download_from(
            &base,
            "base.en",
            dir.path(),
            2,
            |done, _| {
                if done > 0 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &cancel,
        );
        let _ = release.send(());
        assert_eq!(result, Err(DownloadError::Cancelled));
        assert!(left(dir.path()).is_empty(), "{:?}", left(dir.path()));
    }

    /// A body shorter than its Content-Length is a failure, and leaves
    /// nothing.
    #[test]
    fn a_short_body_fails_and_leaves_nothing() {
        let (base, _asked, _release) = serve(Answer::Short {
            length: 10_000,
            body: vec![1u8; 4_000],
        });
        let dir = tempfile::tempdir().unwrap();
        let result = download_from(
            &base,
            "small",
            dir.path(),
            3,
            |_, _| {},
            &AtomicBool::new(false),
        );
        assert!(
            matches!(result, Err(DownloadError::Failed(_))),
            "{result:?}"
        );
        assert!(left(dir.path()).is_empty(), "{:?}", left(dir.path()));
    }

    #[test]
    fn a_404_fails_and_leaves_nothing() {
        let (base, _asked, _release) = serve(Answer::NotFound);
        let dir = tempfile::tempdir().unwrap();
        let result = download_from(
            &base,
            "nope",
            dir.path(),
            3,
            |_, _| {},
            &AtomicBool::new(false),
        );
        match result {
            Err(DownloadError::Failed(why)) => assert!(why.contains("404"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(left(dir.path()).is_empty());
    }

    /// Each download writes a `.part` of its own and removes only it: a
    /// cancelled one leaves another download's `.part` of the same model.
    #[test]
    fn each_download_has_its_own_part() {
        let dir = tempfile::tempdir().unwrap();
        assert_ne!(
            part_path(dir.path(), "tiny.en", 1),
            part_path(dir.path(), "tiny.en", 2)
        );
        assert_ne!(next_id(), next_id());
        let other = part_path(dir.path(), "base.en", 7);
        fs::write(&other, b"another download's bytes").unwrap();
        let (base, _asked, release) = serve(Answer::Hold {
            length: 5_000_000,
            body: vec![7u8; 100_000],
        });
        let cancel = AtomicBool::new(false);
        let result = download_from(
            &base,
            "base.en",
            dir.path(),
            8,
            |_, _| cancel.store(true, Ordering::Relaxed),
            &cancel,
        );
        let _ = release.send(());
        assert_eq!(result, Err(DownloadError::Cancelled));
        assert!(!part_path(dir.path(), "base.en", 8).exists());
        assert_eq!(fs::read(&other).unwrap(), b"another download's bytes");
        // Not a model either: model_files lists only whole ggml-*.bin.
        assert!(!other.to_string_lossy().ends_with(".bin"));
    }

    /// A read that stalls past the stall time fails the download, and its
    /// `.part` goes.
    #[test]
    fn a_stalled_read_fails_and_leaves_nothing() {
        let (base, _asked, release) = serve(Answer::Hold {
            length: 5_000_000,
            body: vec![7u8; 1_000],
        });
        let dir = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        let result = fetch(
            (&base, Duration::from_millis(400)),
            "tiny",
            dir.path(),
            9,
            |_, _| {},
            &AtomicBool::new(false),
        );
        let _ = release.send(());
        match result {
            Err(DownloadError::Failed(why)) => assert!(why.contains("interrupted"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(left(dir.path()).is_empty(), "{:?}", left(dir.path()));
        assert_eq!(STALL, Duration::from_secs(10));
    }

    #[test]
    fn a_name_is_never_a_path() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["", "../x", "a/b", ".hidden"] {
            let result = download_from(
                "http://127.0.0.1:9",
                name,
                dir.path(),
                4,
                |_, _| {},
                &AtomicBool::new(false),
            );
            assert!(matches!(result, Err(DownloadError::Failed(_))), "{name}");
        }
    }
}
