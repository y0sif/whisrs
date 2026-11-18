//! Naming the speakers of a recording.
//!
//! [`super::speakers::assign`] tells the voices apart and numbers them; this
//! module names them ([`name_speakers`]). The steps run in order, and the
//! first that names a speaker wins: a later step never overrides an earlier
//! one.
//!
//! 1. **User.** A name given with `whisrs recording name`, kept in
//!    `recording.json` `speaker_names` apart from the computed list, so a
//!    retry keeps it (`user`). A voice's name follows the voice, by where
//!    it first spoke, when a retry numbers the voices differently
//!    ([`rebind_user_names`]).
//! 2. **Self.** The mic, when it is one voice (`me`), is the user
//!    (`[recording] user_name`, `self`).
//! 3. **Participants.** With exactly one remote voice and exactly one
//!    participant (`--with`), the voice is that participant (`with`).
//! 4. **The LLM** (opt-in, [`super::llm_names`]). A name it gives a voice
//!    still unnamed, with a quote from the transcript that shows it, is kept
//!    when it is one of the participants (when there are any), is not the
//!    user's, and is given to no other speaker (`llm`, with `evidence`).
//! 5. **The last remaining voice.** When one remote voice is left unnamed,
//!    one participant is left unassigned, there are as many remote voices as
//!    participants and the mic is not shared, that voice is that participant
//!    (`last`).
//! 6. Everyone else is `Speaker N` (`none`), numbered by first appearance
//!    among the unnamed, except a single remote voice, which keeps the
//!    others label (`channel`).
//!
//! Nothing is guessed: a voice is named only when the steps say who it is. A
//! remote voice is one on the system track (`S1`, `S2`, ...); a minor one
//! ([`super::speakers::is_minor`]) is never named after a participant and
//! does not count as a voice.

use std::collections::BTreeMap;
use std::path::Path;

use tracing::info;

use super::speakers::{ME, MIC, MIC_LABEL, OTHERS, SOURCE_CHANNEL, SOURCE_NONE, SOURCE_SELF};
use super::{
    store, RecordingMeta, RecordingState, Speaker, SpeakerName, TrackKind, TranscriptSegment,
    VoiceAnchor,
};
use crate::{Config, RecordingConfig};

/// Most people `--with` may name.
pub const PARTICIPANTS_MAX: usize = 20;

/// Longest name of a person (`--with`, `whisrs recording name`), in
/// characters.
pub const PERSON_NAME_MAX: usize = 64;

/// [`Speaker::source`] of a name the user gave (`whisrs recording name`).
pub const SOURCE_USER: &str = "user";

/// [`Speaker::source`] of the one remote voice named after the one
/// participant.
pub const SOURCE_WITH: &str = "with";

/// [`Speaker::source`] of the last remote voice named after the last
/// participant.
pub const SOURCE_LAST: &str = "last";

/// [`Speaker::source`] of a name the LLM found, with its evidence.
pub const SOURCE_LLM: &str = "llm";

/// A name the LLM gave a speaker, with the words of the transcript that
/// show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub name: String,
    pub evidence: String,
}

/// The LLM's names, by speaker id.
pub type Claims = BTreeMap<String, Claim>;

/// Validate and trim the name of a person: non-empty, one line, at most
/// [`PERSON_NAME_MAX`] characters.
pub fn validate_person_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("a name is empty".to_string());
    }
    if trimmed.chars().any(char::is_control) {
        return Err(format!(
            "the name {trimmed:?} must be a single line of text"
        ));
    }
    let len = trimmed.chars().count();
    if len > PERSON_NAME_MAX {
        return Err(format!(
            "the name {trimmed:?} is {len} characters long: use at most {PERSON_NAME_MAX}"
        ));
    }
    Ok(trimmed.to_string())
}

/// Parse `--with "Sarah, Omar"`: names separated by commas, each checked by
/// [`validate_participants`]. A blank list names nobody, which lets a retry
/// clear the participants.
pub fn parse_participants(list: &str) -> Result<Vec<String>, String> {
    if list.trim().is_empty() {
        return Ok(Vec::new());
    }
    validate_participants(list.split(',').map(str::to_string).collect())
}

/// Check and trim the participants of a recording: at most
/// [`PARTICIPANTS_MAX`], each a valid name ([`validate_person_name`])
/// without a comma (the separator of `--with`), none listed twice.
pub fn validate_participants(names: Vec<String>) -> Result<Vec<String>, String> {
    if names.len() > PARTICIPANTS_MAX {
        return Err(format!(
            "{} participants: name at most {PARTICIPANTS_MAX}",
            names.len()
        ));
    }
    let mut out: Vec<String> = Vec::with_capacity(names.len());
    for name in names {
        let name = validate_person_name(&name).map_err(|e| {
            format!("participants: {e}; separate names with commas, e.g. \"Sarah, Omar\"")
        })?;
        if name.contains(',') {
            return Err(format!(
                "participants: {name:?} holds a comma, which separates names"
            ));
        }
        if out.iter().any(|n| normalize(n) == normalize(&name)) {
            return Err(format!("participants: {name:?} is listed twice"));
        }
        out.push(name);
    }
    Ok(out)
}

/// Lowercased, with each run of whitespace one space and none at the ends.
pub(crate) fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The first word of a normalized name.
fn first_word(normalized: &str) -> &str {
    normalized.split(' ').next().unwrap_or("")
}

/// Whether `name` names participant `person`: their full name, their first
/// name, or a short form of 3 or more letters that starts their first name
/// ("Sam" for "Samantha Reed"). Case and spacing do not count.
pub fn names_participant(name: &str, person: &str) -> bool {
    let name = normalize(name);
    let person = normalize(person);
    if name.is_empty() {
        return false;
    }
    let first = first_word(&person);
    name == person || name == first || (name.chars().count() >= 3 && first.starts_with(&name))
}

/// Whether `name` is the user's: their name, its first word, a short form
/// of that ([`names_participant`]), or a name with the same first word.
fn is_user(name: &str, user_name: &str) -> bool {
    names_participant(name, user_name)
        || first_word(&normalize(name)) == first_word(&normalize(user_name))
}

/// What names one recording's speakers, besides its speakers and segments.
#[derive(Debug, Clone, Copy)]
pub struct Naming<'a> {
    /// The user's name, for the mic when it is the user.
    pub user_name: &'a str,
    /// The label of the system track ([`RecordingMeta::others_label`]).
    pub others_label: &'a str,
    /// Who the recording is with (`--with`).
    pub participants: &'a [String],
    /// Several people share the mic.
    pub shared_mic: bool,
    /// Names the user gave, by speaker id (`whisrs recording name`).
    pub user_names: &'a BTreeMap<String, SpeakerName>,
}

impl<'a> Naming<'a> {
    /// What names `meta`'s speakers, for a user called `user_name`.
    pub fn of(meta: &'a RecordingMeta, user_name: &'a str) -> Self {
        Naming {
            user_name,
            others_label: meta.others_label(),
            participants: &meta.participants,
            shared_mic: meta.shared_mic.unwrap_or(false),
            user_names: &meta.speaker_names,
        }
    }

    /// The participants other than the user, who is on the mic: a listed
    /// name that names the user ([`names_participant`]: their full name,
    /// first name or a short form of it) is the user.
    fn listed_people(&self) -> Vec<&'a str> {
        self.participants
            .iter()
            .map(String::as_str)
            .filter(|p| !names_participant(p, self.user_name))
            .collect()
    }
}

/// A speaker's name and how it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Named {
    name: String,
    source: String,
    evidence: Option<String>,
}

impl Named {
    fn new(name: &str, source: &str) -> Self {
        Named {
            name: name.to_string(),
            source: source.to_string(),
            evidence: None,
        }
    }
}

/// The track of voice id `id`: `S<n>` on the system track, `M<n>` on a
/// shared mic. `None` for a track's own speaker (`me`, `others`, `mic`).
pub fn voice_track(id: &str) -> Option<TrackKind> {
    let (prefix, number) = id.split_at_checked(1)?;
    let track = match prefix {
        "S" => TrackKind::System,
        "M" => TrackKind::Mic,
        _ => return None,
    };
    (!number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())).then_some(track)
}

/// A voice on the system track that is not minor: one the participants
/// can be.
fn is_remote_voice(speaker: &Speaker) -> bool {
    voice_track(&speaker.id) == Some(TrackKind::System) && !speaker.minor
}

