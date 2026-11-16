//! Renders `docs/config-reference.md` from the Config screen's rows
//! ([`super::schema`]) and the defaults serde gives a config that sets
//! nothing, and fails when the file differs. `WHISRS_UPDATE_DOCS=1`
//! rewrites it, as `WHISRS_UPDATE_SNAPSHOTS=1` does for the snapshots.
//!
//! Every row is rendered whatever this build's features (a gated key says
//! which feature it needs), so the default build and the one without
//! `recording` render the same file.

use std::collections::BTreeSet;
use std::path::Path;

use whisrs::config::catalog;
use whisrs::Config;

use super::schema::{Body, Check, Entries, Field, Gate, Kind, Source, EDITOR_ONLY, SECTIONS};

/// What the test says when the file differs.
const STALE: &str = "docs/config-reference.md is stale: run WHISRS_UPDATE_DOCS=1 cargo test";

/// Entry keys serde lets out but [`whisrs::agent::resolve_agents`] skips an
/// entry without, each with a value it takes.
const VALIDATED: &[(&str, &str)] = &[("agents.name", "x"), ("agents.dir", "/")];

/// The guide for a section, linked under its heading.
const GUIDES: &[(&str, &str, &str)] = &[
    (
        "[general]",
        "Batch and streaming backends",
        "configuration.md#batch-and-streaming-backends",
    ),
    (
        "[input]",
        "Terminal detection",
        "configuration.md#terminal-detection",
    ),
    (
        "[audio]",
        "Speech detection",
        "configuration.md#speech-detection",
    ),
    ("[hotkeys]", "Hotkeys", "configuration.md#hotkeys"),
    (
        "[openai-compatible-realtime]",
        "Realtime servers",
        "configuration.md#realtime-servers",
    ),
    (
        "[asr-sidecar]",
        "ASR sidecar servers",
        "configuration.md#asr-sidecar-servers",
    ),
    ("[overlay]", "Overlay", "configuration.md#overlay"),
    ("[overlay.colors]", "Overlay", "configuration.md#overlay"),
    (
        "[agent_overlay]",
        "The agent overlay",
        "agents.md#the-agent-overlay",
    ),
    ("[tts]", "Read aloud", "configuration.md#read-aloud"),
    ("[llm]", "LLM commands", "configuration.md#llm-commands"),
    (
        "[[llm_commands]]",
        "LLM commands",
        "configuration.md#llm-commands",
    ),
    ("[[agents]]", "Agent mode", "agents.md"),
    (
        "[replacements]",
        "Word replacements",
        "configuration.md#word-replacements",
    ),
    ("[recording]", "Recording", "recording.md"),
    ("[hooks]", "Hooks", "configuration.md#hooks"),
];

/// One config table of the page: its rows, each with the feature it needs.
struct Table {
    heading: String,
    rows: Vec<([String; 5], Option<Gate>)>,
}

/// `s` as code.
fn code(s: impl std::fmt::Display) -> String {
    format!("`{s}`")
}

/// Help text as a table cell: `<` would open an HTML tag.
fn text(s: &str) -> String {
    s.replace('<', "&lt;")
}

/// A typed check that takes URLs and refuses other text.
fn takes_urls(check: Check) -> bool {
    check("https://example.com").is_ok() && check("example.com").is_err()
}

