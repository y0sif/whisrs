//! The API key test behind `t`: one authenticated request that sends no
//! audio and costs nothing, to say whether a key works before a dictation
//! finds out.
//!
//! Groq and OpenAI are asked for their model list (`GET /models`, Bearer),
//! Deepgram for the key's projects (`GET /v1/projects`, `Token`, as
//! `transcription/deepgram.rs` sends it). An ASR sidecar gets half a second
//! of silence, a local whisper model is checked for the GGML magic, and an
//! OpenAI-compatible realtime server is not testable without a session.
//!
//! The key tested is the one the daemon would use: a non-empty
//! `WHISRS_*_API_KEY` wins over config.toml. It goes into the request header
//! and nowhere else: never into a message, a log line or an error.

use std::fmt;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

use reqwest::header::{HeaderValue, AUTHORIZATION};

use crate::Config;

/// How long a test waits for an answer.
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// A provider whose keys are tested with one GET.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Groq,
    OpenAi,
    Deepgram,
}

impl Provider {
    /// The provider a transcription or TTS backend name uses.
    pub fn of_backend(backend: &str) -> Option<Provider> {
        match backend {
            "groq" => Some(Provider::Groq),
            "openai" | "openai-realtime" => Some(Provider::OpenAi),
            "deepgram" | "deepgram-streaming" => Some(Provider::Deepgram),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Provider::Groq => "groq",
            Provider::OpenAi => "openai",
            Provider::Deepgram => "deepgram",
        }
    }

    /// The env var that wins over config.toml for this provider's key.
    pub fn env_var(self) -> &'static str {
        match self {
            Provider::Groq => "WHISRS_GROQ_API_KEY",
            Provider::OpenAi => "WHISRS_OPENAI_API_KEY",
            Provider::Deepgram => "WHISRS_DEEPGRAM_API_KEY",
        }
    }

    /// This provider's key in `config`, if the section has one.
    fn config_key(self, config: &Config) -> Option<&str> {
        match self {
            Provider::Groq => config.groq.as_ref().map(|g| g.api_key.as_str()),
            Provider::OpenAi => config.openai.as_ref().map(|o| o.api_key.as_str()),
            Provider::Deepgram => config.deepgram.as_ref().map(|d| d.api_key.as_str()),
        }
    }
}

/// Where each provider's test request goes. Tests point them at a local
/// server.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub groq: String,
    pub openai: String,
    pub deepgram: String,
    pub timeout: Duration,
}

impl Default for Endpoints {
    fn default() -> Endpoints {
        Endpoints {
            groq: "https://api.groq.com/openai/v1/models".to_string(),
            openai: "https://api.openai.com/v1/models".to_string(),
            deepgram: "https://api.deepgram.com/v1/projects".to_string(),
            timeout: TIMEOUT,
        }
    }
}

impl Endpoints {
    fn of(&self, provider: Provider) -> &str {
        match provider {
            Provider::Groq => &self.groq,
            Provider::OpenAi => &self.openai,
            Provider::Deepgram => &self.deepgram,
        }
    }
}

/// Whose key to test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The key (or model, or server) a transcription backend uses.
    Backend(String),
    /// The key `[tts]` reads aloud with.
    Tts,
    /// `[llm] api_key`, for an OpenAI or Groq `api_url`.
    Llm,
}

/// What a test found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The provider took the key; how long it took to say so.
    Valid { ms: u64 },
    /// 401: the provider does not know this key.
    Rejected,
    /// 403: a real key without access to this.
    Limited,
    /// No answer, and why.
    Unreachable(String),
    /// An answer this does not expect.
    Unexpected(u16),
    /// There is no key to test.
    NoKey,
    /// The key holds characters no key has (a space, a line break).
    Malformed,
    /// The local whisper model file, checked.
    Model(Model),
    /// Nothing here can be tested, and why.
    NotTestable(&'static str),
}

/// What is at `[local-whisper] model_path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Model {
    /// A GGML file this long.
    Ggml(u64),
    /// A file, but not a whisper.cpp model.
    NotGgml,
    /// Not there, or not readable, and why.
    Missing(String),
}