/// What each track says of a speaker before anyone names it: the mic's one
/// voice is the user, a track's own speaker is its label, a voice is
/// unnamed. A speaker of a kind this does not know keeps what it has.
fn base(speaker: &Speaker, naming: &Naming<'_>) -> Option<Named> {
    match speaker.id.as_str() {
        ME => Some(Named::new(naming.user_name, SOURCE_SELF)),
        OTHERS => Some(Named::new(naming.others_label, SOURCE_CHANNEL)),
        MIC => Some(Named::new(MIC_LABEL, SOURCE_CHANNEL)),
        id if voice_track(id).is_some() => None,
        _ => Some(Named {
            name: speaker.name.clone(),
            source: speaker.source.clone(),
            evidence: speaker.evidence.clone(),
        }),
    }
}

/// Whether a speaker already goes by participant `person`'s name, or a
/// name that `person` names in turn (the user calling "Omar" "Omar
/// Haddad"). Only the names given to people count: a track's label or the
/// user's own name never assigns a participant.
fn assigned(person: &str, named: &[Option<Named>]) -> bool {
    named.iter().flatten().any(|n| {
        matches!(
            n.source.as_str(),
            SOURCE_USER | SOURCE_WITH | SOURCE_LLM | SOURCE_LAST
        ) && (names_participant(&n.name, person) || names_participant(person, &n.name))
    })
}

/// When each speaker first speaks in `segments`; never for one without a
/// segment.
fn first_appearance(speakers: &[Speaker], segments: &[TranscriptSegment]) -> Vec<u64> {
    speakers
        .iter()
        .map(|s| {
            segments
                .iter()
                .filter(|seg| seg.speaker == s.id)
                .map(|seg| seg.start_ms)
                .min()
                .unwrap_or(u64::MAX)
        })
        .collect()
}

/// The steps of the module docs before the LLM, user to participants:
/// each speaker's name and source, `None` when no step names it.
fn before_llm(speakers: &[Speaker], naming: &Naming<'_>) -> Vec<Option<Named>> {
    let mut named: Vec<Option<Named>> = speakers.iter().map(|s| base(s, naming)).collect();

    // 1. The user, over everything else.
    for (i, speaker) in speakers.iter().enumerate() {
        if let Some(given) = naming.user_names.get(&speaker.id) {
            named[i] = Some(Named::new(&given.name, SOURCE_USER));
        }
    }

    let remote: Vec<usize> = (0..speakers.len())
        .filter(|&i| is_remote_voice(&speakers[i]))
        .collect();
    let participants = naming.listed_people();

    // 3. One remote voice, one participant. Not with others on a shared
    // mic: the one participant may be in the room.
    let others_on_mic = speakers
        .iter()
        .any(|s| voice_track(&s.id) == Some(TrackKind::Mic) && !s.minor);
    if let ([voice], [person]) = (remote.as_slice(), participants.as_slice()) {
        if !others_on_mic && named[*voice].is_none() && !assigned(person, &named) {
            named[*voice] = Some(Named::new(person, SOURCE_WITH));
        }
    }
    named
}

/// Step 4: the LLM's names for voices still unnamed in `named`. A claim is
/// kept when it names a voice that is not minor, a participant when there
/// are any (the participant's name is used, as listed), not the user, and
/// nobody else: a name another speaker has, or that two claims give, is
/// refused.
fn accept_claims(
    speakers: &[Speaker],
    named: &mut [Option<Named>],
    naming: &Naming<'_>,
    claims: &Claims,
) {
    let people = naming.listed_people();
    let mut picks: Vec<(usize, Named)> = Vec::new();
    for (i, speaker) in speakers.iter().enumerate() {
        let Some(claim) = claims.get(&speaker.id) else {
            continue;
        };
        let refuse = |why: &str| {
            info!(
                "the LLM's name {:?} for {} is refused: {why}",
                claim.name, speaker.id
            );
        };
        if named[i].is_some() || voice_track(&speaker.id).is_none() || speaker.minor {
            refuse("not a voice left to name");
            continue;
        }
        let name = if people.is_empty() {
            match validate_person_name(&claim.name) {
                Ok(name) => name,
                Err(why) => {
                    refuse(&why);
                    continue;
                }
            }
        } else {
            let matching: Vec<&str> = people
                .iter()
                .copied()
                .filter(|p| names_participant(&claim.name, p))
                .collect();
            match matching.as_slice() {
                [person] => person.to_string(),
                [] => {
                    refuse("not a participant");
                    continue;
                }
                _ => {
                    refuse("it names more than one participant");
                    continue;
                }
            }
        };
        if is_user(&name, naming.user_name) {
            refuse("the user's name");
            continue;
        }
        let taken = named
            .iter()
            .flatten()
            .any(|n| normalize(&n.name) == normalize(&name))
            || (!people.is_empty() && assigned(&name, named));
        if taken {
            refuse("another speaker has it");
            continue;
        }
        picks.push((
            i,
            Named {
                name,
                source: SOURCE_LLM.to_string(),
                evidence: Some(claim.evidence.clone()),
            },
        ));
    }
    for (i, pick) in &picks {
        let twice = picks
            .iter()
            .filter(|(_, other)| normalize(&other.name) == normalize(&pick.name))
            .count()
            > 1;
        if twice {
            info!(
                "the LLM's name {:?} for {} is refused: it gave it to another speaker too",
                pick.name, speakers[*i].id
            );
            continue;
        }
        named[*i] = Some(pick.clone());
    }
}

/// Every step of the module docs but the last: each speaker's name and
/// source, `None` when no step names it.
fn named_by_steps(
    speakers: &[Speaker],
    naming: &Naming<'_>,
    claims: &Claims,
) -> Vec<Option<Named>> {
    let mut named = before_llm(speakers, naming);
    accept_claims(speakers, &mut named, naming, claims);
    let remote: Vec<usize> = (0..speakers.len())
        .filter(|&i| is_remote_voice(&speakers[i]))
        .collect();
    let participants = naming.listed_people();

    // 5. The last remaining voice is the last remaining participant, when
    // the voices and the participants are as many and nobody else is in the
    // room.
    if !naming.shared_mic && remote.len() == participants.len() {
        let unnamed: Vec<usize> = remote
            .iter()
            .copied()
            .filter(|&i| named[i].is_none())
            .collect();
        let left: Vec<&str> = participants
            .iter()
            .copied()
            .filter(|p| !assigned(p, &named))
            .collect();
        if let ([voice], [person]) = (unnamed.as_slice(), left.as_slice()) {
            named[*voice] = Some(Named::new(person, SOURCE_LAST));
        }
    }
    named
}

/// Every speaker's name and source ([`named_by_steps`]), the unnamed ones
/// as `Speaker N` by first appearance in `segments`, or a single remote
/// voice by the others label.
fn decide(
    speakers: &[Speaker],
    segments: &[TranscriptSegment],
    naming: &Naming<'_>,
    claims: &Claims,
) -> Vec<Named> {
    let mut named = named_by_steps(speakers, naming, claims);
    let mut remote = (0..speakers.len()).filter(|&i| is_remote_voice(&speakers[i]));
    let single_remote = match (remote.next(), remote.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    };
    let first = first_appearance(speakers, segments);
    let mut unnamed: Vec<usize> = (0..speakers.len())
        .filter(|&i| named[i].is_none())
        .collect();
    unnamed.sort_by_key(|&i| (first[i], i));
    let mut n = 0;
    for i in unnamed {
        named[i] = Some(if Some(i) == single_remote {
            Named::new(naming.others_label, SOURCE_CHANNEL)
        } else {
            n += 1;
            Named::new(&format!("Speaker {n}"), SOURCE_NONE)
        });
    }
    named
        .into_iter()
        .map(|n| n.expect("every speaker is named"))
        .collect()
}

/// How far from where a named voice first spoke its voice is found again
/// ([`rebind_user_names`]).
pub const ANCHOR_TOLERANCE_MS: u64 = 1000;

/// Start of the note `recording.json` keeps in `errors` when a name the
/// user gave matches no voice any more ([`rebind_user_names`]).
pub const USER_NAME_LOST: &str = "a speaker name you gave was dropped: ";

