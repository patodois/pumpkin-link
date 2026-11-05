# CLOUD-REPORT: pumpkin-link

Branch `claude/pumpkin-link-plugin-rv18cr`, then `fix/review`. Everything below was run in the cloud session unless
it says otherwise; commands are given so they can be run again.

## Update, 2026-11-04: run against a real Pumpkin on a Mac

* Pumpkin was built at the pinned commit with a server jar already on that machine, and
  `scripts/live-test.sh --server pumpkin` ran for the first time: Pumpkin loads the plugin with its four
  permissions; host, guest, the three token cases, a guest entity shown as a block display and removed, `/link status`
  and `/link stop` all pass, with every socket on 127.0.0.1. No check fails; 3 are NEEDS-CLIENT.
* With no player online, collision and player publishing did not run on a real server. Those paths have only run in
  the sandbox. `WorldSource`, in particular, has not read a real block.
* The sections below that say the plugin was never run in Pumpkin and that Pumpkin could not be built describe the
  cloud session. They stay as the record of that session; this update replaces their conclusions.
* Fixed since: the raw pointer in `WorldSource` is now an `Rc<RefCell<..>>`; `/link status` of a guest names the
  host's port; the guest's entities are logged; the live test writes its own server config (127.0.0.1, offline, no
  Bedrock, no telemetry).
* Still open: the client test below.

## Update, port onto `rewrite_bridge::link::Link`

* The plugin no longer has its own session. `src/session.rs` (1126 lines) and `src/endpoint.rs` (201 lines) are
  gone; `src/bridge.rs` drives `rewrite_bridge::link::Link` for both roles, and the token and endpoint file are
  `LinkConfig::endpoint_file`. `getrandom` is no longer a dependency of the plugin.
* Run after the port: `cargo test --no-default-features` (36 unit, 8 bridge, 2 bridge-tick, 2 pixel-file and 28 protocol tests), the
  protocol and pixel-file tests again on `wasm32-wasip2` under wasmtime 50.0.1, clippy and rustfmt as in CI, and
  `scripts/live-test.sh --server sandbox`: ALL PASSED (21 rows: 20 checks and the conformance info line), the plugin built
  against the new crate and the peers against rewrite-bridge 0.3.0 from crates.io. `--server pumpkin` was not run.
* A peer that stops reading used to be able to hold the server's tick: under wasmtime, closing a socket waits for the
  bytes the runtime still has for it, which for a peer that neither reads nor exits took 5 s on macOS and never ended
  on Linux. `Link` now sets such a connection aside instead of closing it (see the README, "The link"). The plugin's
  own control traffic is a few hundred bytes per player, so it cannot fill a socket by itself; the path is reached
  only by a peer that stops reading while a lot is queued for it.
* This needs a `rewrite-bridge` that runs on `wasm32-wasip2`, which 0.3.0 does not. The sections below that describe
  `src/session.rs` and `src/endpoint.rs`, and the crate failing on this target, are the record of the first
  version; the rows of the path proof still say what the sandbox allows.

## Summary

* A Pumpkin plugin (`pumpkin_link.wasm`, 0.88 MB, a component with the `pumpkin:plugin@0.1.0` ABI) that is the
  **host** or the **guest** of the rewrite-bridge link (contract revision 6, with the token of 6.2): the server's
  players are entities with poses, a host sends collision snapshots of the blocks around a player and shows the
  guest's entities as block displays, `/link status|host|guest|stop|focus`, a JSON config in the data folder,
  the token with its endpoint file.
* **Not run in a Pumpkin server.** Pumpkin cannot be built here (see "Pumpkin could not be built"). Everything
  except the thin Pumpkin glue (`src/plugin.rs`) was run: natively, and as a WebAssembly component under
  wasmtime-wasi with the socket and folder rules Pumpkin gives a plugin, against the real `rewrite-bridge`
  peers. The glue compiles, passes clippy for `wasm32-wasip2`, and the built plugin component instantiates and
  reports its metadata (below), but its calls into the server (players, blocks, entities, commands) are
  untested.
