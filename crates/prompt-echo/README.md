# prompt-echo

Detect Whisper prompt-regurgitation hallucination on silent audio.

## The problem

Whisper-family models (OpenAI `whisper-1`, `gpt-4o-*-transcribe`, and most
Whisper-derived APIs) condition decoding on an optional **prompt** parameter.
When the audio carries no speech, the model has nothing to anchor decoding
to and falls back to its strongest prior (the prompt itself), emitting it
verbatim (or in long contiguous chunks) as the "transcription."

Without filtering, those echoes are typed at the cursor, which for a
multi-hundred-character prompt can take tens of seconds at a configured key
delay.

## The solution

Two conservative heuristics, neither of which false-positives on real speech:

1. **Substring check:** after normalisation (lowercase, strip punctuation,
   collapse whitespace), the entire response is a substring of the prompt.
2. **Word-run check:** the longest contiguous word-run shared between
   response and prompt spans at least 6 words *and* covers at least 70% of
   the response.

Short responses (fewer than 8 normalised characters or 6 words) are never
flagged: they could plausibly be a real utterance that happens to overlap
the prompt's vocabulary.

## Example

```rust
use prompt_echo::is_prompt_echo;

let prompt = "John Doe speaking. Professional, culinary register.";

// Echo detected (the model regurgitated the prompt on silence):
assert!(is_prompt_echo("John Doe speaking. Professional, culinary register.", prompt));

// Real speech not flagged:
assert!(!is_prompt_echo("I'm baking sourdough tonight", prompt));
```

## When NOT to use this

Do **not** use this with streaming backends that do not accept a prompt
parameter. The heuristics assume the API accepts a prompt and that prompt
regurgitation is a known failure mode on silent audio. For prompt-less
backends, false positives from vocabulary overlap are possible (the
heuristics are designed conservatively, but the library's purpose is
prompt-echo detection).

## Installation

```sh
cargo add prompt-echo
```

## License

MIT