/// Where speaker `id` is first heard in `segments`, and on which track.
fn anchor_of(segments: &[TranscriptSegment], id: &str) -> Option<VoiceAnchor> {
    segments
        .iter()
        .filter(|s| s.speaker == id)
        .min_by_key(|s| s.start_ms)
        .map(|s| VoiceAnchor {
            channel: s.channel,
            first_ms: s.start_ms,
        })
}

/// Give each name the user gave a voice to the voice that first speaks on
/// its track within [`ANCHOR_TOLERANCE_MS`] of where the named one did,
/// whatever its id is now: a retry sends the audio again, and the voices
/// can come back numbered differently. Names and voices are matched as a
/// whole, nearest pairs first, each name and each voice at most once, so
/// one name taking another's voice cannot push that name onto a stranger.
/// A name whose track has voices this time but none for it is dropped, and
/// the returned note says so. A name whose track has no voices told apart
/// this time (the separation failed, or found one voice) waits, as it is,
/// for an attempt that has them. A name without an anchor (a track's own
/// speaker, or a name stored before anchors) stays on its id.
pub fn rebind_user_names(meta: &mut RecordingMeta) -> Vec<String> {
    let old = std::mem::take(&mut meta.speaker_names);
    let (plain, anchored): (Vec<_>, Vec<_>) = old
        .into_iter()
        .partition(|(_, given)| given.anchor.is_none());
    let mut names: BTreeMap<String, SpeakerName> = plain.into_iter().collect();
    // This attempt's voices, where each first speaks; one a plain name
    // holds is taken.
    let voices: Vec<(&str, VoiceAnchor)> = meta
        .speakers
        .iter()
        .filter(|s| voice_track(&s.id).is_some() && !names.contains_key(&s.id))
        .filter_map(|s| Some((s.id.as_str(), anchor_of(&meta.segments, &s.id)?)))
        .collect();
    let anchors: Vec<VoiceAnchor> = anchored
        .iter()
        .map(|(_, given)| given.anchor.expect("partitioned on the anchor"))
        .collect();
    let mut pairs: Vec<(u64, usize, usize)> = Vec::new();
    for (n, anchor) in anchors.iter().enumerate() {
        for (v, (_, at)) in voices.iter().enumerate() {
            let off = at.first_ms.abs_diff(anchor.first_ms);
            if at.channel == anchor.channel && off <= ANCHOR_TOLERANCE_MS {
                pairs.push((off, n, v));
            }
        }
    }
    pairs.sort_unstable();
    let mut voice_of: Vec<Option<usize>> = vec![None; anchored.len()];
    let mut taken = vec![false; voices.len()];
    for (_, n, v) in pairs {
        if voice_of[n].is_none() && !taken[v] {
            voice_of[n] = Some(v);
            taken[v] = true;
        }
    }
    let told_apart = |channel: TrackKind| {
        meta.speakers
            .iter()
            .any(|s| voice_track(&s.id) == Some(channel))
    };
    let mut notes = Vec::new();
    for (((id, given), anchor), voice) in anchored.into_iter().zip(anchors).zip(voice_of) {
        match voice {
            Some(v) => {
                names.insert(voices[v].0.to_string(), given);
            }
            None if !told_apart(anchor.channel) => {
                names.entry(id).or_insert(given);
            }
            None => notes.push(format!(
                "{USER_NAME_LOST}{:?} (was {id}) matches no voice after the speakers were \
                 told apart again; name them again with whisrs recording name",
                given.name
            )),
        }
    }
    meta.speaker_names = names;
    notes
}

/// Name `meta`'s speakers, from scratch: every step of the module docs, for
/// a user called `user_name`, with the LLM's `claims` (already checked
/// against the transcript, [`super::llm_names::verify`]). The user's names
/// follow their voices first ([`rebind_user_names`]; the notes of names
/// that found none go to `errors`). Their ids, talk times and order stay.
pub fn name_speakers(meta: &mut RecordingMeta, user_name: &str, claims: &Claims) {
    let lost = rebind_user_names(meta);
    meta.errors.extend(lost);
    let named = decide(
        &meta.speakers,
        &meta.segments,
        &Naming::of(meta, user_name),
        claims,
    );
    for (speaker, named) in meta.speakers.iter_mut().zip(named) {
        speaker.name = named.name;
        speaker.source = named.source;
        speaker.evidence = named.evidence;
    }
}

/// What the LLM is asked about a recording ([`to_ask`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ask {
    /// The voices the steps before the LLM leave unnamed, minor ones aside.
    pub voices: Vec<String>,
    /// The speakers those steps named after a person, as (id, name).
    pub known: Vec<(String, String)>,
    /// People other than the user are listed (`--with`): the LLM's names
    /// must be theirs ([`super::llm_names::verify`]).
    pub listed: bool,
}

/// Which of `meta`'s voices the LLM is asked about: those the user, the
/// user's name and the participants leave unnamed.
pub fn to_ask(meta: &RecordingMeta, user_name: &str) -> Ask {
    let naming = Naming::of(meta, user_name);
    let named = before_llm(&meta.speakers, &naming);
    let mut ask = Ask {
        listed: !naming.listed_people().is_empty(),
        ..Ask::default()
    };
    for (speaker, named) in meta.speakers.iter().zip(named) {
        match named {
            None if voice_track(&speaker.id).is_some() && !speaker.minor => {
                ask.voices.push(speaker.id.clone())
            }
            Some(n) if matches!(n.source.as_str(), SOURCE_USER | SOURCE_WITH) => {
                ask.known.push((speaker.id.clone(), n.name))
            }
            _ => {}
        }
    }
    ask
}

/// The LLM's names a processed recording keeps (`source` `llm`), to name
/// its speakers again without asking ([`rename_speaker`]): a name the user
/// gives one speaker can take another's.
pub fn kept_claims(speakers: &[Speaker]) -> Claims {
    speakers
        .iter()
        .filter(|s| s.source == SOURCE_LLM)
        .map(|s| {
            (
                s.id.clone(),
                Claim {
                    name: s.name.clone(),
                    evidence: s.evidence.clone().unwrap_or_default(),
                },
            )
        })
        .collect()
}

/// The speaker `query` means: its id (`S2`, case aside), else the name it
/// has now (`Speaker 2`, `Sarah`; case and spacing aside), which must be
/// one speaker's.
pub fn resolve_speaker<'a>(speakers: &'a [Speaker], query: &str) -> Result<&'a Speaker, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("which speaker? Give its id (S2), \"Speaker N\" or its name".to_string());
    }
    if let Some(speaker) = speakers.iter().find(|s| s.id.eq_ignore_ascii_case(query)) {
        return Ok(speaker);
    }
    let by_name: Vec<&Speaker> = speakers
        .iter()
        .filter(|s| normalize(&s.name) == normalize(query))
        .collect();
    match by_name.as_slice() {
        [speaker] => Ok(speaker),
        [] => {
            let all: Vec<String> = speakers
                .iter()
                .map(|s| format!("{} ({})", s.id, s.name))
                .collect();
            Err(format!(
                "no speaker {query:?}; the speakers are {}",
                if all.is_empty() {
                    "none".to_string()
                } else {
                    all.join(", ")
                }
            ))
        }
        several => {
            let ids: Vec<&str> = several.iter().map(|s| s.id.as_str()).collect();
            Err(format!(
                "{} speakers are called {query:?} ({}): give its id",
                several.len(),
                ids.join(", ")
            ))
        }
    }
}

/// Refuse a recording without a transcript: its speakers are not known
/// until it is transcribed.
pub fn ensure_transcribed(meta: &RecordingMeta) -> anyhow::Result<()> {
    if meta.status == RecordingState::Done {
        return Ok(());
    }
    let state = serde_json::to_string(&meta.status).unwrap_or_default();
    anyhow::bail!(
        "recording {} has no transcript yet (status {}): name its speakers once it is \
         transcribed",
        meta.id,
        state.trim_matches('"')
    )
}

