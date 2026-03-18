# whisrs — Version Roadmap (0.1.2 → 0.1.5)

Incremental feature releases building toward v1.5.

---

## v0.1.2 — Multi-language & Transcription History

- [ ] **Multi-language support + auto-detection**: Language selection in config (`[transcription] language = "fr"`), auto-detect spoken language when not specified
- [ ] **Transcription history** (`whisrs log`): Log recent transcriptions to disk, viewable from CLI with timestamps

---

## v0.1.3 — Command Mode & Custom Vocabulary

- [ ] **Command mode**: Select text + hotkey + speak instruction → LLM rewrites selected text in place (e.g., "make this more formal", "turn into bullet points")
- [ ] **Custom vocabulary / personal dictionary**: User-defined terms, names, and acronyms that improve transcription accuracy

---

## v0.1.4 — System Tray & Configurable Hotkey

- [ ] **System tray indicator**: Persistent icon showing whisrs state (idle / recording / transcribing) via `ksni` or `tray-icon` crate
- [ ] **Configurable hotkey**: Set trigger key from CLI or config (`[hotkey] toggle = "Super+H"`) instead of relying on WM keybinds

---

## v0.1.5 — Local Vosk Backend

- [ ] **Vosk backend**: CPU-only local speech recognition via `vosk` crate — true streaming, small models (~40 MB), works on Intel (no GPU required)
- [ ] Final polish pass for v1.5 readiness

---

## Deferred

- **Parakeet backend** — requires NVIDIA GPU
- **Static binary releases / install script** — post-feature distribution work
- **Cross-compositor testing** — community/contributor effort
- **Non-QWERTY layout testing** — later
- **Demo GIF** — later
