# AGENTS.md

Guide for coding agents (and people) working in this repository. The README says what the plugin does; this file
says how to work on it.

## What this is

`pumpkin-link` is a plugin for the [Pumpkin](https://github.com/Pumpkin-MC/Pumpkin) Minecraft server. It makes the
server the host or the guest of the rewrite-bridge passthrough link: a local protocol on `127.0.0.1` (TCP control
lines and UDP datagrams) defined in `docs/CONTRACT.md` of
[rewrite-bridge](https://github.com/patodois/rewrite-bridge), protocol v1 revision 8. The protocol itself is
implemented by `rewrite_bridge::link::Link`; this repository is the bridge between that link and the server.

## Toolchain

- Rust, edition 2024, minimum version 1.97 (`rust-version` in `Cargo.toml`; CI checks it).
- The plugin is a WebAssembly component: `rustup target add wasm32-wasip2`.
- `rewrite-bridge` is a git dependency pinned to one commit (revision 8 is not in a release yet). Change the pin
  only on purpose, and update `Cargo.lock` with it; `scripts/live-test.sh` builds its peers from that same commit.
- Python 3.9+ for the public-tree gate in `tools/` (standard library only).

## Commands (what CI runs, `.github/workflows/ci.yml`)

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --no-default-features -- -D warnings      # native: everything but the glue
cargo clippy --locked --target wasm32-wasip2 --lib -- -D warnings             # the plugin glue
cargo test --locked --no-default-features                                     # unit and loopback end-to-end tests
cargo build --locked --release --target wasm32-wasip2                         # the plugin component
python3 tools/test_public_tree.py                                             # the gate's own tests
python3 tools/check_public_tree.py                                            # the gate on the tracked files
python3 tools/check_public_tree.py --history                                  # and on every commit
```

`cargo clippy --all-targets` and `cargo test` with the default features also work natively: the glue
(`src/plugin.rs`) is only compiled for `wasm32`. CI also runs some test files on `wasm32-wasip2` under wasmtime and
`scripts/live-test.sh --server sandbox`. `scripts/live-test.sh --server pumpkin` builds a real Pumpkin server and
needs a server jar you already have; it is never run by CI.

## Code map

| Path | What it holds |
|---|---|
| `src/bridge.rs` | the session: the link, the players published, the collision scan driven per tick, the guest's entities handed to a sink, `/link status` text (peer text is cleaned there) |
| `src/collision.rs` | collision snapshots: a scan one layer per tick, faces made as layers are read, cut to the contract's limit |
| `src/config.rs` | the JSON config, its parser and the rule for a missing or broken file |
| `src/displays.rs` | the guest's entities as tagged block displays, and the sweep that keeps none past its session |
| `src/mapping.rs` | Minecraft coordinates and angles to link space and back |
| `src/plugin.rs` | the glue to `pumpkin-plugin-api` (WebAssembly only, no tests: it needs Pumpkin to run) |
| `examples/linkpeer.rs` | a test peer (host or guest) used by the live script |
| `examples/sandbox_server.rs` | the bridge driven like a server, as a WASI command, for the sandbox live test |
| `tools/sandbox-host/` | a wasmtime host with Pumpkin's socket and folder rules (a separate crate) |
| `tools/check_public_tree.py` | the public-tree gate; `tools/test_public_tree.py` tests it |
| `tests/` | protocol tests with hand-written raw-socket peers, end-to-end tests against real link peers, memory bound of the collision scan |

## Rules of the project

- Loopback only. Nothing binds or connects anywhere but `127.0.0.1`.
- Nothing from the peer is trusted: every text it sends is cleaned and cut before a player, the console or a log
  sees it (`bridge::clean_peer_text`); the pixel file a peer names is never opened.
- The token never appears in a status line, a log, an error or a test's output.
- A config file that exists is never overwritten; a broken one stops the link from starting.
- The server's tick never waits: `try_lock` only, and every per-tick piece of work is bounded.
- New behaviour comes with a test that fails without it. The glue in `src/plugin.rs` stays thin so that the logic
  can be tested natively.
- The README claims only what a test, a script or a kept record shows.
- No code copied from Pumpkin, from Mojang or from any game; no decompiled code.

## Public files

Everything tracked here is public. Never commit game files or game code (jars, worlds, data extracted from the
game), build output, logs, credentials or tokens, personal paths (home directories) or personal names. The gate
(`tools/check_public_tree.py`) rejects the file kinds it knows; run it before every push, and install it as a
pre-push hook as its header says.

## Licence and AI use

MIT OR Apache-2.0, at your option (`LICENSE-MIT`, `LICENSE-APACHE`). Pumpkin is GPL-3.0 and is only built as a test
harness in a temporary folder by `scripts/live-test.sh`; nothing of it is in this repository.

The code was written with AI coding agents (Anthropic's Claude and OpenAI's GPT models) under the maintainer's direction.