* Biggest finding of the first version: `rewrite_bridge::link::Link` **compiled for `wasm32-wasip2` but could not
  run there** (details below), so `src/session.rs` implemented both roles from CONTRACT.md on top of the crate's
  wire codec. The crate has been fixed and the plugin now uses it (see the update at the top).

## The path proof (task step 1)

What the sandbox can do. "works" rows were observed by running the real code under the sandbox host (wasmtime-wasi 50,
Pumpkin's socket rules); rows marked *inferred* come from the target's standard library (no such API, or it is documented
as unsupported) and were not run. A probe plugin built against the Pumpkin API early on was never loaded anywhere.

| Needed by the protocol | In the `wasm32-wasip2` sandbox |
|---|---|
| TCP listen on `127.0.0.1`, accept, non-blocking | works (`network.tcp`, `network.loopback`) |
| TCP connect to loopback | works, but wasmtime binds the wildcard address first; Pumpkin's policy lets that one precursor through (`SocketPolicy::allows`), so it is allowed with `network.tcp` |
| UDP bind, send, receive, non-blocking | works (`network.udp`) |
| Monotonic and wall clocks | work |
| Random bytes (token, session) | work, from WASI `random` (`getrandom` crate); `/dev/urandom` and `libc::getrandom` do not exist |
| Files in the data folder: exclusive create, write, rename, read | work (`fs.write.data`, folder preopened as `data`) |
| File mode `0600` at creation, `chmod` | **not available** (no mode bits in WASI; *inferred*) |
| `SO_RCVBUF` (contract 2: ask for 4 MiB) | not available through `std`; **available through the WASI interface**: `set-receive-buffer-size` took the default 786896 to 4194304 under wasmtime 50 (the crate does this now) |
| Threads | **not available** on this target (*inferred*; the crate used one for a guest's connect, and no longer does there) |
| `std::process::id()` | **panics** ("no pids on this platform"), observed |
| Memory-mapped pixel file | not attempted (see "left out") |

Consequences, as found in the first version (the crate now handles them, see the update at the top):

* `rewrite_bridge::link::Link` (feature `link`) builds for the target. Run in the sandbox it **panics in the
  host path** (`random_session()` calls `std::process::id()` when a guest says hello) and its **guest connects
  from a spawned thread**, which this target cannot do (read from its source and inferred; the crate's guest was never run in the sandbox, both roles were replaced as soon as the host path panicked). As the task says, nothing was forked or vendored: `src/session.rs`
  implements host and guest from CONTRACT.md (section numbers in the comments) and uses only the crate's public
  codec and data types (`encode_datagram`, `decode_datagram`, `Control::parse/to_line`, `Event`, `PeerInfo`,
  `Stats`, `token_equal`, `negotiate`, `Endpoint::read`).
* `generate_token` and `Endpoint::write` of the crate need OS random bytes through `libc`/`/dev/urandom` and
  fail here, so `src/endpoint.rs` does the same two jobs with WASI random and `std::fs`.
* The endpoint file cannot be created `0600`; the plugin cannot read its mode back either. The README tells the
  operator to start the server with `umask 077`. This is a deviation from 6.2 ("`open(O_CREAT|O_EXCL, 0600)`,
  never a `chmod` after") that the sandbox forces. `Link` writes it the same way now: `0600` from the start on Unix, the umask's mode on WASI.
* A plugin sees only its own data folder, so `endpoint_file` is a file name in it (the recommended
  `$XDG_RUNTIME_DIR/rewrite-link/` is unreachable). A guest-role server reads an endpoint file that its host wrote
  (or was copied) into the data folder.
* A guest-role connect is a blocking `connect` to loopback (instant on loopback; no timeout API exists here).
* The sandbox gives no timers: the link is driven by `schedule_repeating_task(1, 1, ...)`, one poll per server
  tick (50 ms at 20 TPS); heartbeats are sent from that loop, as the contract wants.

## Pumpkin could not be built here

`cargo build -p pumpkin` at the pinned release (tag `0.2.1+26.3-26.51`, commit
`224f9f8144ca53a8d0800acb87607d714196e2a6`) stops in `crates/pumpkin-data/build.rs`, which needs the game's
vanilla datapack: it tries to fetch the server jar from `piston-meta.mojang.com` (that request fails from this
environment) unless `PUMPKIN_MINECRAFT_SERVER_JAR` points at a jar. The rules forbid downloading game data, so
I did not supply one; I also tried an empty `assets/datapack/data/minecraft` folder in the throw-away clone
(38 compile errors in `pumpkin-data`), which is not an option. So there is no live Pumpkin result.
`scripts/live-test.sh --server pumpkin` is written for the pinned commit and refuses to run without
`PUMPKIN_MINECRAFT_SERVER_JAR` (checked: it exits 2 with a message). It has **never been executed past that
check**; expect to fix small things in it (the console `stop` and `link status` input, the headless start,
`config` the server wants) the first time it runs.

Other facts found while pinning:

* The crate `pumpkin-plugin-api 0.2.1+26.3-26.51` bundles the **v0.1** WIT (`package pumpkin:plugin@0.1.0`):
  "0.2.x" is Pumpkin's release number, not the WIT version. The host identifies a plugin by its metadata export;
  `0.1.*` goes to the v0.1 engine, so the server needs **no** `plugin-v0_2` feature. (Main moved on to a v0.2
  async ABI; the published crate does not use it.) The pinned tag, not main, is what matches the crate.
* Building Pumpkin from a clone pulls a `rust-toolchain.toml` with extra components; with rustup that failed
  here and broke the local toolchain until reinstalled. The script overwrites that file in its throw-away clone
  with plain `stable`.

## What was verified, and how

All in `/home/user/pumpkin-link`.

| Check | Command | Result |
|---|---|---|
| Unit tests: mapping with sign checks (13), collision (13), config (5), endpoint file and token (6), pose building (5) | `cargo test --no-default-features` | 42 passed (the 6 of the endpoint file left with `src/endpoint.rs`) |
| Bridge against real `rewrite-bridge` peers, in process over loopback (host with and without token, wrong token, `busy`, guest role, guest entities reaching the sink, taken port writes no file) | same | 7 passed (8 after the port) |
| Session rules (now `rewrite_bridge::link::Link`) against hand-written raw-socket peers (first line, `bye`, token-first and only the word `unauthorized`, `busy`, second hello, 65536-byte line, 1024-entity limit, stale poses, foreign session/address, collision assembly rules of 9.5, heartbeats, watchdog, EOF, focus request, guest role) | same | 20 passed (28 after the port, with the tick-rate ones) |
| MSRV 1.97 | `cargo +1.97.0 build --release --target wasm32-wasip2 && cargo +1.97.0 test --no-default-features` | passed |
| Formatting | `cargo fmt --all -- --check` (and the same for `tools/sandbox-host`) | clean |
| Clippy | `cargo clippy --all-targets --no-default-features -- -D warnings`; `cargo clippy --target wasm32-wasip2 --lib -- -D warnings`; same for the sandbox host | clean |
| Public-tree gate | `python3 tools/check_public_tree.py` and `--range origin/main..HEAD` | passed |
| The real plugin component loads and declares | `tools/sandbox-host/target/release/sandbox-host --inspect target/wasm32-wasip2/release/pumpkin_link.wasm` | `init-plugin` + `get-metadata` run with Pumpkin's imports stubbed as traps: name `pumpkin-link`, permissions `network.tcp, network.udp, network.loopback, fs.write.data` |
| End to end in the WASI sandbox with Pumpkin's socket rules | `scripts/live-test.sh --server sandbox` | see the table below |

### `scripts/live-test.sh --server sandbox`

The sandbox host (`tools/sandbox-host`, wasmtime-wasi 50 like the pinned Pumpkin) runs
`examples/sandbox_server.rs`, a command that links the plugin's own `bridge`/`session`/`collision`/`config`/
`endpoint` modules and feeds them two fake walking players over a flat fake floor, with only the data folder
preopened and TCP/UDP allowed to loopback. The peers are native: `examples/linkpeer.rs` and the
`rewrite-bridge` 0.3.0 examples `fake_guest`, `fake_host`, `interop_peer`, `conformance`.

```
================ results (sandbox) ================
plugin: component loads, metadata and permissions    PASS  tcp, udp, loopback, fs.write.data
info: rewrite-bridge conformance example ran         PASS  exit 1 (1 is its normal answer on the toy provider)
host/plain: server listens                           PASS  127.0.0.1:47811
host/plain: linkpeer sees players, poses, collision  PASS  PEER OK 2 players spawned and posed, 1 collision snapshots, 60 own pos
host/plain: rewrite-bridge fake_guest connects       PASS  fake_guest connected
host/plain: guest entity shown by the server         PASS  found: GUEST spawn entity=90 provider=linkpeer
host/plain: guest entity despawned or cleared        PASS  found: GUEST \(despawn\|clear\)
host/token: server listens                           PASS  127.0.0.1:47811
host/token: linkpeer with the endpoint file          PASS  PEER OK 2 players spawned and posed, 1 collision snapshots, 60 own pos
host/token: linkpeer without a token is refused      PASS  PEER OK unauthorized: peer said bye: unauthorized
host/token: linkpeer with a wrong token is refused   PASS  PEER OK unauthorized: peer said bye: unauthorized
host/token: interop_peer --wrong-token               PASS  RBL-INTEROP-RESULT {"cameras":0,"collision_triangles":0,"connected":fa
host/token: the token appears in no log              PASS  not in the server or peer logs
host/token: guest entity shown by the server         PASS  found: GUEST spawn entity=90 provider=linkpeer
host/token: guest entity despawned or cleared        PASS  found: GUEST \(despawn\|clear\)
host/token: status counts refusals                   PASS  refused as unauthorized
guest/plain: linkpeer host sees players and poses    PASS  PEER OK 2 players spawned and posed, 0 collision snapshots, 0 own pose
guest/plain: rewrite-bridge fake_host sees the server PASS  connected: guest
guest/plain: fake_host saw the players spawn         PASS  guest spawned entity
guest/token: server reads the endpoint file and connects PASS  PEER OK 2 players spawned and posed, 0 collision snapshots, 0 own pose
guest/refused: interop_peer host --expect-unauthorized PASS  RBL-INTEROP-RESULT {"cameras":0,"collision_triangles":0,"connected":fa
ALL PASSED   (21 + 1 checks; exit 0; run on this machine, 2026-11-01)
```

Notes on that table:

* The `conformance` example of rewrite-bridge is a **checker for asset providers** (a toy cube provider; it
  exits 1 by design). It does not touch the link, so it says nothing about this plugin; it is run because it
  was asked for, and its exit code is only checked to be that normal 1.
* `fake_guest` / `fake_host` have no token option, so they run in the no-token cases; the token cases use
  `linkpeer` and `interop_peer --wrong-token` / `--expect-unauthorized` (the latter two print their own
  `RBL-INTEROP-RESULT` line, which the script treats by exit code).
* "The token appears in no log": the token read from the endpoint file is searched for in the server and
  peer logs.
* This runs **the plugin's code**, not the plugin component, in the sandbox (`sandbox_server` is a command, the
  plugin is a library component with Pumpkin's imports). The difference is exactly `src/plugin.rs`.

## What was built

| File | What |
|---|---|
| `src/bridge.rs` | players to entities and poses (feet position, view rotation, velocity from real displacement, yaw rate, TELEPORT), collision driver, guest-entity sink, `/link status` text, the link's start and stop |
| `src/mapping.rs` | CONTRACT 4 mapping with flip/scale/offset (mirror reverses winding) |
| `src/collision.rs` | block boxes to triangles, face culling, cut to 925,696 nearest, fingerprint |
| `src/config.rs` | the JSON config (keys of minecraft-link that apply) |
| `src/plugin.rs` | the Pumpkin glue (wasm only): players, blocks, block-display entities, `/link`, permission, scheduler |
| `examples/`, `tools/sandbox-host/`, `scripts/live-test.sh`, `.github/workflows/ci.yml` | tests and CI |

## What is left out, and why

* `Camera`, `Frame`, the pixel file, depth and reprojection (CONTRACT 9.3, 9.6, 9.7): a server has no renderer or
  camera. A received `Camera`/`frame_channel`/`Frame` is counted and ignored; the plugin never opens a pixel file.
* Input focus for a server: a guest's request is ignored (allowed by section 8); `/link focus` exists for hosts.
* Interpolation of guest entities (`interp_delay_ms`, `show_frames` of minecraft-link): they are moved to their
  latest pose. Scale and roll of a guest pose are not applied (the block display has the plugin's fixed
  centring transform, yaw and pitch only).
* Per-player collision: one snapshot, centred on `collision_player` or the first player online.
* Mapping of block shapes beyond the boxes the server reports: nothing is invented; a block with no collision
  box adds nothing.
* `SO_RCVBUF` (contract 2): not available through `std` in the sandbox; `rewrite-bridge` asks for it through the WASI interface now.
* Port arguments on `/link host|guest`: the port is in the config.

## What needs a person with a Minecraft client or a built Pumpkin

1. Build Pumpkin at the pinned tag with their own server jar, install `pumpkin_link.wasm`, and run
   `PUMPKIN_MINECRAFT_SERVER_JAR=... scripts/live-test.sh --server pumpkin`. First check that the server loads
   the plugin (the ABI is `0.1.0`; the `--inspect` check says the component is well formed, not that Pumpkin
   accepts it).
2. `src/plugin.rs` unverified assumptions: block shapes from `get-block-state` are block-local boxes (read in
   Pumpkin's source, not run); `get-chunk` returns only loaded chunks; `Entity::teleport` / `set-rotation` /
   the display transformation put a block display where the pose says (the translation `(-0.5, -0.5, -0.5)`
   centres it, as minecraft-link describes, but the visual result is unseen); the `/link` command tree and the
   permission node `pumpkin-link:command.link` work with Pumpkin's command system; the repeating task calls
   `on_tick` once per tick with the lock free.
3. With a client: join the server, `/link host`, start a guest (for example rewrite-bridge's `fake_guest`) and
   check a block shows where the guest's entity is and moves; check the collision against the world with a
   guest that collides (`fake_guest --in-front` uses the collision); check yaw/pitch of the player pose against
   what the other game shows (the sign conventions are unit-tested against minecraft-link's table, not against a
   running world).
4. `umask 077` + the token: confirm a real guest accepts the endpoint file the server wrote.

## Uncertain

* Whether Pumpkin accepts the component (it is built with the published API crate; its imports are the
  `pumpkin:plugin/*@0.1.0` and `wasi:*@0.2.x` set listed by `strings`; the sandbox host with wasmtime 50 instantiated it).
* Rescan cost: a radius-16 scan is 33 x 33 x 33 = 35,937 block reads across the plugin boundary, one layer
  (1,089 reads) per tick, so about 1.7 s per rescan; a radius of 64 is 129 layers, about 6.5 s. Not measured
  on a server; the layers-per-tick constant is `SCAN_LAYERS_PER_TICK` in `src/bridge.rs`.
* A server with more than 1024 players publishes only the first 1024 (counted in `players_over_limit`).
* The config file is read when the plugin loads; there is no reload command, so edits need the plugin (or the
  server) restarted.

## Rule notes

No game files or game data were downloaded or committed. The one request for game data came from Pumpkin's own
build script (see above) and failed. Pumpkin's source was read (not copied) to find the socket policy, the
plugin-version identification, and how the host fills block shapes; nothing of it is in the repository. The
`rewrite-bridge` crate is used as a dependency (`=0.3.1`), not vendored. No pull request, tag, release or
publication was made.