impl Outcome {
    /// Whether the test passed.
    pub fn passed(&self) -> bool {
        matches!(self, Outcome::Valid { .. } | Outcome::Model(Model::Ggml(_)))
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Valid { ms } => write!(f, "valid, {ms} ms"),
            Outcome::Rejected => f.write_str("rejected: the provider does not know this key (401)"),
            Outcome::Limited => f.write_str("limited: a real key, without access to this (403)"),
            Outcome::Unreachable(why) => write!(f, "unreachable: {why}"),
            Outcome::Unexpected(status) => write!(f, "unexpected answer: HTTP {status}"),
            Outcome::NoKey => f.write_str("no key to test"),
            Outcome::Malformed => {
                f.write_str("not a key: it holds a space, a line break or another odd character")
            }
            Outcome::Model(Model::Ggml(bytes)) => {
                write!(f, "a whisper.cpp model, {} MB", bytes / 1_000_000)
            }
            Outcome::Model(Model::NotGgml) => f.write_str("not a whisper.cpp model file"),
            Outcome::Model(Model::Missing(why)) => f.write_str(why),
            Outcome::NotTestable(why) => write!(f, "not testable: {why}"),
        }
    }
}

/// Which key was tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The env var, which wins as it does in the daemon.
    Env(&'static str),
    /// config.toml (as edited).
    Config,
    /// No key was involved.
    None,
}

/// A test's outcome, and whose key it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tested {
    pub outcome: Outcome,
    pub source: Source,
}

/// Test `target` in `config`, the key resolved with `env` as the daemon
/// resolves it.
pub fn check(
    config: &Config,
    target: &Target,
    env: impl Fn(&str) -> Option<String>,
    endpoints: &Endpoints,
) -> Tested {
    let untested = |why| Tested {
        outcome: Outcome::NotTestable(why),
        source: Source::None,
    };
    match target {
        Target::Backend(backend) => match backend.as_str() {
            "asr-sidecar" | "asr" | "vibevoice" => sidecar(config, &env, endpoints.timeout),
            "local-whisper" | "local" => Tested {
                outcome: Outcome::Model(model_file(&model_path(config))),
                source: Source::None,
            },
            "openai-compatible-realtime" => {
                untested("a realtime server of your own answers only a live session")
            }
            "local-vosk" | "local-parakeet" => untested("this backend is not implemented yet"),
            name => match Provider::of_backend(name) {
                Some(provider) => provider_key(config, provider, &env, endpoints),
                None => untested("whisrs does not know this backend"),
            },
        },
        Target::Tts => {
            let backend = config.tts.as_ref().map_or("groq", |t| t.backend.as_str());
            let Some(provider) = Provider::of_backend(backend) else {
                return untested("a local TTS server needs no key");
            };
            match config
                .tts
                .as_ref()
                .and_then(|t| t.api_key.as_deref())
                .filter(|k| !k.is_empty())
            {
                Some(key) => Tested {
                    outcome: request(endpoints.of(provider), provider, key, endpoints.timeout),
                    source: Source::Config,
                },
                None => provider_key(config, provider, &env, endpoints),
            }
        }
        Target::Llm => {
            let llm = config.llm.clone().unwrap_or_default();
            let host = reqwest::Url::parse(llm.api_url.trim())
                .ok()
                .and_then(|url| url.host_str().map(str::to_ascii_lowercase));
            let provider = match host.as_deref() {
                Some("api.openai.com") => Provider::OpenAi,
                Some("api.groq.com") => Provider::Groq,
                _ => return untested("only OpenAI and Groq LLM keys are tested"),
            };
            let (key, source) = llm_key(&llm, &env);
            if key.is_empty() {
                return Tested {
                    outcome: Outcome::NoKey,
                    source: Source::None,
                };
            }
            Tested {
                outcome: request(endpoints.of(provider), provider, &key, endpoints.timeout),
                source,
            }
        }
    }
}