/// `whisrs recording name`: in the transcribed recording in `dir`, give the
/// speaker `speaker` means ([`resolve_speaker`]) the name `name`, or clear
/// the name the user gave it when `name` is blank. The name is kept in
/// `speaker_names`, every speaker is named again (the LLM's names are kept,
/// [`kept_claims`], and it is not asked again), and `recording.json` and the
/// transcript are written; nothing is transcribed. Returns the speaker's id
/// and the new meta. The caller keeps the recording from being processed
/// meanwhile.
pub fn rename_speaker(
    dir: &Path,
    config: &Config,
    speaker: &str,
    name: &str,
) -> anyhow::Result<(String, RecordingMeta)> {
    let mut meta = store::read_meta(dir)?;
    ensure_transcribed(&meta)?;
    let id = resolve_speaker(&meta.speakers, speaker)
        .map_err(|e| anyhow::anyhow!("recording {}: {e}", meta.id))?
        .id
        .clone();
    if name.trim().is_empty() {
        meta.speaker_names.remove(&id);
    } else {
        let name = validate_person_name(name).map_err(anyhow::Error::msg)?;
        // A voice's name follows where it first spoke; a track's own
        // speaker is one per track, so its id is enough.
        let anchor = voice_track(&id).and_then(|_| anchor_of(&meta.segments, &id));
        meta.speaker_names
            .insert(id.clone(), SpeakerName { name, anchor });
    }
    let claims = kept_claims(&meta.speakers);
    let user = super::process::user_name(&RecordingConfig::effective(config));
    let before = meta.clone();
    name_speakers(&mut meta, &user, &claims);
    // The summary names speakers by the labels it was made with.
    if super::summary::has_summary(&meta) && super::summary::names_changed(&before, &meta) {
        meta.summary_stale = true;
    }
    super::process::save(dir, &meta)?;
    Ok((id, meta))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn speaker(id: &str, channel: TrackKind) -> Speaker {
        Speaker {
            id: id.into(),
            name: String::new(),
            source: String::new(),
            evidence: None,
            talk_ms: 60_000,
            channel: Some(channel),
            minor: false,
        }
    }

    fn minor(id: &str) -> Speaker {
        Speaker {
            minor: true,
            talk_ms: 1_000,
            ..speaker(id, TrackKind::System)
        }
    }

    fn seg(start_ms: u64, speaker: &str) -> TranscriptSegment {
        TranscriptSegment {
            start_ms,
            end_ms: start_ms + 1000,
            speaker: speaker.into(),
            channel: if speaker.starts_with('S') || speaker == OTHERS {
                TrackKind::System
            } else {
                TrackKind::Mic
            },
            text: "words".into(),
        }
    }

    /// One segment per speaker, in the order given, each first heard a
    /// second after the one before.
    fn in_order(speakers: &[Speaker]) -> Vec<TranscriptSegment> {
        speakers
            .iter()
            .enumerate()
            .map(|(i, s)| seg(i as u64 * 1000, &s.id))
            .collect()
    }

    /// A call: the user, then the system voices in the order given
    /// ([`in_order`]).
    fn call(voices: &[Speaker]) -> (Vec<Speaker>, Vec<TranscriptSegment>) {
        let mut speakers = vec![speaker(ME, TrackKind::Mic)];
        speakers.extend(voices.iter().cloned());
        let segments = in_order(&speakers);
        (speakers, segments)
    }

    fn people(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    struct Case {
        participants: Vec<String>,
        shared_mic: bool,
        user_names: BTreeMap<String, SpeakerName>,
        claims: Claims,
    }

    impl Case {
        fn with(names: &[&str]) -> Self {
            Case {
                participants: people(names),
                shared_mic: false,
                user_names: BTreeMap::new(),
                claims: Claims::new(),
            }
        }

        fn user(mut self, id: &str, name: &str) -> Self {
            self.user_names.insert(id.into(), SpeakerName::plain(name));
            self
        }

        /// The LLM names `id` `name`, quoting "I'm <name>".
        fn llm(mut self, id: &str, name: &str) -> Self {
            self.claims.insert(
                id.into(),
                Claim {
                    name: name.into(),
                    evidence: format!("I'm {name}"),
                },
            );
            self
        }

        fn naming(&self) -> Naming<'_> {
            Naming {
                user_name: "Yosif",
                others_label: "Others",
                participants: &self.participants,
                shared_mic: self.shared_mic,
                user_names: &self.user_names,
            }
        }

        fn run(
            &self,
            speakers: &[Speaker],
            segments: &[TranscriptSegment],
        ) -> Vec<(String, String)> {
            decide(speakers, segments, &self.naming(), &self.claims)
                .into_iter()
                .zip(speakers)
                .map(|(n, s)| (s.id.clone(), format!("{} ({})", n.name, n.source)))
                .collect()
        }
    }

    fn expect(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    /// The mic is the user; one remote voice keeps the others label, and
    /// more are `Speaker N` by first appearance, without participants.
    #[test]
    fn without_participants_nobody_is_named() {
        let (speakers, segments) = call(&[speaker("S1", TrackKind::System)]);
        assert_eq!(
            Case::with(&[]).run(&speakers, &segments),
            expect(&[("me", "Yosif (self)"), ("S1", "Others (channel)")])
        );
        let (speakers, mut segments) = call(&[
            speaker("S1", TrackKind::System),
            speaker("S2", TrackKind::System),
        ]);
        // S2 speaks first: it is Speaker 1.
        segments[2].start_ms = 100;
        assert_eq!(
            Case::with(&[]).run(&speakers, &segments),
            expect(&[
                ("me", "Yosif (self)"),
                ("S1", "Speaker 2 (none)"),
                ("S2", "Speaker 1 (none)"),
            ])
        );
    }

    /// Only a name given to a person assigns a participant: the user, here
    /// "Sam", is not the participant "Samantha" whose name theirs starts.
    #[test]
    fn the_users_own_name_assigns_nobody() {
        let (speakers, segments) = call(&[speaker("S1", TrackKind::System)]);
        let case = Case::with(&["Samantha"]);
        let naming = Naming {
            user_name: "Sam",
            ..case.naming()
        };
        let named = decide(&speakers, &segments, &naming, &Claims::new());
        assert_eq!(
            (named[1].name.as_str(), named[1].source.as_str()),
            ("Samantha", "with")
        );
    }

    /// A track's own speaker holds speech no voice was found for: it is
    /// never named after a participant, and never counts as a voice.
    #[test]
    fn a_tracks_own_speaker_is_never_a_participant() {
        let (speakers, segments) = call(&[
            speaker("S1", TrackKind::System),
            speaker("S2", TrackKind::System),
            speaker(OTHERS, TrackKind::System),
        ]);
        let case = Case::with(&["Sarah", "Omar"]).user("S1", "Sarah");
        assert_eq!(
            case.run(&speakers, &segments),
            expect(&[
                ("me", "Yosif (self)"),
                ("S1", "Sarah (user)"),
                ("S2", "Omar (last)"),
                ("others", "Others (channel)"),
            ])
        );
        let speakers = vec![
            speaker("M1", TrackKind::Mic),
            speaker(MIC, TrackKind::Mic),
            speaker("S1", TrackKind::System),
        ];
        let segments = in_order(&speakers);
        let mut case = Case::with(&["Sarah"]);
        case.shared_mic = true;
        assert_eq!(
            case.run(&speakers, &segments),
            expect(&[
                ("M1", "Speaker 1 (none)"),
                ("mic", "Mic (channel)"),
                ("S1", "Others (channel)"),
            ])
        );
    }

    /// One remote voice and one participant: the voice is them. A minor
    /// voice beside it does not count, and is not named.
    #[test]
    fn one_voice_and_one_participant_is_with() {
        let (speakers, segments) = call(&[speaker("S1", TrackKind::System)]);
        assert_eq!(
            Case::with(&["Sarah"]).run(&speakers, &segments),
            expect(&[("me", "Yosif (self)"), ("S1", "Sarah (with)")])
        );
        let (speakers, segments) = call(&[speaker("S1", TrackKind::System), minor("S2")]);
        assert_eq!(
            Case::with(&["Sarah"]).run(&speakers, &segments),
            expect(&[
                ("me", "Yosif (self)"),
                ("S1", "Sarah (with)"),
                ("S2", "Speaker 1 (none)"),
            ])
        );
        // The user listed by first name, with the full name as the user's
        // name (the passwd default): still the user.
        let case = Case::with(&["Yosif", "Sarah"]);
        let naming = Naming {
            user_name: "Yosif Soliman",
            ..case.naming()
        };
        let named = decide(&speakers, &segments, &naming, &Claims::new());
        assert_eq!(
            (named[1].name.as_str(), named[1].source.as_str()),
            ("Sarah", "with")
        );
        // The user listed among the participants is the user, on the mic.
        let (speakers, segments) = call(&[speaker("S1", TrackKind::System)]);
        assert_eq!(
            Case::with(&["yosif", "Sarah"]).run(&speakers, &segments),
            expect(&[("me", "Yosif (self)"), ("S1", "Sarah (with)")])
        );
    }

    /// Two participants for one voice, or two voices for one participant:
    /// nobody is named.
    #[test]
    fn with_needs_one_of_each() {
        let (speakers, segments) = call(&[speaker("S1", TrackKind::System)]);
        assert_eq!(
            Case::with(&["Sarah", "Omar"]).run(&speakers, &segments),
            expect(&[("me", "Yosif (self)"), ("S1", "Others (channel)")])
        );
        let (speakers, segments) = call(&[
            speaker("S1", TrackKind::System),
            speaker("S2", TrackKind::System),
        ]);
        assert_eq!(
            Case::with(&["Sarah"]).run(&speakers, &segments),
            expect(&[
                ("me", "Yosif (self)"),
                ("S1", "Speaker 1 (none)"),
                ("S2", "Speaker 2 (none)"),
            ])
        );
        // An unseparated system track is not a voice: it keeps its label.
        let (speakers, segments) = call(&[speaker(OTHERS, TrackKind::System)]);
        assert_eq!(
            Case::with(&["Sarah"]).run(&speakers, &segments),
            expect(&[("me", "Yosif (self)"), ("others", "Others (channel)")])
        );
    }

    /// A shared mic with other voices on it: the one participant may be in
    /// the room, so the one remote voice is not named after them.
    #[test]
    fn with_skips_a_shared_mic_with_other_voices() {
        let speakers = vec![
            speaker("M1", TrackKind::Mic),
            speaker("M2", TrackKind::Mic),
            speaker("S1", TrackKind::System),
        ];
        let segments = in_order(&speakers);
        let mut case = Case::with(&["Sarah"]);
        case.shared_mic = true;
        assert_eq!(
            case.run(&speakers, &segments),
            expect(&[
                ("M1", "Speaker 1 (none)"),
                ("M2", "Speaker 2 (none)"),
                ("S1", "Others (channel)"),
            ])
        );
        // A shared mic that held one voice is the user alone.
        let (speakers, segments) = call(&[speaker("S1", TrackKind::System)]);
        assert_eq!(
            case.run(&speakers, &segments),
            expect(&[("me", "Yosif (self)"), ("S1", "Sarah (with)")])
        );
    }

    fn three_voices() -> (Vec<Speaker>, Vec<TranscriptSegment>) {
        call(&[
            speaker("S1", TrackKind::System),
            speaker("S2", TrackKind::System),
            speaker("S3", TrackKind::System),
        ])
    }

    /// The user's names win over every step, on any speaker, and the
    /// unnamed are numbered among themselves.
    #[test]
    fn the_users_names_win() {
        let (speakers, segments) = three_voices();
        let case = Case::with(&[])
            .user("S2", "Dana")
            .user("me", "Yo")
            .user("S9", "Ghost");
        assert_eq!(
            case.run(&speakers, &segments),
            expect(&[
                ("me", "Yo (user)"),
                ("S1", "Speaker 1 (none)"),
                ("S2", "Dana (user)"),
                ("S3", "Speaker 2 (none)"),
            ])
        );
        // Over `with` too.
        let (speakers, segments) = call(&[speaker("S1", TrackKind::System)]);
        assert_eq!(
            Case::with(&["Sarah"])
                .user("S1", "Omar")
                .run(&speakers, &segments),
            expect(&[("me", "Yosif (self)"), ("S1", "Omar (user)")])
        );
    }

    /// With every remote voice but one named and one participant left,
    /// the last voice is them.
    #[test]
    fn the_last_voice_is_the_last_participant() {
        let (speakers, segments) = three_voices();
        let case = Case::with(&["Sarah", "Omar Haddad", "Dana"])
            .user("S1", "Sarah")
            // A first name assigns the participant it starts.
            .user("S3", "omar");
        assert_eq!(
            case.run(&speakers, &segments),
            expect(&[
                ("me", "Yosif (self)"),
                ("S1", "Sarah (user)"),
                ("S2", "Dana (last)"),
                ("S3", "omar (user)"),
            ])
        );
    }

    /// The last voice is skipped unless voices and participants are as
    /// many, the mic is not shared, one voice and one participant are
    /// left, and minor voices do not count.
    #[test]
    fn the_last_voice_has_exclusions() {
        let (speakers, segments) = three_voices();
        let named = |case: &Case| {
            case.run(&speakers, &segments)
                .into_iter()
                .find(|(id, _)| id == "S2")
                .unwrap()
                .1
        };
        let two_named = |names: &[&str]| Case::with(names).user("S1", "Sarah").user("S3", "Omar");

        // Three voices, four participants: someone did not speak.
        assert_eq!(
            named(&two_named(&["Sarah", "Omar", "Dana", "Lee"])),
            "Speaker 1 (none)"
        );
        // Three voices, two participants: someone was not listed (here
        // Bob), so the last voice need not be the last participant.
        let case = Case::with(&["Sarah", "Dana"])
            .user("S1", "Sarah")
            .user("S3", "Bob");
        assert_eq!(named(&case), "Speaker 1 (none)");
        // A shared mic: the participants may be in the room.
        let mut shared = two_named(&["Sarah", "Omar", "Dana"]);
        shared.shared_mic = true;
        assert_eq!(named(&shared), "Speaker 1 (none)");
        // The user named S3 after a participant who is already S1: two
        // participants are left for one voice.
        let case = Case::with(&["Sarah", "Omar", "Dana"])
            .user("S1", "Sarah")
            .user("S3", "Sarah");
        assert_eq!(named(&case), "Speaker 1 (none)");
        // Named, but not after a participant: two are left.
        let case = Case::with(&["Sarah", "Omar", "Dana"])
            .user("S1", "Sarah")
            .user("S3", "Bob");
        assert_eq!(named(&case), "Speaker 1 (none)");
        // The baseline: all four hold.
        assert_eq!(named(&two_named(&["Sarah", "Omar", "Dana"])), "Dana (last)");

        // A minor voice is not a voice: two voices for two participants.
        let (speakers, segments) = call(&[
            speaker("S1", TrackKind::System),
            minor("S2"),
            speaker("S3", TrackKind::System),
        ]);
        let case = Case::with(&["Sarah", "Omar"]).user("S1", "Sarah");
        assert_eq!(
            case.run(&speakers, &segments),
            expect(&[
                ("me", "Yosif (self)"),
                ("S1", "Sarah (user)"),
                ("S2", "Speaker 1 (none)"),
                ("S3", "Omar (last)"),
            ])
        );
    }

    /// A participant's first name, full name or a 3+ letter start of the
    /// first name names them; nothing shorter, nothing else.
    #[test]
    fn names_match_participants_loosely_but_not_too_loosely() {
        for (name, person) in [
            ("Sarah", "Sarah"),
            ("sarah  smith", "Sarah Smith"),
            ("Sarah", "Sarah Smith"),
            ("Sam", "Samantha Reed"),
            ("SAMANT", "Samantha"),
        ] {
            assert!(names_participant(name, person), "{name} / {person}");
        }
        for (name, person) in [
            ("Sa", "Sarah"),
            ("Smith", "Sarah Smith"),
            ("Sarah Jones", "Sarah Smith"),
            ("Sarahs", "Sarah"),
            ("", "Sarah"),
        ] {
            assert!(!names_participant(name, person), "{name} / {person}");
        }
    }

    /// `--with` takes names separated by commas, trimmed, each one line
    /// and at most 64 characters, at most 20 of them, none twice; a blank
    /// list is nobody.
    #[test]
    fn participants_are_validated() {
        assert_eq!(
            parse_participants(" Sarah ,Omar Haddad"),
            Ok(people(&["Sarah", "Omar Haddad"]))
        );
        assert_eq!(parse_participants("  "), Ok(Vec::new()));
        let longest = "é".repeat(PERSON_NAME_MAX);
        assert_eq!(parse_participants(&longest), Ok(vec![longest.clone()]));
        let twenty = vec!["x"; PARTICIPANTS_MAX]
            .iter()
            .enumerate()
            .map(|(i, x)| format!("{x}{i}"))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(parse_participants(&twenty).unwrap().len(), PARTICIPANTS_MAX);

        for (list, want) in [
            ("Sarah,,Omar", "a name is empty"),
            ("Sarah,", "a name is empty"),
            (&format!("{longest}x"), "at most 64"),
            ("Sarah\u{7}", "single line"),
            ("Sarah, sarah", "listed twice"),
            (&format!("{twenty},y"), "at most 20"),
        ] {
            let err = parse_participants(list).unwrap_err();
            assert!(err.contains(want), "{list:?}: {err}");
        }
        // Over IPC a name arrives whole: a comma in it is refused.
        let err = validate_participants(people(&["Sarah, Omar"])).unwrap_err();
        assert!(err.contains("comma"), "{err}");
        assert_eq!(
            validate_participants(people(&[" Sarah\t"])),
            Ok(people(&["Sarah"]))
        );
    }

    /// Without participants the LLM may give any name; it comes with its
    /// evidence. Its names count for the last voice.
    #[test]
    fn the_llm_names_unnamed_voices() {
        let (speakers, segments) = three_voices();
        let case = Case::with(&[]).llm("S2", "Dana");
        let named = decide(&speakers, &segments, &case.naming(), &case.claims);
        assert_eq!(
            (named[2].name.as_str(), named[2].source.as_str()),
            ("Dana", "llm")
        );
        assert_eq!(named[2].evidence.as_deref(), Some("I'm Dana"));
        assert_eq!(
            (named[1].name.as_str(), named[3].name.as_str()),
            ("Speaker 1", "Speaker 2")
        );

        let case = Case::with(&["Sarah", "Omar", "Dana"])
            .user("S1", "Sarah")
            .llm("S3", "Omar");
        assert_eq!(
            case.run(&speakers, &segments),
            expect(&[
                ("me", "Yosif (self)"),
                ("S1", "Sarah (user)"),
                ("S2", "Dana (last)"),
                ("S3", "Omar (llm)"),
            ])
        );
    }

    /// With participants, the LLM's name must be one of them (full name,
    /// first name or a 3+ letter start of it), and the participant's name
    /// is used as listed; anything else stays unnamed.
    #[test]
    fn the_llm_names_only_participants() {
        let (speakers, segments) = three_voices();
        let case = Case::with(&["Samantha Reed", "Omar", "Lee Park", "Lee Chan"])
            .llm("S1", "sam")
            .llm("S2", "Bob")
            .llm("S3", "Lee");
        assert_eq!(
            case.run(&speakers, &segments),
            expect(&[
                ("me", "Yosif (self)"),
                ("S1", "Samantha Reed (llm)"),
                // Not listed.
                ("S2", "Speaker 1 (none)"),
                // Two participants are Lee.
                ("S3", "Speaker 2 (none)"),
            ])
        );
        let case = Case::with(&["Sarah", "Omar"]).llm("S1", "Sa");
        assert_eq!(
            case.run(&speakers, &segments)[1].1,
            "Speaker 1 (none)",
            "too short"
        );
    }

    /// No two speakers get one name: two claims of one name are both
    /// refused, as is a claim of a name another step gave, or of a
    /// participant someone already is. The user's name is never given.
    #[test]
    fn the_llm_never_gives_a_name_twice_or_the_users() {
        let (speakers, segments) = three_voices();
        let named = |case: Case| case.run(&speakers, &segments);
        assert_eq!(
            named(Case::with(&[]).llm("S1", "Dana").llm("S3", "dana")),
            expect(&[
                ("me", "Yosif (self)"),
                ("S1", "Speaker 1 (none)"),
                ("S2", "Speaker 2 (none)"),
                ("S3", "Speaker 3 (none)"),
            ])
        );
        assert_eq!(
            named(Case::with(&[]).user("S1", "Dana").llm("S2", "Dana"))[2].1,
            "Speaker 1 (none)"
        );
        // The user called S1 "Sarah": the participant "Sarah Smith" is
        // taken, so S2 is not her, and is the last one left instead.
        assert_eq!(
            named(
                Case::with(&["Sarah Smith", "Omar", "Dana"])
                    .user("S1", "Sarah")
                    .llm("S2", "Sarah Smith")
                    .llm("S3", "Dana")
            ),
            expect(&[
                ("me", "Yosif (self)"),
                ("S1", "Sarah (user)"),
                ("S2", "Omar (last)"),
                ("S3", "Dana (llm)"),
            ])
        );
        for user in ["Yosif", "yosif soliman", "Yos"] {
            assert_eq!(
                named(Case::with(&[]).llm("S1", user))[1].1,
                "Speaker 1 (none)",
                "{user}"
            );
        }
        // Listed or not, the user is never a remote voice.
        assert_eq!(
            named(Case::with(&["Yosif", "Sarah", "Omar"]).llm("S1", "Yosif"))[1].1,
            "Speaker 1 (none)"
        );
    }

    /// The LLM names only voices still unnamed: never a minor voice, the
    /// user, a track's own speaker (`others`, `mic`), or a voice an earlier
    /// step named.
    #[test]
    fn the_llm_names_only_voices_left() {
        let speakers = vec![
            speaker(ME, TrackKind::Mic),
            speaker("S1", TrackKind::System),
            minor("S2"),
            speaker("S3", TrackKind::System),
            speaker(OTHERS, TrackKind::System),
        ];
        let segments = in_order(&speakers);
        let case = Case::with(&[])
            .user("S3", "Omar")
            .llm("me", "Dana")
            .llm("S1", "Lee")
            .llm("S2", "Kim")
            .llm("S3", "Ali")
            .llm("others", "Max");
        assert_eq!(
            case.run(&speakers, &segments),
            expect(&[
                ("me", "Yosif (self)"),
                ("S1", "Lee (llm)"),
                ("S2", "Speaker 1 (none)"),
                ("S3", "Omar (user)"),
                ("others", "Others (channel)"),
            ])
        );
        let speakers = vec![
            speaker("M1", TrackKind::Mic),
            speaker("M2", TrackKind::Mic),
            speaker(MIC, TrackKind::Mic),
        ];
        let segments = in_order(&speakers);
        let mut case = Case::with(&[]).llm("M2", "Lee").llm("mic", "Max");
        case.shared_mic = true;
        assert_eq!(
            case.run(&speakers, &segments),
            expect(&[
                ("M1", "Speaker 1 (none)"),
                ("M2", "Lee (llm)"),
                ("mic", "Mic (channel)"),
            ])
        );
    }

    /// The LLM is asked about the voices the steps before it leave
    /// unnamed, minor ones aside, and told who the others are.
    #[test]
    fn the_llm_is_asked_about_the_voices_left() {
        let mut meta = RecordingMeta::new("m".into(), None, chrono::Local::now());
        let (speakers, segments) = call(&[
            speaker("S1", TrackKind::System),
            minor("S2"),
            speaker("S3", TrackKind::System),
            speaker("S4", TrackKind::System),
        ]);
        meta.speakers = speakers;
        meta.segments = segments;
        meta.speaker_names = [("S3".to_string(), SpeakerName::plain("Omar"))].into();
        assert_eq!(
            to_ask(&meta, "Tester"),
            Ask {
                voices: vec!["S1".into(), "S4".into()],
                known: vec![("S3".into(), "Omar".into())],
                listed: false,
            }
        );
        // One voice and one participant: nobody is left to ask about.
        meta.speakers.truncate(3);
        meta.speaker_names.clear();
        meta.participants = people(&["Sarah"]);
        assert_eq!(
            to_ask(&meta, "Tester"),
            Ask {
                voices: Vec::new(),
                known: vec![("S1".into(), "Sarah".into())],
                listed: true,
            }
        );
    }

    /// A speaker is found by its id, case aside, or by the name it has
    /// now, case and spacing aside; an unknown or shared name is refused.
    #[test]
    fn speakers_are_found_by_id_or_name() {
        let (mut speakers, _) = three_voices();
        for (s, name) in speakers
            .iter_mut()
            .zip(["Yosif", "Speaker 1", "Sarah Smith", "Speaker 2"])
        {
            s.name = name.into();
        }
        let id = |query: &str| resolve_speaker(&speakers, query).map(|s| s.id.as_str());
        assert_eq!(id("S2"), Ok("S2"));
        assert_eq!(id(" s3 "), Ok("S3"));
        assert_eq!(id("ME"), Ok("me"));
        assert_eq!(id("Speaker 2"), Ok("S3"));
        assert_eq!(id("speaker  1"), Ok("S1"));
        assert_eq!(id("sarah smith"), Ok("S2"));
        let err = id("Speaker 9").unwrap_err();
        assert!(err.contains("no speaker \"Speaker 9\""), "{err}");
        assert!(err.contains("S1 (Speaker 1)"), "{err}");
        assert!(id(" ").unwrap_err().contains("which speaker"));
        speakers[3].name = "Sarah Smith".into();
        let err = resolve_speaker(&speakers, "Sarah Smith").unwrap_err();
        assert!(
            err.contains("2 speakers are called") && err.contains("S2, S3"),
            "{err}"
        );
    }

    /// A user's name for a system voice first heard at `first_ms`.
    fn anchored(name: &str, first_ms: u64) -> SpeakerName {
        SpeakerName {
            name: name.into(),
            anchor: Some(VoiceAnchor {
                channel: TrackKind::System,
                first_ms,
            }),
        }
    }

    /// The user's names follow their voices by where they first spoke:
    /// a name moves to the voice now first heard within a second of its
    /// anchor, on its track, whatever its id; one that finds none is
    /// dropped with a note; one without an anchor keeps its id.
    #[test]
    fn user_names_follow_their_voice() {
        let mut meta = RecordingMeta::new("m".into(), None, chrono::Local::now());
        // Sarah was S1, first heard at 2.0 s; a voice heard at 1.0 s is S1 now.
        let (speakers, segments) = three_voices();
        meta.speakers = speakers;
        meta.segments = segments;
        meta.speaker_names = BTreeMap::from([
            ("S1".into(), anchored("Sarah", 2000)),
            ("S2".into(), anchored("Omar", 3900)),
            ("S3".into(), anchored("Lost", 6000)),
            ("me".into(), SpeakerName::plain("Yo")),
        ]);
        name_speakers(&mut meta, "Tester", &Claims::new());
        let names: Vec<(&str, &str, &str)> = meta
            .speakers
            .iter()
            .map(|s| (s.id.as_str(), s.name.as_str(), s.source.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                ("me", "Yo", "user"),
                ("S1", "Speaker 1", "none"),
                ("S2", "Sarah", "user"),
                ("S3", "Omar", "user"),
            ]
        );
        assert_eq!(
            meta.speaker_names.keys().collect::<Vec<_>>(),
            ["S2", "S3", "me"],
            "stored under the ids they have now"
        );
        assert_eq!(
            meta.speaker_names["S2"],
            anchored("Sarah", 2000),
            "anchor kept"
        );
        assert_eq!(meta.errors.len(), 1, "{:?}", meta.errors);
        assert!(
            meta.errors[0].starts_with(USER_NAME_LOST)
                && meta.errors[0].contains("\"Lost\" (was S3)"),
            "{:?}",
            meta.errors
        );

        // More than a second away: no voice, dropped.
        let mut one = meta.clone();
        one.errors.clear();
        one.segments
            .retain(|s| s.speaker != "S2" && s.speaker != "S3");
        one.speakers.truncate(2);
        let mut far = one.clone();
        far.speaker_names =
            BTreeMap::from([("S1".into(), anchored("Far", 1000 + ANCHOR_TOLERANCE_MS + 1))]);
        name_speakers(&mut far, "Tester", &Claims::new());
        assert!(far.speaker_names.is_empty());
        assert_eq!(far.speakers[1].name, "Others");
        // On the other track, which has no voices told apart: it waits.
        let on_the_mic = SpeakerName {
            name: "Mic".into(),
            anchor: Some(VoiceAnchor {
                channel: TrackKind::Mic,
                first_ms: 1000,
            }),
        };
        let mut mic = one.clone();
        mic.speaker_names = BTreeMap::from([("M1".into(), on_the_mic.clone())]);
        name_speakers(&mut mic, "Tester", &Claims::new());
        assert_eq!(
            mic.speaker_names,
            BTreeMap::from([("M1".into(), on_the_mic)])
        );
        assert_eq!(mic.speakers[1].name, "Others", "not on the system voice");
        assert!(mic.errors.is_empty(), "{:?}", mic.errors);
        // A name stored without an anchor stays on its id.
        let mut meta = meta.clone();
        meta.speaker_names = BTreeMap::from([("S1".into(), SpeakerName::plain("Kim"))]);
        name_speakers(&mut meta, "Tester", &Claims::new());
        assert_eq!(meta.speakers[1].name, "Kim");
    }

    /// Names and voices are matched as a whole, nearest first: Omar's
    /// voice, now first heard at 2.5 s, is nearer Omar's anchor (2.6 s)
    /// than Sarah's (2.0 s), so it is Omar's; Sarah's voice is gone, and
    /// the stranger at 3.4 s is no one's, rather than Omar's.
    #[test]
    fn names_and_voices_are_matched_as_a_whole() {
        let mut meta = RecordingMeta::new("m".into(), None, chrono::Local::now());
        meta.speakers = vec![
            speaker(ME, TrackKind::Mic),
            speaker("S1", TrackKind::System),
            speaker("S2", TrackKind::System),
        ];
        meta.segments = vec![seg(0, ME), seg(2500, "S1"), seg(3400, "S2")];
        meta.speaker_names = BTreeMap::from([
            ("S1".into(), anchored("Sarah", 2000)),
            ("S2".into(), anchored("Omar", 2600)),
        ]);
        name_speakers(&mut meta, "Tester", &Claims::new());
        let names: Vec<(&str, &str)> = meta
            .speakers
            .iter()
            .map(|s| (s.id.as_str(), s.name.as_str()))
            .collect();
        assert_eq!(
            names,
            [("me", "Tester"), ("S1", "Omar"), ("S2", "Speaker 1")]
        );
        assert_eq!(
            meta.speaker_names,
            BTreeMap::from([("S1".into(), anchored("Omar", 2600))])
        );
        assert_eq!(meta.errors.len(), 1, "{:?}", meta.errors);
        assert!(
            meta.errors[0].contains("\"Sarah\" (was S1)"),
            "{:?}",
            meta.errors
        );
    }

    /// When an attempt tells no voices apart on a name's track (the
    /// sidecar was down, Deepgram refused), the name waits as it is, and
    /// the next attempt that tells them apart puts it back on its voice.
    #[test]
    fn names_wait_while_voices_are_not_told_apart() {
        let mut meta = RecordingMeta::new("m".into(), None, chrono::Local::now());
        let names = BTreeMap::from([("S2".to_string(), anchored("Omar", 2000))]);
        meta.speakers = vec![
            speaker(ME, TrackKind::Mic),
            speaker(OTHERS, TrackKind::System),
        ];
        meta.segments = vec![seg(0, ME), seg(1000, OTHERS)];
        meta.speaker_names = names.clone();
        name_speakers(&mut meta, "Tester", &Claims::new());
        assert_eq!(meta.speaker_names, names, "kept as it was");
        assert!(meta.errors.is_empty(), "{:?}", meta.errors);
        assert_eq!(meta.speakers[1].name, "Others");

        let (speakers, segments) = three_voices();
        meta.speakers = speakers;
        meta.segments = segments;
        name_speakers(&mut meta, "Tester", &Claims::new());
        assert_eq!(meta.speakers[2].name, "Omar", "back on its voice");
        assert!(meta.errors.is_empty(), "{:?}", meta.errors);
    }

    /// A transcribed recording of three voices and three participants,
    /// with its transcript, in a temporary folder.
    fn transcribed(participants: &[&str]) -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let (id, dir) = store::create(tmp.path(), chrono::Local::now(), Some("sync")).unwrap();
        let mut meta = RecordingMeta::new(id, Some("sync".into()), chrono::Local::now());
        let (speakers, segments) = three_voices();
        meta.speakers = speakers;
        meta.segments = segments;
        meta.participants = people(participants);
        meta.status = RecordingState::Done;
        name_speakers(&mut meta, "Tester", &Claims::new());
        store::write_meta(&dir, &meta).unwrap();
        (tmp, dir)
    }

    fn config() -> Config {
        toml::from_str("[recording]\nuser_name = \"Tester\"\n").unwrap()
    }

    fn names_in(dir: &Path) -> Vec<(String, String)> {
        store::read_meta(dir)
            .unwrap()
            .speakers
            .iter()
            .map(|s| (s.id.clone(), format!("{} ({})", s.name, s.source)))
            .collect()
    }

    /// A name that changes makes a real summary stale, since it names the
    /// speakers by their old labels; a name that changes nothing, and a
    /// summary that only says the transcript was too short, stay as they
    /// are.
    #[test]
    fn a_new_name_makes_the_summary_stale() {
        let (_tmp, dir) = transcribed(&["Sarah", "Omar", "Dana"]);
        let mut meta = store::read_meta(&dir).unwrap();
        meta.summary = Some(super::super::summary::TOO_SHORT.into());
        store::write_meta(&dir, &meta).unwrap();
        let (_, renamed) = rename_speaker(&dir, &config(), "S2", "Omar").unwrap();
        assert!(!renamed.summary_stale, "too short is no summary");

        let mut meta = store::read_meta(&dir).unwrap();
        meta.summary = Some("Speaker 1 agreed.".into());
        store::write_meta(&dir, &meta).unwrap();
        let (_, same) = rename_speaker(&dir, &config(), "S2", "Omar").unwrap();
        assert!(!same.summary_stale, "the same name changes nothing");
        let (_, renamed) = rename_speaker(&dir, &config(), "S1", "Sarah").unwrap();
        assert!(renamed.summary_stale);
        assert!(store::read_meta(&dir).unwrap().summary_stale);
    }

    /// Naming a speaker rewrites the stored names and the transcript at
    /// once, and the other steps run again around it: the unnamed are
    /// numbered again and the last voice left is the last participant.
    #[test]
    fn renaming_a_speaker_names_the_rest_again() {
        let (_tmp, dir) = transcribed(&["Sarah", "Omar", "Dana"]);
        let (id, meta) = rename_speaker(&dir, &config(), "Speaker 2", " Omar ").unwrap();
        assert_eq!(id, "S2");
        assert_eq!(
            meta.speaker_names,
            BTreeMap::from([("S2".into(), anchored("Omar", 2000))]),
            "with where S2 first spoke"
        );
        assert_eq!(
            names_in(&dir),
            expect(&[
                ("me", "Tester (self)"),
                ("S1", "Speaker 1 (none)"),
                ("S2", "Omar (user)"),
                ("S3", "Speaker 2 (none)"),
            ])
        );
        let md = std::fs::read_to_string(store::transcript_path(&dir, &meta.id)).unwrap();
        assert!(md.contains("  - \"Omar\"\n"), "{md}");
        assert!(md.contains("] Omar:** words"), "{md}");

        // By its current name; the last voice left is then Dana.
        rename_speaker(&dir, &config(), "speaker 1", "Sarah").unwrap();
        assert_eq!(
            names_in(&dir),
            expect(&[
                ("me", "Tester (self)"),
                ("S1", "Sarah (user)"),
                ("S2", "Omar (user)"),
                ("S3", "Dana (last)"),
            ])
        );
        // A longer name for a participant still assigns them.
        rename_speaker(&dir, &config(), "omar", "Omar Haddad").unwrap();
        assert_eq!(names_in(&dir)[3].1, "Dana (last)");

        // An empty name clears the user's: S2 is unnamed again.
        let (_, meta) = rename_speaker(&dir, &config(), "S2", "  ").unwrap();
        assert_eq!(
            meta.speaker_names,
            BTreeMap::from([("S1".into(), anchored("Sarah", 1000))])
        );
        assert_eq!(
            names_in(&dir),
            expect(&[
                ("me", "Tester (self)"),
                ("S1", "Sarah (user)"),
                ("S2", "Speaker 1 (none)"),
                ("S3", "Speaker 2 (none)"),
            ])
        );
        let md = std::fs::read_to_string(store::transcript_path(&dir, &meta.id)).unwrap();
        assert!(!md.contains("Omar"), "{md}");
    }

    /// The LLM's names stay when another speaker is named, and give way
    /// to the user's: a name the user takes is no longer the LLM's.
    #[test]
    fn the_llms_names_stay_unless_the_user_takes_them() {
        let (_tmp, dir) = transcribed(&[]);
        let mut meta = store::read_meta(&dir).unwrap();
        let claims = Claims::from([(
            "S1".to_string(),
            Claim {
                name: "Sarah".into(),
                evidence: "I'm Sarah".into(),
            },
        )]);
        name_speakers(&mut meta, "Tester", &claims);
        store::write_meta(&dir, &meta).unwrap();

        rename_speaker(&dir, &config(), "S2", "Omar").unwrap();
        let meta = store::read_meta(&dir).unwrap();
        assert_eq!(
            (
                meta.speakers[1].name.as_str(),
                meta.speakers[1].source.as_str()
            ),
            ("Sarah", "llm")
        );
        assert_eq!(meta.speakers[1].evidence.as_deref(), Some("I'm Sarah"));

        rename_speaker(&dir, &config(), "S3", "sarah").unwrap();
        assert_eq!(
            names_in(&dir),
            expect(&[
                ("me", "Tester (self)"),
                ("S1", "Speaker 1 (none)"),
                ("S2", "Omar (user)"),
                ("S3", "sarah (user)"),
            ])
        );
    }

    /// An unknown speaker, a bad name and a recording without a
    /// transcript are refused, and nothing is written.
    #[test]
    fn renaming_refuses_what_it_cannot_name() {
        let (_tmp, dir) = transcribed(&[]);
        let before = std::fs::read(dir.join("recording.json")).unwrap();
        for (speaker, name, want) in [
            ("S9", "Dana", "no speaker \"S9\""),
            ("S1", "Dana\nSmith", "single line"),
            ("S1", &"x".repeat(PERSON_NAME_MAX + 1), "at most 64"),
        ] {
            let err = rename_speaker(&dir, &config(), speaker, name).unwrap_err();
            assert!(
                format!("{err:#}").contains(want),
                "{speaker} {name}: {err:#}"
            );
        }
        assert_eq!(std::fs::read(dir.join("recording.json")).unwrap(), before);
        let mut meta = store::read_meta(&dir).unwrap();
        meta.status = RecordingState::Processing;
        store::write_meta(&dir, &meta).unwrap();
        let before = std::fs::read(dir.join("recording.json")).unwrap();
        let err = rename_speaker(&dir, &config(), "S1", "Dana").unwrap_err();
        assert!(
            err.to_string()
                .contains("has no transcript yet (status processing)"),
            "{err}"
        );
        assert_eq!(std::fs::read(dir.join("recording.json")).unwrap(), before);
        assert!(!store::transcript_path(&dir, &meta.id).exists());
    }

    #[test]
    fn voice_ids_name_their_track() {
        assert_eq!(voice_track("S1"), Some(TrackKind::System));
        assert_eq!(voice_track("M12"), Some(TrackKind::Mic));
        for id in ["me", "others", "mic", "S", "Sx", "s1", "", "Ş1"] {
            assert_eq!(voice_track(id), None, "{id}");
        }
    }

    /// Naming from scratch: names, sources and evidence from an earlier
    /// run go, ids, talk times and order stay.
    #[test]
    fn name_speakers_renames_in_place() {
        let mut meta = RecordingMeta::new("m".into(), None, chrono::Local::now());
        let (speakers, segments) = call(&[speaker("S1", TrackKind::System)]);
        meta.speakers = speakers;
        meta.segments = segments;
        meta.speakers[1].name = "Stale".into();
        meta.speakers[1].source = "llm".into();
        meta.speakers[1].evidence = Some("old".into());
        meta.others_label = Some("Video".into());
        name_speakers(&mut meta, "Tester", &Claims::new());
        let got: Vec<_> = meta
            .speakers
            .iter()
            .map(|s| {
                (
                    s.id.as_str(),
                    s.name.as_str(),
                    s.source.as_str(),
                    s.evidence.clone(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("me", "Tester", "self", None),
                ("S1", "Video", "channel", None)
            ]
        );
        meta.participants = people(&["Sarah"]);
        name_speakers(&mut meta, "Tester", &Claims::new());
        assert_eq!(
            (
                meta.speakers[1].name.as_str(),
                meta.speakers[1].source.as_str()
            ),
            ("Sarah", "with")
        );
        assert_eq!(meta.speakers[1].talk_ms, 60_000);
    }
}
