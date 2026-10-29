//! API keys as the interactive flows show them: masked to their last four
//! characters, never in full.

use anyhow::{Context, Result};

use crate::Config;

/// Mask an API key for display, showing only the last 4 characters.
///
/// Counted in characters, not bytes: slicing the last four bytes panicked on
/// a key that ends in a multi-byte character, such as one pasted with a stray
/// `…` on the end.
pub fn mask_api_key(key: &str) -> String {
    let len = key.chars().count();
    if len <= 4 {
        "****".to_string()
    } else {
        let tail: String = key.chars().skip(len - 4).collect();
        format!("****{tail}")
    }
}

/// Where the active backend's API key comes from, as `whisrs config` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySummary {
    /// A `WHISRS_*_API_KEY` env var, which wins over config.toml as it does
    /// in the daemon. Shown as "this shell": a daemon run by systemd takes its
    /// env from the user manager and may not see it.
    Env { var: &'static str, masked: String },
    /// The key in config.toml.
    File { masked: String },
    /// A local backend, which needs no key.
    NotNeeded,
    /// No key, and the backend works without one. Names what it would be:
    /// `"bearer token"` or `"API key"`.
    Optional(&'static str),
    /// No key anywhere.
    NotSet,
}

/// Summarize whether the active backend has an API key configured, from the
/// process environment and `config`.
pub fn key_summary(config: &Config) -> KeySummary {
    key_summary_from(config, |name| std::env::var(name).ok())
}

/// [`key_summary`] with the environment passed in, so tests never touch the
/// process env. Mirrors the daemon's resolvers in `factory.rs`: a
/// `WHISRS_*_API_KEY` env var wins over `config.toml`.
pub fn key_summary_from(config: &Config, env: impl Fn(&str) -> Option<String>) -> KeySummary {
    let from_env = |var: &'static str, key: &str| KeySummary::Env {
        var,
        masked: mask_api_key(key),
    };
    let (var, key) = match config.general.backend.as_str() {
        "groq" => (
            "WHISRS_GROQ_API_KEY",
            config.groq.as_ref().map(|g| g.api_key.as_str()),
        ),
        "deepgram" | "deepgram-streaming" => (
            "WHISRS_DEEPGRAM_API_KEY",
            config.deepgram.as_ref().map(|d| d.api_key.as_str()),
        ),
        "openai" | "openai-realtime" => (
            "WHISRS_OPENAI_API_KEY",
            config.openai.as_ref().map(|o| o.api_key.as_str()),
        ),
        "local-whisper" | "local" | "local-vosk" | "local-parakeet" => {
            return KeySummary::NotNeeded;
        }
        "openai-compatible-realtime" => {
            return match config
                .openai_compatible_realtime
                .as_ref()
                .and_then(|r| r.api_key.as_deref())
                .filter(|key| !key.is_empty())
            {
                Some(key) => KeySummary::File {
                    masked: mask_api_key(key),
                },
                None => KeySummary::Optional("bearer token"),
            };
        }
        "asr-sidecar" | "asr" | "vibevoice" => {
            // Unlike the other backends, the daemon trims both sides here and
            // treats a blank value as absent.
            let var = "WHISRS_ASR_SIDECAR_API_KEY";
            if let Some(key) = env(var).filter(|key| !key.trim().is_empty()) {
                return from_env(var, key.trim());
            }
            return match config
                .asr_sidecar
                .as_ref()
                .and_then(|s| s.api_key.as_deref())
                .filter(|key| !key.trim().is_empty())
            {
                Some(key) => KeySummary::File {
                    masked: mask_api_key(key),
                },
                None => KeySummary::Optional("API key"),
            };
        }
        _ => return KeySummary::NotSet,
    };
    if let Some(env_key) = env(var).filter(|key| !key.is_empty()) {
        return from_env(var, &env_key);
    }
    match key {
        Some(k) if !k.is_empty() => KeySummary::File {
            masked: mask_api_key(k),
        },
        _ => KeySummary::NotSet,
    }
}

