# ASR sidecars

`whisrs` can call any local HTTP ASR service through the generic
`asr-sidecar` backend. Sidecars accept WAV audio as multipart form data and
return a JSON response with a `text` field.

## Contract

Endpoint:

```text
POST /transcribe
```

Multipart fields:

- `file`: WAV audio
- `model`: model identifier
- `language`: ISO 639-1 language code, or `auto`
- `hotwords`: optional vocabulary/context prompt

Response:

```json
{ "text": "transcribed text" }
```

This is the OpenAI `/v1/audio/transcriptions` shape, so an OpenAI-compatible
transcription server needs no wrapper: point `[asr-sidecar] url` at its full
URL. The path is whatever you configure, `/transcribe` is only the default.
The one divergence is the prompt, which whisrs sends as `hotwords` rather than
`prompt`, so a strict OpenAI-compatible server ignores `[general] prompt`.

### Recordings

Recording chunks also send `response_format=verbose_json`,
`timestamp_granularities[]=segment` and `timestamp_granularities[]=word`.
Dictation does not. A 400 or 422 makes whisrs ask with `verbose_json` alone,
then with the plain request, and it keeps the form that worked.

The response's `segments` (`start`, `end`, `text`, optional `speaker`) and
`words` (`word`, `start`, `end`), in seconds, place the text in the
recording. Either one is enough, and whisrs cuts words into sentences:

```json
{
  "text": "Hello there. Shall we start?",
  "words": [
    { "word": "Hello", "start": 0.4, "end": 0.8 },
    { "word": "there.", "start": 0.8, "end": 1.3 },
    { "word": "Shall", "start": 1.9, "end": 2.1 },
    { "word": "we", "start": 2.1, "end": 2.2 },
    { "word": "start?", "start": 2.2, "end": 2.7 }
  ]
}
```

An answer with text only, or with one segment over 30 seconds and no words,
makes whisrs transcribe each phrase of the chunk alone. A phrase sent alone
loses the words around it, which costs accuracy, so a sidecar should return
`words` or `segments`.

## Implementations

## Recommended choices

- `moonshine`: fastest lightweight CPU/low-memory English sidecar.
- `parakeet`: recommended local GPU sidecar for high-quality multilingual
  dictation.
- `vibevoice`: heavier long-form transcription sidecar.

| Sidecar | Default model | Best for |
|---|---|---|
| `moonshine` | `UsefulSensors/moonshine-base` | Fast lightweight English dictation |
| `parakeet` | `nvidia/parakeet-tdt-0.6b-v3` | High-quality local GPU dictation |
| `vibevoice` | `microsoft/VibeVoice-ASR-HF` | Long-form local transcription experiments |

Each sidecar has its own README with installation and GPU notes, plus a
`config.toml.example` snippet for whisrs.

## AMD ROCm note

For AMD/ROCm, PyTorch still uses `cuda:0` device spelling, but NVIDIA CUDA
driver libraries are not present. NeMo's CUDA graph decoder expects
`libcuda.so.1`, so the Parakeet sidecar disables that decoder by default. Do
not pass `--use-cuda-graph-decoder` on ROCm.
