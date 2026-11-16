# sherpa-onnx diarization sidecar

[`server.py`](server.py) tells apart the voices of a whisrs recording with
[sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx), on the CPU, in a
small Python environment that `uv` builds on first run. Two ONNX models do
the work: pyannote segmentation-3.0 finds the speech and where the speaker
changes, and 3D-Speaker's ERes2Net (trained on English VoxCeleb) tells the
voices apart. [The sidecars index](../README.md) compares it with the
Nemotron recipe.

- sherpa-onnx: Apache-2.0.
- pyannote segmentation-3.0, converted to ONNX by sherpa-onnx: MIT (the
  `LICENSE` in its tarball).
- 3D-Speaker ERes2Net, English VoxCeleb (`3dspeaker_speech_eres2net_sv_en_voxceleb_16k.onnx`):
  Apache-2.0.
- No account, token or GPU needed. Avoid the `sherpa-onnx-reverb-diarization-v1`
  model that sits next to these downloads: its licence is non-commercial.

The environment takes about 100 MB (sherpa-onnx bundles onnxruntime) and the
two models 32 MB. The Python packages are pinned in `server.py`; another
sherpa-onnx version may need a new `--threshold`.

Reading an upload takes about 3 times its size in memory on top of the
diarization, about 1 GB more for a 3-hour track. `--max-upload-mb` (default
400, a little over 3.5 hours of speech) refuses anything larger. It diarizes
one recording at a time, and whisrs waits and tries again while it is busy.

## Tuning

Without a speaker count, the voices are grouped by distance, then small groups
are merged:

- `--threshold` (default 1.1, set for this embedding model on meetings): lower
  finds more voices, higher merges different people.
- `--min-share` (default 0.05): a voice with less of the speech than this is
  merged into the one it sounds closest to.
- `--keep-seconds` (default 10) and `--min-similarity` (default 0.6): a small
  voice with that much speech that sounds unlike every other stays apart.

With `num_speakers` (whisrs' `num_speakers` or `--num-speakers N`),
sherpa-onnx groups the speech into that many voices (one can come back empty
on a short clip) and nothing is merged. The threshold and merge usually do
better, so leave it at 0.

## Install

The models, with their checksums:

```sh
mkdir -p ~/.local/share/sherpa-onnx && cd ~/.local/share/sherpa-onnx
rel=https://github.com/k2-fsa/sherpa-onnx/releases/download
curl -LO "$rel/speaker-segmentation-models/sherpa-onnx-pyannote-segmentation-3-0.tar.bz2"
curl -LO "$rel/speaker-recongition-models/3dspeaker_speech_eres2net_sv_en_voxceleb_16k.onnx"
sha256sum --check <<'EOF'
24615ee884c897d9d2ba09bb4d30da6bb1b15e685065962db5b02e76e4996488  sherpa-onnx-pyannote-segmentation-3-0.tar.bz2
c59158379255ad66e161679cca6af8d52d51e389e3224ab7d7a7baae295c2db5  3dspeaker_speech_eres2net_sv_en_voxceleb_16k.onnx
EOF
tar xjf sherpa-onnx-pyannote-segmentation-3-0.tar.bz2
```

(`recongition` is how the sherpa-onnx release is spelled.) Then
[uv](https://docs.astral.sh/uv/), which reads the dependencies from the top of
`server.py`.

## Run

`server.py` can live anywhere; here it is run from a whisrs checkout at
`~/whisrs`:

```sh
uv run ~/whisrs/contrib/diarization-sidecars/sherpa-onnx/server.py \
  --seg ~/.local/share/sherpa-onnx/sherpa-onnx-pyannote-segmentation-3-0/model.onnx \
  --emb ~/.local/share/sherpa-onnx/3dspeaker_speech_eres2net_sv_en_voxceleb_16k.onnx
```

It listens on `http://127.0.0.1:8770/diarize` (`--host`, `--port`) and uses 4
CPU threads (`--threads`). Keep it on 127.0.0.1: anyone who can reach the
port can use it. It answers one recording at a time and logs one line per
request: the audio length, the time taken and the voices found.

## Check it

```sh
curl -s http://127.0.0.1:8770/health
ffmpeg -loglevel error -i some-talk.m4a -t 120 -ac 1 -ar 16000 /tmp/talk.wav
curl -s -F file=@/tmp/talk.wav http://127.0.0.1:8770/diarize | head -c 300
```

Or without the server: the same command with `--input /tmp/talk.wav` prints
the same answer, and on stderr how many voices it found before and after the
merge.

## whisrs config

```toml
[recording]
diarization_url = "http://127.0.0.1:8770/diarize"
```

`diarization = "auto"`, the default, uses the sidecar once `diarization_url`
is set.
