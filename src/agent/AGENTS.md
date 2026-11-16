# src/agent

Agent mode's shared part; the runtime is `src/daemon/agent.rs`. Each file's `//!` says how its
part works; these rules span files.

- Only session ids whisrs stored, from sessions it started, are passed to a harness. Nothing is
  matched by name.
- A new harness needs a `Harness` variant and its place in `Harness::ALL` (`mod.rs`), its driver
  module, its entry in `AgentRuntime::new`, output fixtures in `tests/fixtures/<harness>/` (for a
  process-per-turn one) and a section in `docs/agents.md`.
- A parser change adds the lines it handles to its harness's fixtures. A change to what a harness
  is passed updates its `docs/agents.md` section (`## Claude Code`, `## OpenCode`, `## Codex`,
  `## GitHub Copilot`).