/// The LLM key the daemon uses (`llm::resolve_api_key`): `[llm] api_key`
/// when it is not empty, else `WHISRS_OPENAI_API_KEY` when it is set (even
/// empty), else `WHISRS_GROQ_API_KEY`; and where it came from.
fn llm_key(llm: &crate::llm::LlmConfig, env: &impl Fn(&str) -> Option<String>) -> (String, Source) {
    if !llm.api_key.is_empty() {
        return (llm.api_key.clone(), Source::Config);
    }
    for var in ["WHISRS_OPENAI_API_KEY", "WHISRS_GROQ_API_KEY"] {
        if let Some(key) = env(var) {
            return (key, Source::Env(var));
        }
    }
    (String::new(), Source::None)
}

/// Test `provider`'s key: the env var when set, else config.toml's.
fn provider_key(
    config: &Config,
    provider: Provider,
    env: &impl Fn(&str) -> Option<String>,
    endpoints: &Endpoints,
) -> Tested {
    let var = provider.env_var();
    let (key, source) = match env(var).filter(|key| !key.is_empty()) {
        Some(key) => (key, Source::Env(var)),
        None => match provider.config_key(config).filter(|key| !key.is_empty()) {
            Some(key) => (key.to_string(), Source::Config),
            None => {
                return Tested {
                    outcome: Outcome::NoKey,
                    source: Source::None,
                }
            }
        },
    };
    Tested {
        outcome: request(endpoints.of(provider), provider, &key, endpoints.timeout),
        source,
    }
}

/// One GET to `url` with `key` as `provider` takes it.
pub fn request(url: &str, provider: Provider, key: &str, timeout: Duration) -> Outcome {
    // A header cannot carry it, and no provider issues such a key.
    if key.is_empty() || !key.bytes().all(|b| b.is_ascii_graphic()) {
        return Outcome::Malformed;
    }
    let client = match client(timeout) {
        Ok(client) => client,
        Err(why) => return Outcome::Unreachable(why),
    };
    let request = match provider {
        Provider::Groq | Provider::OpenAi => client.get(url).bearer_auth(key),
        Provider::Deepgram => {
            let Ok(mut value) = HeaderValue::from_str(&["Token ", key].concat()) else {
                return Outcome::Malformed;
            };
            value.set_sensitive(true);
            client.get(url).header(AUTHORIZATION, value)
        }
    };
    answer(request, timeout)
}

/// Half a second of silence to the ASR sidecar, as the backend posts audio.
fn sidecar(config: &Config, env: &impl Fn(&str) -> Option<String>, timeout: Duration) -> Tested {
    let var = "WHISRS_ASR_SIDECAR_API_KEY";
    let file = config
        .asr_sidecar
        .as_ref()
        .and_then(|s| s.api_key.as_deref())
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string);
    let (key, source) = match env(var)
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
    {
        Some(key) => (Some(key), Source::Env(var)),
        None => {
            let source = if file.is_some() {
                Source::Config
            } else {
                Source::None
            };
            (file, source)
        }
    };
    let sidecar = config
        .asr_sidecar
        .clone()
        .unwrap_or_else(|| toml::from_str("").expect("an empty [asr-sidecar] section parses"));
    Tested {
        outcome: post_silence(
            &sidecar.url,
            &sidecar.model,
            &config.general.language,
            key.as_deref(),
            timeout,
        ),
        source,
    }
}

/// POST half a second of 16 kHz silence to `url`, as `audio.wav`.
fn post_silence(
    url: &str,
    model: &str,
    language: &str,
    key: Option<&str>,
    timeout: Duration,
) -> Outcome {
    if key.is_some_and(|key| !key.bytes().all(|b| b.is_ascii_graphic())) {
        return Outcome::Malformed;
    }
    let wav = match crate::audio::wav::encode_wav(&[0i16; 8_000]) {
        Ok(wav) => wav,
        Err(e) => return Outcome::Unreachable(format!("cannot make the test audio: {e}")),
    };
    let client = match client(timeout) {
        Ok(client) => client,
        Err(why) => return Outcome::Unreachable(why),
    };
    let part = match reqwest::blocking::multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")
    {
        Ok(part) => part,
        Err(e) => return Outcome::Unreachable(e.to_string()),
    };
    let mut form = reqwest::blocking::multipart::Form::new()
        .part("file", part)
        .text("model", model.to_string());
    if language != "auto" {
        form = form.text("language", language.to_string());
    }
    let mut request = client.post(url.trim_end_matches('/')).multipart(form);
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    answer(request, timeout)
}

