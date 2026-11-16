# src/cli/tui

The TUI behind `whisrs config` and `whisrs setup`. Each file's `//!` says what it does; these
rules span files.

- One path for keys and agents. `App::key` is `App::mode`, then `keys::action`, then `App::act`.
  Every agent input from taria lowers into `App::act`, or `App::key` for a raw key (`agent.rs`).
- A new key or screen needs its nodes in `tree.rs`, and a new node needs its lowering in
  `agent.rs`.
- config.toml is written only through the library (`config::write`, `config::vocabulary`), a
  profile through `config::profile::write_profile` after `save_profile` (`screens/config.rs`)
  edits it with toml_edit. Both keep comments and write only the keys that changed.
