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