fn client(timeout: Duration) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent("whisrs")
        .connect_timeout(timeout)
        .timeout(timeout)
        .build()
        .map_err(|e| format!("cannot build an HTTP client: {e}"))
}

/// Send `request` and read its status.
fn answer(request: reqwest::blocking::RequestBuilder, timeout: Duration) -> Outcome {
    let start = Instant::now();
    match request.send() {
        Ok(response) => match response.status().as_u16() {
            200..=299 => Outcome::Valid {
                ms: start.elapsed().as_millis() as u64,
            },
            401 => Outcome::Rejected,
            403 => Outcome::Limited,
            status => Outcome::Unexpected(status),
        },
        Err(e) if e.is_timeout() => {
            Outcome::Unreachable(format!("no answer in {} s", timeout.as_secs_f32()))
        }
        Err(e) if e.is_connect() => {
            Outcome::Unreachable(match e.url().and_then(|u| u.host_str()) {
                Some(host) => format!("cannot connect to {host}"),
                None => "cannot connect".to_string(),
            })
        }
        // The text names the URL, never a header.
        Err(e) => Outcome::Unreachable(e.without_url().to_string()),
    }
}

/// `[local-whisper] model_path`, or the default the daemon falls back to.
fn model_path(config: &Config) -> String {
    config
        .local_whisper
        .as_ref()
        .map(|l| l.model_path.clone())
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(crate::config::types::default_whisper_model_path)
}

