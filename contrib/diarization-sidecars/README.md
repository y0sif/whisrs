# Diarization sidecars

A diarization sidecar is a local HTTP server that only finds who speaks
when. whisrs asks it about each track of a recording
(`[recording] diarization_url`) while any backend transcribes the words, then
gives every word the voice speaking at that moment.

| Recipe | What runs |
|---|---|
| [`nemotron`](nemotron/) (recommended) | `nemo-speech serve`, one prebuilt binary, on the CPU or a Vulkan GPU |
| [`sherpa-onnx`](sherpa-onnx/) | `uv run server.py`, a small Python environment, on the CPU |

Nemotron is the recommended one: a single binary with no Python, faster on a
CPU, and it mixes up voices least. sherpa-onnx gives more of the quiet speech
a voice, but runs several times slower.

## Contract

```text
POST {diarization_url}   multipart form: file = the track's speech as a 16 kHz mono WAV,
                         num_speakers = N (only when N > 0)
200 {"segments": [{"start": 1.20, "end": 4.85, "speaker": "SPEAKER_00"}]}
```

`start` and `end` are seconds into the WAV; `speaker` is a string or an
integer that only has to be consistent within one request. See
[Diarization sidecar](../../docs/recording.md#diarization-sidecar) for the
timeouts, retries and caching.