/// Serialize a copy of the config with API keys replaced by `****<last4>`.
///
/// We clone before masking so the in-memory edit buffer keeps real keys.
pub fn render_masked_toml(config: &Config) -> Result<String> {
    let mut clone = config.clone();
    if let Some(d) = clone.deepgram.as_mut() {
        d.api_key = mask_api_key(&d.api_key);
    }
    if let Some(g) = clone.groq.as_mut() {
        g.api_key = mask_api_key(&g.api_key);
    }
    if let Some(o) = clone.openai.as_mut() {
        o.api_key = mask_api_key(&o.api_key);
    }
    if let Some(r) = clone.openai_compatible_realtime.as_mut() {
        r.api_key = r.api_key.as_ref().map(|key| mask_api_key(key));
    }
    if let Some(s) = clone.asr_sidecar.as_mut() {
        s.api_key = s.api_key.as_ref().map(|key| mask_api_key(key));
    }
    if let Some(l) = clone.llm.as_mut() {
        l.api_key = mask_api_key(&l.api_key);
    }
    if let Some(t) = clone.tts.as_mut() {
        t.api_key = t.api_key.as_ref().map(|key| mask_api_key(key));
    }
    toml::to_string_pretty(&clone).context("failed to serialize config")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ASCII keys keep the masking they always had.
    #[test]
    fn mask_api_key_shows_the_last_four_characters() {
        assert_eq!(mask_api_key("gsk_abcdef1234"), "****1234");
        assert_eq!(mask_api_key("abcde"), "****bcde");
        assert_eq!(mask_api_key("abcd"), "****");
        assert_eq!(mask_api_key(""), "****");
    }

    /// A key ending in multi-byte characters used to panic: the cut fell
    /// four bytes from the end, inside a character. It is four characters now.
    #[test]
    fn mask_api_key_counts_characters_not_bytes() {
        assert_eq!(mask_api_key("sk-abc€€"), "****bc€€");
        assert_eq!(mask_api_key("key-ключ"), "****ключ");
        assert_eq!(mask_api_key("sk-12345🔑"), "****345🔑");
        // Four characters or fewer, however many bytes, show nothing.
        assert_eq!(mask_api_key("€€€€"), "****");
        // Every cut point of a mixed key, so no length lands mid-character.
        let key = "a€b🔑cdé…";
        for end in key.char_indices().map(|(i, _)| i).chain([key.len()]) {
            let masked = mask_api_key(&key[..end]);
            assert!(masked.starts_with("****"), "{masked}");
            assert!(masked.chars().count() <= 8, "{masked}");
        }
    }

    /// Every `Config` field whose section carries an `api_key`, discovered from
    /// the struct definitions rather than a hand-written list. `[tts]` was
    /// missed by `render_masked_toml` for six releases precisely because the
    /// set of key-bearing sections lived only in that function's body.
    fn key_bearing_config_fields() -> Vec<String> {
        let types_src = include_str!("types.rs");

        let mut with_key: Vec<&str> = Vec::new();
        for src in [types_src, include_str!("../llm.rs")] {
            for chunk in src.split("\npub struct ").skip(1) {
                let name = chunk
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or_default();
                let body = chunk.split("\n}").next().unwrap_or_default();
                if body.contains("pub api_key") {
                    with_key.push(name);
                }
            }
        }
        assert_eq!(
            with_key.len(),
            7,
            "the struct scan found {with_key:?}; it only sees column-0 `pub struct` \
             in types.rs and llm.rs, so a key-bearing struct that moved elsewhere is \
             invisible here and would pass vacuously"
        );

        let config_body = types_src
            .split("\npub struct Config {")
            .nth(1)
            .expect("types.rs defines Config")
            .split("\n}")
            .next()
            .expect("Config has a body");

        let mut fields = Vec::new();
        for line in config_body.lines() {
            let Some(rest) = line.trim().strip_prefix("pub ") else {
                continue;
            };
            let Some((name, ty)) = rest.split_once(':') else {
                continue;
            };
            if with_key.iter().any(|s| ty.contains(s)) {
                fields.push(name.trim().to_string());
            }
        }
        fields
    }

    /// `render_masked_toml` masks a hand-written list of sections, so a new
    /// key-bearing section is silently printed in cleartext under a heading
    /// that promises "masked". Assert the function touches every section the
    /// struct definitions say carries a key.
    ///
    /// Structural only: it proves each section is *mentioned*, not that the
    /// mention masks anything. `a_populated_config_renders_no_cleartext_key`
    /// is the semantic half, and neither test replaces the other.
    #[test]
    fn render_masked_toml_covers_every_key_bearing_section() {
        let source = include_str!("keys.rs");
        let body = source
            .split("fn render_masked_toml(")
            .nth(1)
            .expect("keys.rs defines render_masked_toml")
            .split("\n}\n")
            .next()
            .expect("render_masked_toml has a body");

        let fields = key_bearing_config_fields();
        for field in &fields {
            // The trailing dot is load-bearing: `clone.openai` is a prefix of
            // `clone.openai_compatible_realtime`, so an unanchored match let a
            // deleted `[openai]` arm pass this test.
            assert!(
                body.contains(&format!("clone.{field}.")),
                "render_masked_toml never masks `[{field}] api_key`, so the masked \
                 render shows it in cleartext"
            );
        }
        assert_eq!(
            fields.len(),
            7,
            "expected 7 key-bearing sections, found {fields:?}; update this count \
             deliberately once the new section is masked above"
        );
    }

    /// The value half: a config with a key in every section must render with
    /// no secret surviving, and with every `api_key` still present and masked.
    /// A section dropped from serialization would otherwise pass the scan test.
    #[test]
    fn a_populated_config_renders_no_cleartext_key() {
        let config: Config = toml::from_str(
            r#"
[deepgram]
api_key = "deepgram-SECRET-aaaa"
[groq]
api_key = "groq-SECRET-bbbb"
[openai]
api_key = "openai-SECRET-cccc"
[asr-sidecar]
api_key = "sidecar-SECRET-dddd"
[openai-compatible-realtime]
api_key = "realtime-SECRET-eeee"
[llm]
api_key = "llm-SECRET-ffff"
[tts]
api_key = "tts-SECRET-gggg"
"#,
        )
        .expect("fixture parses");

        let rendered = render_masked_toml(&config).expect("renders");

        assert!(
            !rendered.contains("SECRET"),
            "a key survived masking:\n{rendered}"
        );
        assert_eq!(
            rendered.matches("api_key").count(),
            7,
            "every section must still print a masked api_key:\n{rendered}"
        );
        for tail in ["aaaa", "bbbb", "cccc", "dddd", "eeee", "ffff", "gggg"] {
            assert!(
                rendered.contains(&format!("****{tail}")),
                "`{tail}` section is missing its masked key:\n{rendered}"
            );
        }
    }

    /// Parse `toml` and summarize its key with `env` standing in for the
    /// process environment, which these tests never touch.
    fn key_summary(toml: &str, env: &[(&str, &str)]) -> KeySummary {
        let config: Config = toml::from_str(toml).expect("fixture parses");
        key_summary_from(&config, |name| {
            env.iter()
                .find(|(var, _)| *var == name)
                .map(|(_, value)| value.to_string())
        })
    }

    /// One row per keyed backend family, including an alias of each shared
    /// section: backend, section name, and the env var the daemon reads.
    const KEYED_BACKENDS: &[(&str, &str, &str)] = &[
        ("groq", "groq", "WHISRS_GROQ_API_KEY"),
        ("deepgram", "deepgram", "WHISRS_DEEPGRAM_API_KEY"),
        ("deepgram-streaming", "deepgram", "WHISRS_DEEPGRAM_API_KEY"),
        ("openai", "openai", "WHISRS_OPENAI_API_KEY"),
        ("openai-realtime", "openai", "WHISRS_OPENAI_API_KEY"),
        ("asr-sidecar", "asr-sidecar", "WHISRS_ASR_SIDECAR_API_KEY"),
        ("asr", "asr-sidecar", "WHISRS_ASR_SIDECAR_API_KEY"),
        ("vibevoice", "asr-sidecar", "WHISRS_ASR_SIDECAR_API_KEY"),
    ];

    fn env_key(var: &'static str, masked: &str) -> KeySummary {
        KeySummary::Env {
            var,
            masked: masked.to_string(),
        }
    }

    fn file_key(masked: &str) -> KeySummary {
        KeySummary::File {
            masked: masked.to_string(),
        }
    }

    #[test]
    fn key_summary_shows_an_env_key_without_a_config_key() {
        for &(backend, section, var) in KEYED_BACKENDS {
            let absent = format!("[general]\nbackend = \"{backend}\"\n");
            let empty = format!("{absent}[{section}]\napi_key = \"\"\n");
            for toml in [absent, empty] {
                let summary = key_summary(&toml, &[(var, "env-key-9876")]);
                assert_eq!(summary, env_key(var, "****9876"), "{backend}");
            }
        }
    }

    #[test]
    fn key_summary_prefers_the_env_key_like_the_daemon() {
        for &(backend, section, var) in KEYED_BACKENDS {
            let toml = format!(
                "[general]\nbackend = \"{backend}\"\n[{section}]\napi_key = \"file-key-1234\"\n"
            );
            let both = key_summary(&toml, &[(var, "env-key-9876")]);
            assert_eq!(both, env_key(var, "****9876"), "{backend}");

            let file_only = key_summary(&toml, &[]);
            assert_eq!(file_only, file_key("****1234"), "{backend}");
        }
    }

    #[test]
    fn key_summary_without_any_key_is_unchanged() {
        for &(backend, section, _) in KEYED_BACKENDS {
            let summary = key_summary(&format!("[general]\nbackend = \"{backend}\"\n"), &[]);
            let expected = if section == "asr-sidecar" {
                KeySummary::Optional("API key")
            } else {
                KeySummary::NotSet
            };
            assert_eq!(summary, expected, "{backend}");
        }
    }

    /// The backends with no key to show, or an optional one.
    #[test]
    fn key_summary_of_keyless_and_optional_key_backends() {
        for backend in ["local-whisper", "local", "local-vosk", "local-parakeet"] {
            let toml = format!("[general]\nbackend = \"{backend}\"\n");
            assert_eq!(key_summary(&toml, &[]), KeySummary::NotNeeded, "{backend}");
        }
        let realtime = "[general]\nbackend = \"openai-compatible-realtime\"\n";
        assert_eq!(
            key_summary(realtime, &[]),
            KeySummary::Optional("bearer token")
        );
        let keyed = format!("{realtime}[openai-compatible-realtime]\napi_key = \"rt-key-5555\"\n");
        assert_eq!(key_summary(&keyed, &[]), file_key("****5555"));
        let unknown = "[general]\nbackend = \"nope\"\n";
        assert_eq!(key_summary(unknown, &[]), KeySummary::NotSet);
    }

    /// The daemon's blank-env rules differ by backend: groq only skips an
    /// empty string, so whitespace counts as set, while the sidecar trims.
    #[test]
    fn key_summary_follows_each_backends_blank_env_rule() {
        let groq = "[general]\nbackend = \"groq\"\n[groq]\napi_key = \"file-key-1234\"\n";
        let empty = key_summary(groq, &[("WHISRS_GROQ_API_KEY", "")]);
        assert_eq!(empty, file_key("****1234"));
        let spaces = key_summary(groq, &[("WHISRS_GROQ_API_KEY", "     ")]);
        assert!(matches!(
            spaces,
            KeySummary::Env {
                var: "WHISRS_GROQ_API_KEY",
                ..
            }
        ));

        let sidecar = "[general]\nbackend = \"asr-sidecar\"\n\
                       [asr-sidecar]\napi_key = \"file-key-1234\"\n";
        let spaces = key_summary(sidecar, &[("WHISRS_ASR_SIDECAR_API_KEY", "   ")]);
        assert_eq!(spaces, file_key("****1234"));
        let padded = key_summary(
            sidecar,
            &[("WHISRS_ASR_SIDECAR_API_KEY", " env-key-9876\n")],
        );
        assert_eq!(padded, env_key("WHISRS_ASR_SIDECAR_API_KEY", "****9876"));
    }
}
