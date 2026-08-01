# whisrs — Version Roadmap

Incremental feature releases. Earlier entries (v0.1.2 → v0.1.5) are kept here as a
historical record; current development happens against the v0.1.x patch line.

---

## v0.1.2 — Multi-language & Transcription History ✓

- [x] **Multi-language support + auto-detection**: Language selection menu in setup with 18 common languages + auto-detect + custom ISO codes
- [x] **Transcription history** (`whisrs log [-n N] [--clear]`): JSONL storage at `~/.local/share/whisrs/history.jsonl`, newest-first, with timestamp/backend/language/duration
- [x] **whisper-rs update**: 0.15→0.16 (fixes bindgen compatibility)
- [x] **Feature gate**: `local-whisper` module properly cfg-gated for no-default-features builds

---

## v0.1.3 — Command Mode & Custom Vocabulary ✓

- [x] **Command mode** (`whisrs command`): Select text + hotkey → record voice instruction → LLM rewrites the selection. Toggleable (press again to stop early). (Originally pasted the result via simulated Ctrl+C/Ctrl+V; later releases inject it through the dictation pipeline instead: typed at the cursor by default, pasted when `[input] paste` is set, with terminals clearing the prompt line first.)
- [x] **Custom vocabulary**: `vocabulary = ["term1", "term2"]` in config — passed as prompt hint to Groq, OpenAI REST, and local whisper backends
- [x] **LLM integration**: `[llm]` config section with provider selection (OpenAI, Groq, OpenRouter, Google Gemini) and model menus with latest models. "Other" option for custom model names.

---

## v0.1.4 — System Tray & Configurable Hotkey ✓

- [x] **System tray indicator**: ksni StatusNotifierItem with colored circle icons — grey (idle), red (recording), yellow (transcribing). Works with waybar, KDE Plasma, GNOME (AppIndicator). Feature-gated behind `tray` feature (enabled by default).
- [x] **Configurable global hotkeys**: evdev-based passive keyboard listener. Config: `[hotkeys] toggle/cancel/command = "Super+Shift+D"`. Supports Super/Alt/Ctrl/Shift modifiers, left/right variants, letters, F-keys, named keys. No device grabbing — works alongside WM keybinds.
- [x] **State broadcasting**: Watch channel for real-time tray updates at all state transitions.

---

## v0.1.5 — Terminal-Aware Command Mode & Polish ✓

- [x] **Terminal-aware command mode**: Fixed LLM command mode in terminal emulators. Uses primary selection (`wl-paste --primary` / arboard) to read highlighted text without Ctrl+C (which sends SIGINT in terminals). Detects terminal windows via `WindowTracker::get_focused_window_class()`. (This release pasted the result via Ctrl+Shift+V after clearing the line with Ctrl+A → Ctrl+K; later releases route it through the dictation injection pipeline instead (typed by default, pasted with Ctrl+Shift+V when `[input] paste` is set) and still clear the prompt line first so the result replaces the original command.)
- [x] **Window class detection**: Added `get_focused_window_class()` to `WindowTracker` trait, implemented for Hyprland via `hyprctl activewindow -j` class field. Recognizes 18+ terminal emulators (Alacritty, Kitty, Foot, WezTerm, Ghostty, etc.).
- [x] **Primary selection support**: Added `get_primary_selection()` to `ClipboardHandler` trait — Wayland (`wl-paste --primary`) and X11 (arboard `LinuxClipboardKind::Primary`).
- [x] **Notification panic fix**: Resolved D-Bus `block_on` conflict with ksni tray runtime.
- [x] **State broadcasting**: Tray updates at all state transition points.
- [x] **curl install**: `curl -sSL https://y0sif.github.io/whisrs/install.sh | bash`

---

## v0.1.6 — Stability fixes ✓

- [x] **Tray icon on boot fix** (#1): Tray icon now appears reliably when the daemon starts at session login.
- [x] **Text injection on systems with brltty** (#2): Resolved a uinput ACL conflict that prevented text injection when brltty was active.
- [x] **Hotkeys / command mode on boot fix** (#3): Hotkey listener and command mode now retry/wait for input devices to be available at session start instead of failing silently.

---

## v0.1.7 — Deepgram + Unicode safety ✓

- [x] **Deepgram backend** (#8): New cloud transcription backend with both REST (Nova) and true WebSocket streaming variants. 60+ languages, $200 free credit on signup. Configured via `backend = "deepgram"` / `backend = "deepgram-streaming"`.
- [x] **Cyrillic / non-ASCII notification panic fix** (#7): Notifications no longer panic when transcribed text contains multi-byte UTF-8 characters; truncation is now codepoint-aware. Tests cover Arabic, CJK, Cyrillic, emoji, and mixed scripts.

---

## v0.1.8 — Overlay, AltGr typing, configurable injection ✓

- [x] **OSD overlay & GNOME Shell extension** (#11): Themed Wispr-Flow-inspired pill overlay (carbon / ember / cyan / custom) with envelope-follower bar visualizer, spring smoothing at 60fps, and a bundled GNOME Shell extension for GNOME Wayland (which lacks layer-shell). Configured via `overlay = true` and the `[overlay]` section. Toast notifications are auto-suppressed when the overlay is on.
- [x] **AltGr as a real modifier + dead-key synthesis** (#15): The keyboard injector now drives AltGr as a true modifier and synthesizes dead-key combinations, so layouts that depend on AltGr (e.g., proper diacritics in many European layouts) type correctly.
- [x] **Free-form transcription prompt** (#13): New `prompt = "..."` field in `[general]`, prepended to vocabulary and wired through `build_transcription_config` into Groq, OpenAI REST, OpenAI Realtime, and local whisper.cpp (Deepgram does not accept a prompt). Refactored `build_transcription_config` helper.
- [x] **Configurable uinput key delay** (#14): New `[input] key_delay_ms = 2` setting — raise it for TUIs (Node/Ink-based apps in raw mode like Claude Code) that drop characters during fast injection.
- [x] **Keyboard layout detection fix + tests** (#10): Fixed layout detection for non-US layouts; added comprehensive layout tests covering 20 layouts (French, Dvorak, Colemak, Spanish, and more).

---

## Read selection aloud (TTS) ✓

- [x] **Read-selection-aloud (TTS)**: New `whisrs speak` command (alias `read`, plus `[hotkeys] speak`) reads the current text selection aloud. Press speak again or `whisrs cancel` to stop playback. Configured via the `[tts]` section (`enabled` off by default). Backends: `groq`, `openai`, `deepgram`, and a local OpenAI-compatible `tts-sidecar` (Kokoro, Supertonic, etc.). `model`/`voice` are optional and default per backend; the TTS key falls back to the matching transcription key unless `[tts] api_key` is set. Adds `synthesizing` and `speaking` daemon states, reflected in the tray icon and the overlay (a sweep while synthesizing, distinct-colored audio-reactive bars while speaking, with a `[overlay.colors] speaking` override).

---

## Upcoming

- [ ] **Local Vosk backend**: CPU-only local speech recognition via `vosk` crate — true streaming, small models (~40 MB), works on Intel (no GPU required)
- [ ] **Parakeet backend** — requires NVIDIA GPU
- [ ] **Cross-compositor testing** — community/contributor effort
- [ ] **Anthropic LLM support** — Anthropic uses a different API format (`/v1/messages` instead of `/v1/chat/completions`). Need to add an adapter in `llm.rs` to support the Messages API. Users can access Anthropic models via OpenRouter in the meantime.
