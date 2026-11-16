# Nemotron diarization sidecar

NVIDIA's [Nemotron 3 Diarization](https://huggingface.co/nvidia/Nemotron-3-Diarization)
model, served by [NeMo-Speech.cpp](https://github.com/NVIDIA/NeMo-Speech.cpp)
(`nemo-speech serve`), tells apart the voices of a whisrs recording on your
machine, on the CPU or a Vulkan GPU, from a single prebuilt binary with no
Python. Its server speaks whisrs' diarization sidecar contract as it is, so
there is nothing to wrap.

- Runtime: NeMo-Speech.cpp 0.2.0, Apache-2.0 (the libraries it bundles carry
  their own licences, listed under `share/licenses/` in the tarball).
- Model: Nemotron 3 Diarization, q8_0 GGUF (107 MB), under OpenMDW-1.1. Not
  gated: no account or token needed.
- Up to 8 voices per recording.
- Trained mostly on English conversations.

It leaves some quiet or short speech without a voice; whisrs gives such words
the nearest voice within a second. Keep the `v3-offline` preset in
`diarization.yaml`: the default streaming preset gives the same result many
times slower.

## Install

1. The runtime. Pick the CPU build, or the Vulkan build for a GPU with a
   Vulkan driver (`vulkan-intel`, `vulkan-radeon` or the NVIDIA driver).

   ```sh
   mkdir -p ~/.local/share/nemo-speech && cd ~/.local/share/nemo-speech
   base=https://github.com/NVIDIA/NeMo-Speech.cpp/releases/download/v0.2.0
   curl -LO "$base/nemo-speech-0.2.0-linux-x86_64-cpu.tar.gz"      # 6.7 MB
   # or: curl -LO "$base/nemo-speech-0.2.0-linux-x86_64-vulkan.tar.gz"   # 16.6 MB
   sha256sum --check --ignore-missing <<'EOF'
   f396057150f1b774935c7414fd32ebc04195a795a34120d78d8c9ecefa1b7507  nemo-speech-0.2.0-linux-x86_64-cpu.tar.gz
   c8d0ed5b9ac5fb6d5c460015ba38ea217415c44edc1e6d49120646781009aee6  nemo-speech-0.2.0-linux-x86_64-vulkan.tar.gz
   EOF
   tar xzf nemo-speech-0.2.0-linux-x86_64-cpu.tar.gz
   ```

   The release's own `SHA256SUMS` lists the same checksums. `sha256sum` must
   print `OK` for the file you downloaded. On a GPU,
   `nemo-speech-0.2.0-linux-x86_64-vulkan/bin/nemo-speech doctor` should list
   it.

2. The model, pinned to a revision:

   ```sh
   cd ~/.local/share/nemo-speech
   curl -LO https://huggingface.co/nvidia/Nemotron-3-Diarization/resolve/f667ed73aee57d40cc39428eb768b4fd87a0a29e/Nemotron-3-Diarization.q8_0.gguf
   echo "08456d9e22cd9a323c0364d98375f3746d6e68507ebb705cd46438c534c7a3a1  Nemotron-3-Diarization.q8_0.gguf" | sha256sum --check
   ```

3. The server config: [`diarization.yaml`](diarization.yaml) with the model's
   full path. This writes it with your home filled in:

   ```sh
   mkdir -p ~/.config/nemo-speech
   cat > ~/.config/nemo-speech/diarization.yaml <<EOF
   diar:
     model_path: $HOME/.local/share/nemo-speech/Nemotron-3-Diarization.q8_0.gguf
     preset: v3-offline
   EOF
   ```

   `preset: v3-offline` is not the API's `mode=offline` (see the comments in
   `diarization.yaml`).

## Run

```sh
~/.local/share/nemo-speech/nemo-speech-0.2.0-linux-x86_64-cpu/bin/nemo-speech serve \
  --host 127.0.0.1 --port 8771 --no-ui \
  --config ~/.config/nemo-speech/diarization.yaml --device cpu
```

With the Vulkan build, use its directory and `--device vulkan:0`. Keep
`--host 127.0.0.1`: anyone who can reach the port can use the server.

To start it with your session instead, install the user unit
[`nemotron-diarization.service`](nemotron-diarization.service) (edit its
`ExecStart` for the Vulkan build). From a whisrs checkout at `~/whisrs`:

```sh
install -Dm644 ~/whisrs/contrib/diarization-sidecars/nemotron/nemotron-diarization.service \
  ~/.config/systemd/user/nemotron-diarization.service
systemctl --user daemon-reload
systemctl --user enable --now nemotron-diarization.service
```

The unit caps the sidecar at `MemoryMax=4500M` (with `MemorySwapMax=0`),
which fits three hours of speech. A longer track gets the sidecar alone
stopped by the kernel, not your session, and whisrs then writes that
transcript without speakers and says why. Lower the cap on a machine with
little memory.

## Check it

```sh
curl -s http://127.0.0.1:8771/health
ffmpeg -loglevel error -i some-talk.m4a -t 120 -ac 1 -ar 16000 /tmp/talk.wav
curl -s -F file=@/tmp/talk.wav http://127.0.0.1:8771/v1/audio/diarizations | head -c 300
```

The answer looks like
`{"segments":[{"end":11.508999744519592,"speaker":1,"start":10.720999755248428},...]}`:
seconds into the WAV, and the voices numbered 1, 2, ... in the order they
first speak.

## whisrs config

```toml
[recording]
diarization_url = "http://127.0.0.1:8771/v1/audio/diarizations"
```

`diarization = "auto"`, the default, uses the sidecar once
`diarization_url` is set (`"sidecar"` says so explicitly). whisrs sends each
separated track's speech once, as a 16 kHz mono WAV in the `file` field,
the same way the recipe's `curl` does, and turns the numbers into its own
speaker ids.

`num_speakers` (and `--num-speakers`) has no effect here: `nemo-speech serve`
takes no speaker count and ignores the field, so the model always finds the
voices itself. Leave it at 0.
