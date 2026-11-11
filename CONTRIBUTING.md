# Contributing

Pull requests and issues are welcome. This project drives the
[AAEmu Rust server](https://github.com/AAEmu/AAEmu-1.2-Rust) with headless ArcheAge 1.2 (r208022)
player bots; it ships no game assets.

## Build

You need a stable Rust toolchain (1.88 or newer, see `rust-version` in `Cargo.toml`).

```sh
cargo build --workspace --all-targets
```

A C compiler is needed for the bundled SQLite (`rusqlite`). To keep build output on another drive,
put a `target-dir` into `.cargo/config.toml` (git-ignored).

## Before you open a pull request

The same three checks run in CI on Linux and Windows:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Add or extend tests with a behaviour change. The tests run without game data and without a server;
no test contacts an external host (the LLM tests talk to a mock endpoint on 127.0.0.1).

### Tests with the real game data

Tests that need real data are `#[ignore]`d and gated by environment variables. Run them locally when
you touch the code they cover:

| Variable | Points to |
| --- | --- |
| `AA_COMPACT` | the AAEmu server's `compact.sqlite3` (`<AAEmu>/rust/crates/aaemu-game/app/Data/compact.sqlite3`) |
| `AA_SERVER_DATA` | the AAEmu server's `app/Data` directory (worlds, spawns, `CharTemplates.json`) |
| `AA_BAKED` | the output directory of `playerbots bake` |
| `AA_PAK` | the client's `game_pak` (the `nav_real` and `heights_real` tests in `crates/bake`) |

```sh
AA_COMPACT=<...>/compact.sqlite3 AA_SERVER_DATA=<...>/Data AA_BAKED=<data dir> \
    cargo test --workspace --release -- --ignored
```

(PowerShell: `$env:AA_COMPACT = "..."` first.) `AA_TEST_TMP` moves the scratch directories of the
unit tests to another volume; the default is the operating system's temp dir.

## Commits and branches

* Branch from `main`; keep a pull request to one topic.
* Use [Conventional Commits](https://www.conventionalcommits.org/): `feat(playerbots): ...`,
  `fix(proto): ...`, `docs: ...`, `test: ...`, `refactor: ...`, `ci: ...`.
* Match the style of the surrounding code; `cargo fmt` and clippy (no warnings) decide the rest.
  Wire formats must match the 1.2 client byte for byte, and S-to-C/C-to-S packet code needs
  byte-exact tests.
* Do not commit client files, `compact.sqlite3`, baked data, rosters, passwords or API keys.

## License

By contributing you agree that your work is licensed under the GPL-3.0-or-later, like the rest of
the project (see `LICENSE`).
