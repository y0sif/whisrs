# GPU Acceleration (local whisper.cpp)

The default build, and every prebuilt tarball that ships whisper.cpp at all,
runs it on the CPU. If you use the `local-whisper` backend, building with a GPU
feature moves the model onto your GPU and cuts dictation latency from seconds to
near-instant:

```bash
cargo install whisrs --features vulkan
```

| Feature | Backend | Hardware |
|---|---|---|
| `vulkan` | Vulkan | AMD, Intel, NVIDIA (cross-vendor; the safe default) |
| `cuda` | CUDA | NVIDIA, needs the CUDA toolkit |
| `hipblas` | ROCm/HIP | AMD, needs ROCm |

These are compile-time features: the GPU backend has to be linked in, so there
is no runtime switch. Each one implies `local-whisper`; the cloud backends are
unaffected, and CPU stays the default.

## Build-time system dependencies

On top of the usual `alsa-lib`, `libxkbcommon`, `clang`, `cmake`. For `vulkan`:

```bash
# Arch Linux
sudo pacman -S vulkan-headers vulkan-icd-loader shaderc

# Debian/Ubuntu
sudo apt install libvulkan-dev glslc

# Fedora
sudo dnf install vulkan-headers vulkan-loader-devel glslc
```

Your GPU driver package alone is **not** enough. The driver ships the runtime,
not the development headers or the shader compiler, so a machine that runs
Vulkan games fine will still fail the build with:

```
Could NOT find Vulkan (missing: Vulkan_INCLUDE_DIR)
```

Install the packages above and rebuild. `cuda` and `hipblas` likewise need
their full toolkits (`cuda` / `rocm-hip-sdk`), not just the driver.

## Verify it worked

The binary should link against the Vulkan loader:

```bash
ldd ~/.cargo/bin/whisrsd | grep vulkan
```

No output means you got a CPU build. Then start the daemon in the foreground
and watch whisper.cpp report the device it picked up:

```bash
RUST_LOG=debug whisrsd
```

A working Vulkan build names your GPU at load time (for example
`ggml_vulkan: Found 1 Vulkan devices: Radeon RX 9070 XT (RADV GFX1201)`) and
loads the model onto it.

## CUDA tuning

Two optional environment variables, set when you build, tune a CUDA build.
Neither changes what whisrs does. A third tip, flash attention, is a config
setting.

**Target your GPU's architecture.** By default ggml builds for `native`, which
only works if an NVIDIA card is visible to `nvcc` during the build (not the case
in a container, CI or a clean chroot). Naming the architecture explicitly fixes
that and compiles code for just your card. Look up its compute capability and
drop the dot:

```bash
nvidia-smi --query-gpu=compute_cap --format=csv,noheader   # e.g. 8.9 -> 89

CMAKE_CUDA_ARCHITECTURES=89-real cargo install whisrs --features cuda
```

The `-real` suffix builds machine code for that exact GPU only, with no
fallback code to compile at first launch. Separate several cards with a
semicolon (`"86-real;89-real"`). Common values: `75` Turing (RTX 20), `80` A100,
`86` Ampere (RTX 30), `89` Ada (RTX 40), `120a` Blackwell (RTX 50). CUDA 13
dropped Maxwell, Pascal and Volta (`50`, `60`, `61`, `70`), so those cards need
CUDA 12.

**Skip device code compression.** ggml compresses the GPU code by default
(`size`) and the daemon has to unpack it every time the model loads. Turning it
off trades a noticeably larger binary for a faster model load. Pairing it with
the architecture setting above keeps the size increase down, since only your
card's code is stored:

```bash
GGML_CUDA_COMPRESSION_MODE=none CMAKE_CUDA_ARCHITECTURES=89-real \
  cargo install whisrs --features cuda
```

Cargo does not track either variable, so changing one does not rebuild anything
on its own. Add `--force` to `cargo install whisrs` to rebuild with the new
values; in a source checkout, run `cargo clean -p whisper-rs-sys` first.

**Turn on flash attention.** This is a runtime setting, not a build option.
Flash attention is a faster way for the model to compute attention that can cut
transcription time on a CUDA build, most noticeably on long recordings. It is
off by default. Enable it in `~/.config/whisrs/config.toml`, then restart the
daemon:

```toml
[local-whisper]
flash_attn = true
```

When it took effect, whisper.cpp's init lines in the daemon log show
`flash attn = 1`. Leave it off on Vulkan and CPU builds: it was slower there,
and on an Intel iGPU under Vulkan it also hung the GPU.

## Upgrading over a distro package or tarball install

`cargo install` writes the new binaries to `~/.cargo/bin` and leaves the old
ones in `/usr/local/bin` or `/usr/bin` untouched. Check that your systemd unit
still points at the binary you just built (`systemctl --user show
whisrs.service -p ExecStart`) and point `ExecStart` at `~/.cargo/bin/whisrsd`
if it doesn't, otherwise you'll keep running the CPU build without noticing.
Don't just delete the old binary: `whisrs setup` writes an absolute
`ExecStart`, so removing what it points at stops the daemon starting rather
than moving it to the new build.
