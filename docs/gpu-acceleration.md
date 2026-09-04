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

## Upgrading over a distro package or tarball install

`cargo install` writes the new binaries to `~/.cargo/bin` and leaves the old
ones in `/usr/local/bin` or `/usr/bin` untouched. Check that your systemd unit
still points at the binary you just built (`systemctl --user show
whisrs.service -p ExecStart`) and point `ExecStart` at `~/.cargo/bin/whisrsd`
if it doesn't, otherwise you'll keep running the CPU build without noticing.
Don't just delete the old binary: `whisrs setup` writes an absolute
`ExecStart`, so removing what it points at stops the daemon starting rather
than moving it to the new build.