/// `choices` as code, the backends not implemented yet marked.
fn choices(choices: &[catalog::Entry]) -> String {
    choices
        .iter()
        .map(|(value, _)| {
            if catalog::STUB_BACKENDS.contains(value) {
                format!("{} (coming soon)", code(value))
            } else {
                code(value)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn source(source: Source, check: Option<Check>) -> String {
    match source {
        Source::List(list) => {
            let other = if check.is_some_and(takes_urls) {
                "another URL"
            } else {
                "any other"
            };
            format!("{}, or {other}", choices(list))
        }
        Source::Languages { auto: true } => {
            "`auto`, or a language code (`en`, `pt-BR`)".to_string()
        }
        Source::Languages { auto: false } => "language codes (`en`, `pt-BR`)".to_string(),
        Source::TtsModels => "the backend's models (whisrs config lists them)".to_string(),
        Source::TtsVoices => "the model's voices (whisrs config lists them)".to_string(),
        Source::AsrSidecarModels => {
            "OpenRouter's models (whisrs config lists them), or your server's model name"
                .to_string()
        }
        Source::LlmModels => "the provider's chat models".to_string(),
        Source::Inputs => "`default`, or a source from `pactl list short sources`".to_string(),
        Source::Monitors => {
            "`default`, `none` (mic only), or a source name ending in `.monitor`".to_string()
        }
        Source::ModelFiles => "a `ggml-*.bin` file".to_string(),
    }
}

fn kind_type(kind: &Kind) -> String {
    match kind {
        Kind::Bool | Kind::MaybeBool { .. } => "bool".into(),
        Kind::Choice(_) | Kind::MaybeChoice { .. } | Kind::Text { .. } | Kind::Suggest { .. } => {
            "string".into()
        }
        Kind::Int(steps) => match steps.unit {
            "" => "integer".into(),
            "min" => "integer (minutes)".into(),
            unit => format!("integer ({unit})"),
        },
        Kind::Float { .. } => "number".into(),
        Kind::Path { .. } => "path".into(),
        Kind::Color => "colour".into(),
        Kind::Hotkey { .. } => "hotkey".into(),
        Kind::Secret => "API key".into(),
        Kind::List { .. } => "list".into(),
    }
}

fn kind_values(kind: &Kind) -> String {
    match kind {
        Kind::Bool | Kind::MaybeBool { .. } => "`true`, `false`".into(),
        Kind::Choice(list) | Kind::MaybeChoice { choices: list, .. } => choices(list),
        Kind::Int(steps) => {
            let mut values = format!("{} to {}", steps.min, steps.max);
            for (n, name) in steps.named {
                values.push_str(&format!("; {} = {name}", code(n)));
            }
            values
        }
        Kind::Float { min, max, .. } => format!("{min} to {max}"),
        Kind::Text { check, .. } => match check {
            Some(check) if takes_urls(*check) => "a URL".into(),
            _ => "text".into(),
        },
        Kind::Suggest { from, check, .. } => source(*from, *check),
        Kind::Path {
            from: Some(from), ..
        } => source(*from, None),
        Kind::Path { tilde: true, .. } => "a path (`~` is your home)".into(),
        Kind::Path { tilde: false, .. } => "an absolute path".into(),
        Kind::Color => "`#RGB`, `#RRGGBB` or `#RRGGBBAA`".into(),
        Kind::Hotkey { .. } => "a key combo ([syntax](configuration.md#hotkeys))".into(),
        Kind::Secret => "your key".into(),
        Kind::List {
            items: Some((from, _)),
            ..
        } => source(*from, None),
        Kind::List { items: None, .. } => "strings".into(),
    }
}

/// What applies while the key is left out.
fn unset(kind: &Kind) -> String {
    let label = match kind {
        Kind::MaybeBool { unset } | Kind::MaybeChoice { unset, .. } => Some(*unset),
        Kind::Text { unset, .. }
        | Kind::Suggest { unset, .. }
        | Kind::Path { unset, .. }
        | Kind::Hotkey { unset } => *unset,
        Kind::Color => Some("the theme's"),
        _ => None,
    };
    match label {
        Some(label) if label != "not set" => format!("unset ({label})"),
        _ => "unset".into(),
    }
}

/// A default string as a cell. One under the data folder is described, not
/// shown as a path: the page is then the same on every machine, and it is
/// not a literal to paste into a key that does not expand `~`.
fn string_cell(value: &str) -> String {
    let data = dirs::data_dir();
    let rest = data
        .iter()
        .map(|data| data.as_path())
        .chain([Path::new("~/.local/share")])
        .find_map(|data| Path::new(value).strip_prefix(data).ok());
    match rest {
        Some(rest) => format!(
            "unset (uses {} in the data folder: `$XDG_DATA_HOME`, `~/.local/share` by default)",
            rest.display()
        ),
        None => code(value),
    }
}

fn default_cell(kind: &Kind, value: Option<&toml::Value>, required: bool) -> String {
    if required {
        return "required".into();
    }
    if matches!(kind, Kind::Secret) {
        return "unset".into();
    }
    match value {
        None => match kind {
            Kind::List { empty, .. } => (*empty).to_string(),
            _ => unset(kind),
        },
        Some(toml::Value::String(s)) if s.trim().is_empty() => unset(kind),
        Some(toml::Value::String(s)) => string_cell(s),
        Some(toml::Value::Integer(n)) => {
            let named = match kind {
                Kind::Int(steps) => steps.named.iter().find(|(v, _)| v == n),
                _ => None,
            };
            match named {
                Some((_, name)) => format!("{} ({name})", code(n)),
                None => code(n),
            }
        }
        Some(toml::Value::Array(items)) if items.is_empty() => match kind {
            Kind::List { empty, .. } => (*empty).to_string(),
            _ => "none".into(),
        },
        Some(toml::Value::Array(items)) => items
            .iter()
            .map(|item| {
                code(
                    item.as_str()
                        .map_or_else(|| item.to_string(), str::to_string),
                )
            })
            .collect::<Vec<_>>()
            .join(", "),
        Some(other) => code(other),
    }
}

fn row(key: &str, field: &Field, value: Option<&toml::Value>, required: bool) -> [String; 5] {
    [
        code(key),
        kind_type(&field.kind),
        default_cell(&field.kind, value, required),
        kind_values(&field.kind),
        text(field.help),
    ]
}

/// The value at the dotted `path` of `table`.
fn lookup<'a>(table: &'a toml::Table, path: &str) -> Option<&'a toml::Value> {
    let mut segments = path.split('.');
    let mut value = table.get(segments.next()?)?;
    for segment in segments {
        value = value.get(segment)?;
    }
    Some(value)
}

/// A config with every table present and nothing set, serialized: each
/// key's default where it has one.
fn defaults() -> toml::Table {
    let mut tables = BTreeSet::new();
    for section in SECTIONS {
        if let Body::Rows(fields) = section.body {
            for field in fields {
                tables.insert(field.path.rsplit_once('.').expect("a dotted path").0);
            }
        }
    }
    for path in EDITOR_ONLY {
        tables.insert(path.rsplit_once('.').expect("a dotted path").0);
    }
    let source: String = tables.iter().map(|t| format!("[{t}]\n")).collect();
    let config: Config = toml::from_str(&source).expect("every table parses empty");
    toml::Table::try_from(config).expect("Config serializes")
}

/// One entry of `entries` with only its required keys set, serialized.
fn entry_defaults(entries: &Entries) -> toml::Table {
    let mut source = format!("[[{}]]\n", entries.path);
    for key in entries.required {
        source.push_str(&format!("{key} = \"x\"\n"));
    }
    let config: Config = toml::from_str(&source).expect("an entry with its required keys");
    let table = toml::Table::try_from(config).expect("Config serializes");
    table
        .get(entries.path)
        .and_then(toml::Value::as_array)
        .and_then(|entries| entries.first())
        .and_then(toml::Value::as_table)
        .expect("the entry")
        .clone()
}

/// Add a row to the table `heading`, which is made on its first row.
fn push(tables: &mut Vec<Table>, heading: String, cells: [String; 5], gate: Option<Gate>) {
    match tables.iter_mut().find(|t| t.heading == heading) {
        Some(table) => table.rows.push((cells, gate)),
        None => tables.push(Table {
            heading,
            rows: vec![(cells, gate)],
        }),
    }
}

fn needs(gate: Gate) -> String {
    format!("Needs the `{}` build feature.", gate.name())
}

/// The whole page.
fn render() -> String {
    let defaults = defaults();
    let mut tables: Vec<Table> = Vec::new();
    for section in SECTIONS {
        match section.body {
            Body::Rows(fields) => {
                for field in fields {
                    let (table, key) = field.path.rsplit_once('.').expect("a dotted path");
                    let cells = row(key, field, lookup(&defaults, field.path), false);
                    push(
                        &mut tables,
                        format!("[{table}]"),
                        cells,
                        field.gate.or(section.gate),
                    );
                }
            }
            Body::Entries(entries) => {
                let entry = entry_defaults(entries);
                for field in entries.fields {
                    let path = format!("{}.{}", entries.path, field.path);
                    let required = entries.required.contains(&field.path)
                        || VALIDATED.iter().any(|(p, _)| *p == path);
                    let cells = row(field.path, field, entry.get(field.path), required);
                    push(
                        &mut tables,
                        format!("[[{}]]", entries.path),
                        cells,
                        field.gate.or(section.gate),
                    );
                }
            }
            Body::Map(map) => push(
                &mut tables,
                format!("[{}]", map.path),
                [
                    code("\"phrase\""),
                    "string".into(),
                    "none".into(),
                    "the text it becomes".into(),
                    text(map.help),
                ],
                section.gate,
            ),
        }
    }
    for path in EDITOR_ONLY {
        let (table, key) = path.rsplit_once('.').expect("a dotted path");
        push(
            &mut tables,
            format!("[{table}]"),
            [
                code(key),
                "path".into(),
                "unset".into(),
                "a path".into(),
                "The backend this configures is not implemented yet.".into(),
            ],
            None,
        );
    }

    let mut page = String::from(
        "<!-- Generated from src/cli/tui/schema.rs by src/cli/tui/reference.rs: edit those, \
         then run WHISRS_UPDATE_DOCS=1 cargo test. -->\n\n\
         # Config reference\n\n\
         Every key of `~/.config/whisrs/config.toml`, with its default. `whisrs config` edits \
         them all; [configuration.md](configuration.md) explains how the features fit \
         together.\n\n\
         A key left out takes its default; \"unset\" says what applies then. A key that needs \
         a build feature does nothing in a build without it: the default build has every \
         feature, and the minimal release build has all but `local-whisper`.\n",
    );
    for table in &tables {
        page.push_str(&format!("\n## {}\n\n", table.heading));
        let first = table.rows[0].1;
        let shared = first.filter(|_| table.rows.iter().all(|(_, gate)| *gate == first));
        let mut intro = Vec::new();
        if let Some(gate) = shared {
            intro.push(needs(gate));
        }
        if let Some((_, title, link)) = GUIDES.iter().find(|(h, ..)| *h == table.heading) {
            intro.push(format!("Guide: [{title}]({link})."));
        }
        if !intro.is_empty() {
            page.push_str(&intro.join(" "));
            page.push_str("\n\n");
        }
        page.push_str("| Key | Type | Default | Values | Description |\n|---|---|---|---|---|\n");
        for (cells, gate) in &table.rows {
            let mut cells = cells.clone();
            if let (Some(gate), None) = (gate, shared) {
                cells[4] = format!("{} {}", cells[4], needs(*gate));
            }
            let cells: Vec<String> = cells.iter().map(|c| c.replace('|', "\\|")).collect();
            page.push_str(&format!("| {} |\n", cells.join(" | ")));
        }
    }
    page
}

/// The GitHub anchor of a heading: lower case, punctuation but `-` and `_`
/// dropped, spaces as `-`.
fn anchor(heading: &str) -> String {
    heading
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

fn docs() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("docs")
}

#[test]
fn config_reference_is_current() {
    let path = docs().join("config-reference.md");
    let rendered = render();
    if std::env::var_os("WHISRS_UPDATE_DOCS").is_some() {
        std::fs::write(&path, &rendered).expect("write docs/config-reference.md");
        return;
    }
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if current != rendered {
        let line = current
            .lines()
            .zip(rendered.lines())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| current.lines().count().min(rendered.lines().count()));
        eprintln!("first difference at line {}", line + 1);
        panic!("{STALE}");
    }
}

/// The keys the page calls required are: an entry without one is skipped.
#[test]
fn validated_keys_are_required() {
    let entry = |except: &str| {
        let source: String = VALIDATED
            .iter()
            .filter(|(path, _)| *path != except)
            .map(|(path, value)| {
                let key = path.strip_prefix("agents.").expect("an [[agents]] key");
                format!("{key} = \"{value}\"\n")
            })
            .collect();
        toml::from_str::<whisrs::agent::AgentConfig>(&source).expect("an [[agents]] entry")
    };
    assert_eq!(whisrs::agent::resolve_agents(&[entry("")]).0.len(), 1);
    for (path, _) in VALIDATED {
        let (agents, _) = whisrs::agent::resolve_agents(&[entry(path)]);
        assert!(agents.is_empty(), "an entry without {path} was kept");
    }
}

/// Every section the page links a guide for exists, and so does the
/// heading each link names.
#[test]
fn guide_links_resolve() {
    let headings: BTreeSet<String> = render()
        .lines()
        .filter_map(|line| line.strip_prefix("## "))
        .map(str::to_string)
        .collect();
    for (heading, _, link) in GUIDES {
        assert!(headings.contains(*heading), "no section {heading}");
        let (file, fragment) = link.split_once('#').unwrap_or((link, ""));
        let page =
            std::fs::read_to_string(docs().join(file)).unwrap_or_else(|e| panic!("{link}: {e}"));
        if fragment.is_empty() {
            continue;
        }
        let found = page
            .lines()
            .filter(|line| line.starts_with('#'))
            .filter_map(|line| line.trim_start_matches('#').strip_prefix(' '))
            .any(|h| anchor(h) == fragment);
        assert!(found, "{link}: no such heading");
    }
}

#[test]
fn anchors_follow_github() {
    assert_eq!(
        anchor("What happens to the LLM's reply"),
        "what-happens-to-the-llms-reply"
    );
    assert_eq!(anchor("[[agents]]"), "agents");
    assert_eq!(anchor("[overlay.colors]"), "overlaycolors");
    assert_eq!(anchor("[agent_overlay]"), "agent_overlay");
}