/// Whether `path` is a whisper.cpp model: it starts with the GGML magic
/// (`0x67676d6c` little-endian, `lmgg` on disk).
pub fn model_file(path: &str) -> Model {
    let mut magic = [0u8; 4];
    let read = std::fs::File::open(Path::new(path)).and_then(|mut file| {
        file.read_exact(&mut magic)?;
        file.metadata()
    });
    match read {
        Ok(meta) if &magic == b"lmgg" => Model::Ggml(meta.len()),
        Ok(_) => Model::NotGgml,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Model::NotGgml,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Model::Missing(format!("no model at {path}"))
        }
        Err(e) => Model::Missing(format!("cannot read {path}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    use super::*;

    /// How the fake server meets its one connection.
    enum Meet {
        Status(u16),
        /// Close without answering.
        Drop,
        /// Read the request, then say nothing.
        Silent,
    }

    /// What the server read: the request line, its Authorization header,
    /// and whether the body held a WAV.
    #[derive(Debug, Default)]
    struct Asked {
        line: String,
        auth: Option<String>,
        wav: bool,
    }

    fn serve(meet: Meet) -> (String, mpsc::Receiver<Asked>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            if matches!(meet, Meet::Drop) {
                return;
            }
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut asked = Asked::default();
            reader.read_line(&mut asked.line).unwrap();
            let mut length = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(value) = lower.strip_prefix("authorization: ") {
                    // Kept as sent, for the test to compare.
                    let at = line.len() - value.len();
                    asked.auth = Some(line[at..].trim_end().to_string());
                }
                if let Some(value) = lower.strip_prefix("content-length: ") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).unwrap();
            asked.wav = body.windows(4).any(|w| w == b"RIFF");
            let _ = tx.send(asked);
            match meet {
                Meet::Status(code) => {
                    let head = format!(
                        "HTTP/1.1 {code} X\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                    );
                    stream.write_all(head.as_bytes()).unwrap();
                }
                Meet::Silent => std::thread::sleep(Duration::from_secs(3)),
                Meet::Drop => {}
            }
        });
        (base, rx)
    }

    fn endpoints(base: &str) -> Endpoints {
        Endpoints {
            groq: format!("{base}/openai/v1/models"),
            openai: format!("{base}/v1/models"),
            deepgram: format!("{base}/v1/projects"),
            timeout: Duration::from_millis(800),
        }
    }

    fn config(toml: &str) -> Config {
        toml::from_str(toml).unwrap()
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// Each status reads as what it means, and the key goes as the
    /// provider takes it.
    #[test]
    fn statuses_read_as_outcomes() {
        for (code, want) in [
            (401, Outcome::Rejected),
            (403, Outcome::Limited),
            (500, Outcome::Unexpected(500)),
        ] {
            let (base, _asked) = serve(Meet::Status(code));
            let c = config("[groq]\napi_key = \"gsk_test\"\n");
            let tested = check(
                &c,
                &Target::Backend("groq".into()),
                no_env,
                &endpoints(&base),
            );
            assert_eq!(tested.outcome, want);
        }

        let (base, asked) = serve(Meet::Status(200));
        let c = config(
            "[general]\nbackend = \"deepgram-streaming\"\n[deepgram]\napi_key = \"dg_test\"\n",
        );
        let tested = check(
            &c,
            &Target::Backend("deepgram-streaming".into()),
            no_env,
            &endpoints(&base),
        );
        assert!(
            matches!(tested.outcome, Outcome::Valid { .. }),
            "{:?}",
            tested.outcome
        );
        assert_eq!(tested.source, Source::Config);
        let asked = asked.recv().unwrap();
        assert!(
            asked.line.starts_with("GET /v1/projects "),
            "{}",
            asked.line
        );
        assert!(
            asked.auth.as_deref() == Some("Token dg_test"),
            "the Deepgram header form"
        );

        let (base, asked) = serve(Meet::Status(200));
        let c = config("[openai]\napi_key = \"sk_test\"\n");
        check(
            &c,
            &Target::Backend("openai-realtime".into()),
            no_env,
            &endpoints(&base),
        );
        let asked = asked.recv().unwrap();
        assert!(asked.line.starts_with("GET /v1/models "), "{}", asked.line);
        assert!(
            asked.auth.as_deref() == Some("Bearer sk_test"),
            "the Bearer header form"
        );
    }

    /// A dropped connection and a server that never answers are
    /// unreachable, within the timeout.
    #[test]
    fn no_answer_is_unreachable() {
        let c = config("[groq]\napi_key = \"gsk_test\"\n");
        let (base, _asked) = serve(Meet::Drop);
        let tested = check(
            &c,
            &Target::Backend("groq".into()),
            no_env,
            &endpoints(&base),
        );
        assert!(
            matches!(tested.outcome, Outcome::Unreachable(_)),
            "{:?}",
            tested.outcome
        );

        let (base, _asked) = serve(Meet::Silent);
        let start = Instant::now();
        let tested = check(
            &c,
            &Target::Backend("groq".into()),
            no_env,
            &endpoints(&base),
        );
        match tested.outcome {
            Outcome::Unreachable(why) => assert!(why.starts_with("no answer in"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    /// The env var wins over config.toml, as in the daemon; nothing at all
    /// is no key, and no request.
    #[test]
    fn the_env_key_wins_as_in_the_daemon() {
        let (base, asked) = serve(Meet::Status(200));
        let c = config("[groq]\napi_key = \"from_file\"\n");
        let env = |var: &str| (var == "WHISRS_GROQ_API_KEY").then(|| "from_env".to_string());
        let tested = check(&c, &Target::Backend("groq".into()), env, &endpoints(&base));
        assert_eq!(tested.source, Source::Env("WHISRS_GROQ_API_KEY"));
        assert!(
            asked.recv().unwrap().auth.as_deref() == Some("Bearer from_env"),
            "env key sent"
        );

        // An empty env var does not count.
        let (base, asked) = serve(Meet::Status(200));
        let empty = |_: &str| Some(String::new());
        let tested = check(
            &c,
            &Target::Backend("groq".into()),
            empty,
            &endpoints(&base),
        );
        assert_eq!(tested.source, Source::Config);
        assert!(
            asked.recv().unwrap().auth.as_deref() == Some("Bearer from_file"),
            "file key sent"
        );

        let none = check(
            &config(""),
            &Target::Backend("groq".into()),
            no_env,
            &endpoints("http://127.0.0.1:9"),
        );
        assert_eq!(none.outcome, Outcome::NoKey);
        let pasted = check(
            &config("[groq]\napi_key = \"gsk 1\\n\"\n"),
            &Target::Backend("groq".into()),
            no_env,
            &endpoints("http://127.0.0.1:9"),
        );
        assert_eq!(pasted.outcome, Outcome::Malformed);
    }

    /// The TTS key is `[tts] api_key`, else its backend's own; the LLM key
    /// is tested on OpenAI and Groq only.
    #[test]
    fn tts_and_llm_keys() {
        let (base, asked) = serve(Meet::Status(200));
        let c = config("[tts]\nbackend = \"openai\"\n[openai]\napi_key = \"sk_tts\"\n");
        let tested = check(&c, &Target::Tts, no_env, &endpoints(&base));
        assert!(tested.outcome.passed());
        assert!(asked.recv().unwrap().line.starts_with("GET /v1/models "));

        let sidecar = config("[tts]\nbackend = \"tts-sidecar\"\n");
        assert!(matches!(
            check(&sidecar, &Target::Tts, no_env, &endpoints(&base)).outcome,
            Outcome::NotTestable(_)
        ));

        let (base, asked) = serve(Meet::Status(401));
        let groq = config(
            "[llm]\napi_key = \"gsk_llm\"\napi_url = \"https://api.groq.com/openai/v1/chat/completions\"\n",
        );
        assert_eq!(
            check(&groq, &Target::Llm, no_env, &endpoints(&base)).outcome,
            Outcome::Rejected
        );
        assert!(asked
            .recv()
            .unwrap()
            .line
            .starts_with("GET /openai/v1/models "));
        // No [llm] api_key: the daemon's env fallback, OpenAI's var first,
        // and set but empty it stops there, as in the daemon.
        let groq_url =
            config("[llm]\napi_url = \"https://api.groq.com/openai/v1/chat/completions\"\n");
        let (base, asked) = serve(Meet::Status(200));
        let both = |var: &str| match var {
            "WHISRS_OPENAI_API_KEY" => Some("sk_env".to_string()),
            "WHISRS_GROQ_API_KEY" => Some("gsk_env".to_string()),
            _ => None,
        };
        let tested = check(&groq_url, &Target::Llm, both, &endpoints(&base));
        assert_eq!(tested.source, Source::Env("WHISRS_OPENAI_API_KEY"));
        assert!(
            asked.recv().unwrap().auth.as_deref() == Some("Bearer sk_env"),
            "the OpenAI var is sent first"
        );
        let (base, asked) = serve(Meet::Status(200));
        let groq_only = |var: &str| (var == "WHISRS_GROQ_API_KEY").then(|| "gsk_env".to_string());
        let tested = check(&groq_url, &Target::Llm, groq_only, &endpoints(&base));
        assert_eq!(tested.source, Source::Env("WHISRS_GROQ_API_KEY"));
        assert!(asked.recv().unwrap().auth.is_some(), "the Groq var is sent");
        let empty_openai = |var: &str| match var {
            "WHISRS_OPENAI_API_KEY" => Some(String::new()),
            _ => Some("gsk_env".to_string()),
        };
        let tested = check(&groq_url, &Target::Llm, empty_openai, &endpoints(&base));
        assert_eq!(tested.outcome, Outcome::NoKey);
        // A key in the file wins over both.
        let (base, asked) = serve(Meet::Status(200));
        let filed = config(
            "[llm]\napi_key = \"gsk_file\"\napi_url = \"https://api.groq.com/openai/v1/chat/completions\"\n",
        );
        let tested = check(&filed, &Target::Llm, both, &endpoints(&base));
        assert_eq!(tested.source, Source::Config);
        assert!(
            asked.recv().unwrap().auth.as_deref() == Some("Bearer gsk_file"),
            "the file key is sent"
        );

        let other = config(
            "[llm]\napi_key = \"k\"\napi_url = \"https://openrouter.ai/api/v1/chat/completions\"\n",
        );
        assert!(matches!(
            check(&other, &Target::Llm, no_env, &endpoints(&base)).outcome,
            Outcome::NotTestable(_)
        ));
    }

    /// The sidecar gets half a second of silence, with its bearer token.
    #[test]
    fn the_sidecar_gets_silence() {
        let (base, asked) = serve(Meet::Status(200));
        let c = config(&format!(
            "[asr-sidecar]\nurl = \"{base}/transcribe\"\napi_key = \" tok \"\n"
        ));
        let tested = check(
            &c,
            &Target::Backend("asr-sidecar".into()),
            no_env,
            &endpoints(&base),
        );
        assert!(tested.outcome.passed(), "{:?}", tested.outcome);
        assert_eq!(tested.source, Source::Config);
        let asked = asked.recv().unwrap();
        assert!(
            asked.line.starts_with("POST /transcribe "),
            "{}",
            asked.line
        );
        assert!(asked.wav);
        assert!(
            asked.auth.as_deref() == Some("Bearer tok"),
            "the trimmed token"
        );

        let (base, _asked) = serve(Meet::Status(403));
        let c = config(&format!("[asr-sidecar]\nurl = \"{base}/transcribe\"\n"));
        let tested = check(
            &c,
            &Target::Backend("asr-sidecar".into()),
            no_env,
            &endpoints(&base),
        );
        assert_eq!(tested.outcome, Outcome::Limited);
        assert_eq!(tested.source, Source::None);
    }

    #[test]
    fn a_local_model_is_checked_for_the_ggml_magic() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("ggml-base.en.bin");
        std::fs::write(&model, b"lmgg and the rest").unwrap();
        let path = model.to_str().unwrap();
        let c = config(&format!("[local-whisper]\nmodel_path = \"{path}\"\n"));
        let tested = check(
            &c,
            &Target::Backend("local-whisper".into()),
            no_env,
            &Endpoints::default(),
        );
        assert_eq!(tested.outcome, Outcome::Model(Model::Ggml(17)));
        std::fs::write(&model, b"<html>").unwrap();
        assert_eq!(model_file(path), Model::NotGgml);
        std::fs::write(&model, b"lm").unwrap();
        assert_eq!(model_file(path), Model::NotGgml);
        assert!(matches!(
            model_file("/no/such/model.bin"),
            Model::Missing(_)
        ));

        let realtime = check(
            &config(""),
            &Target::Backend("openai-compatible-realtime".into()),
            no_env,
            &Endpoints::default(),
        );
        assert!(matches!(realtime.outcome, Outcome::NotTestable(_)));
    }

    /// One live check per provider, run by hand with the provider's key in
    /// its env var:
    /// `cargo test --lib live_ -- --ignored --nocapture --test-threads=1`.
    /// Prints the outcome only, never the key; a made-up key must be
    /// rejected.
    fn live(provider: Provider) {
        let var = provider.env_var();
        let Some(key) = std::env::var(var).ok().filter(|k| !k.is_empty()) else {
            println!("{}: skipped, {var} is not set", provider.name());
            return;
        };
        let endpoints = Endpoints::default();
        let real = request(endpoints.of(provider), provider, &key, TIMEOUT);
        let made_up = request(
            endpoints.of(provider),
            provider,
            "whisrs-not-a-key",
            TIMEOUT,
        );
        // Fixed words only: nothing from the requests is printed.
        println!(
            "{}: the key from {var}: {}; a made-up key: {}",
            provider.name(),
            label(&real),
            label(&made_up)
        );
        assert!(real.passed(), "the real key did not pass");
        assert_eq!(made_up, Outcome::Rejected);
    }

    /// An outcome as a fixed word, carrying nothing from the request.
    fn label(outcome: &Outcome) -> &'static str {
        match outcome {
            Outcome::Valid { .. } => "valid",
            Outcome::Rejected => "rejected (401)",
            Outcome::Limited => "limited (403)",
            Outcome::Unreachable(_) => "unreachable",
            Outcome::Unexpected(_) => "an unexpected status",
            Outcome::NoKey => "no key",
            Outcome::Malformed => "not a key",
            Outcome::Model(_) => "a model file",
            Outcome::NotTestable(_) => "not testable",
        }
    }

    #[test]
    #[ignore = "asks api.groq.com with WHISRS_GROQ_API_KEY"]
    fn live_groq_key() {
        live(Provider::Groq);
    }

    #[test]
    #[ignore = "asks api.openai.com with WHISRS_OPENAI_API_KEY"]
    fn live_openai_key() {
        live(Provider::OpenAi);
    }

    #[test]
    #[ignore = "asks api.deepgram.com with WHISRS_DEEPGRAM_API_KEY"]
    fn live_deepgram_key() {
        live(Provider::Deepgram);
    }
}
