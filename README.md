# pumpkin-link

A plugin for the [Pumpkin](https://github.com/Pumpkin-MC/Pumpkin) Minecraft server that speaks the
rewrite-bridge passthrough link protocol (`docs/CONTRACT.md`, revision 6, with the optional token of
section 6.2, in [rewrite-bridge](https://github.com/patodois/rewrite-bridge)). With it a server shares
what a server holds, its players' poses and the collision of the blocks around a player, with another
game or rewrite on the same machine, and shows that program's entities in the world.

**NOT AN OFFICIAL MINECRAFT PRODUCT. NOT APPROVED BY OR ASSOCIATED WITH MOJANG OR MICROSOFT.**

> **Status.** The protocol, the mapping, the collision snapshots, the config and the token run and are
> tested, natively and inside a WASI sandbox with the rules Pumpkin gives a plugin. The thin layer that
> talks to Pumpkin's plugin API (`src/plugin.rs`) has **not been run in a Pumpkin server** yet: building
> Pumpkin needs the game's data files, which this project never downloads. See "How it is tested" and
> `CLOUD-REPORT.md`.

## What it does

The link connects two programs on one machine: a **host** (owns the shared world) and a **guest**
(contributes entities and poses). The plugin can be either:

| | host (the server listens) | guest (the server connects) |
|---|---|---|
| `hello`, `bye`, heartbeat, watchdog, token | yes | yes |
| the server's players | spawned and posed as host entities (`Minecraft` / `minecraft:player`) | spawned and posed as guest entities |
| collision of the blocks around a player | sent | received, counted |
| the other side's entities | shown in the world as block displays | counted |
| `/link focus` | gives or takes the input focus | -- |

Left out, because a server has no renderer or camera: `Camera` datagrams (not sent; a received one is
counted and dropped), the pixel file and `Frame` datagrams (`frame_channel` and `Frame` are counted and
dropped, nothing is mapped), depth and reprojection (CONTRACT 9.3, 9.6, 9.7). Input focus has no meaning
for a server: a guest's request is ignored, which section 8 allows a host to do.

## Install

Pumpkin's plugins are WebAssembly components. This one is built against `pumpkin-plugin-api`
0.2.1 (Minecraft 26.3), whose ABI is `pumpkin:plugin@0.1.0` (it needs no `plugin-v0_2` build
feature of the server).

```sh
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/pumpkin_link.wasm  <server>/plugins/
```

Start the server once. The plugin writes `plugins/data/pumpkin-link/pumpkin-link.json` with every
setting at its default and does **not** start the link: it opens no port until you ask, with
`"auto_start": true` or `/link host` / `/link guest`.

## Commands

`/link` needs the permission `pumpkin-link:command.link` (operators of level 3 have it by default; the
console always may).

| Command | Meaning |
|---|---|
| `/link` or `/link status` | role, peer, focus, counters, collision size and cuts, token state, last problem |
| `/link host` | listen on `127.0.0.1:<port>` for one guest |
| `/link guest` | connect to a host on `127.0.0.1:<port>` (or the one the endpoint file names), retrying |
| `/link stop` | `bye`, half-close, drain 200 ms, close; the endpoint file is removed |
| `/link focus host\|guest` | host only: give the input focus or take it back |

## Config: `plugins/data/pumpkin-link/pumpkin-link.json`

Modelled on `config/minecraft-link.json` of [minecraft-link](https://github.com/patodois/minecraft-link);
only the keys that apply to a server are here, unknown keys are ignored, a key of the wrong type or out
of range stops the link from starting and is named in `/link status`.

| Key | Default | Meaning |
|---|---|---|
| `auto_start` | `false` | start `role` when the plugin loads |
| `role` | `"host"` | `"host"` or `"guest"` |
| `port` | `47800` | control port: the host listens on it, the guest connects to it (`0` = any free port, host) |
| `udp_port` | `0` | `0` = any free port |
| `flip_x`, `flip_z` | `false` | mirror the world along X or Z when the other game's axes differ |
| `scale` | `1.0` | link metres per block |
| `offset` | `[0,0,0]` | link position of the Minecraft origin (after flip and scale) |
| `collision_player` | `""` | host: the player the collision snapshot is centred on; empty = the first online |
| `collision_radius` | `16` | host: blocks around that player (1 to 64) |
| `collision_interval_ms` | `500` | host: least time between rescans |
| `collision_move_blocks` | `1.0` | host: rescan at once after the player moved this far |
| `show_guests` | `true` | host: show the guest's entities as block displays |
| `guest_block` | `"minecraft:magenta_glazed_terracotta"` | host: the block they are shown as |
| `guest_entity` | `1` | the id of the first player; the next ones take the next ids |
| `guest_provider`, `guest_asset` | `"Minecraft"`, `"minecraft:player"` | what `spawn` announces for a player |
| `heartbeat_ms` | `250` | 10 to 10000 |
| `timeout_ms` | `2000` | the watchdog is `max(timeout_ms, 4 x the peer's heartbeat)` |
| `token_auth` | `false` | the link token (below) |
| `endpoint_file` | `""` | with `token_auth`: the endpoint file; empty = `endpoint.json` in the data folder |

Not carried over from minecraft-link, because they belong to a client: `show_frames`,
`interp_delay_ms` (guest entities are placed at their latest pose, not interpolated).

## Coordinates (CONTRACT section 4)

Same rules as minecraft-link: a block is a metre, Minecraft's axes are already right-handed with +Y up,
so with the defaults a position maps to itself. Directions are converted: yaw 0 faces +Z (south) and
grows clockwise, pitch +90 looks down, the entity rotation is `Ry(-yaw) * Rx(pitch)` so its front (+Z)
is the view direction. A pose is the feet position, the view rotation, the displacement since the last
tick divided by the time between ticks as velocity, the yaw change as angular velocity about +Y, and the
`TELEPORT` flag on a player's first pose and after a jump of more than 8 blocks. With a mirror
(`flip_x` xor `flip_z`) the facing is mirrored and the triangles' winding is reversed so they stay
counter-clockwise from outside. All of it is in `src/mapping.rs` with its tests.

## Collision

A snapshot is the collision boxes the server already holds for each block state (the plugin API's
`collision-shapes`), turned into triangles: faces hidden behind a full block are left out, so solid ground
costs about its top. The world is read one horizontal layer per tick, so a radius of 16 takes about 1.7 s
to rescan; an unchanged result is sent again only every 5 s. A snapshot over the contract's 925,696
triangles is cut to the nearest ones and announces the smaller radius (`/link status` says so).

## Token (optional, CONTRACT 6.2)

`"token_auth": true` on a host makes a new 32-byte token from the sandbox's random source each time the
host starts, binds the port, **then** writes the endpoint file `{"control_port":N,"token":"..."}` atomically,
and removes it on stop. A guest with `token_auth` reads the file before every attempt. The token is never
logged, shown or put in a `bye`.

**Limit: file mode.** The sandbox has no file mode bits, so the plugin cannot create the file `0600` from the
start as section 6.2 asks; it gets the mode the server process's umask gives it, and the plugin cannot read it
back to check. Start the server with `umask 077` (and keep the data folder to yourself) when you use the
token. A guest built on `rewrite-bridge` refuses an endpoint file that group or others can read.

## Permissions it asks for, and why

| Permission | Why |
|---|---|
| `network.tcp` | listen on, or connect to, the control port |
| `network.udp` | the state datagrams |
| `network.loopback` | restricts both to loopback addresses; the contract allows nothing else |
| `fs.write.data` | the config file and the endpoint file in the plugin's own data folder |

It asks for no outbound network, HTTP, DNS, environment or system information.

## How it is tested

* `cargo test --no-default-features`: unit tests (mapping, config, collision, endpoint file, session rules)
  and end-to-end tests of the bridge against real `rewrite-bridge` peers over loopback.
* `scripts/live-test.sh --server sandbox`: runs the same code as a WebAssembly component in
  `tools/sandbox-host` (wasmtime-wasi with Pumpkin's socket and folder rules) and drives it with the
  `rewrite-bridge` examples and `examples/linkpeer.rs`, with and without the token and with a wrong one.
* `scripts/live-test.sh --server pumpkin`: the same cases against a real Pumpkin built from source at the
  pinned release tag in a temporary folder. It needs `PUMPKIN_MINECRAFT_SERVER_JAR`, a server jar you
  already have, because Pumpkin's build reads the game's data from it; the script never downloads it.

## Why not `rewrite_bridge::link::Link`?

It compiles for `wasm32-wasip2` but cannot run there: the host's session number uses
`std::process::id()`, which panics on that target, and a guest connects from a spawned thread, which
that target does not have. `src/session.rs` does both roles from the contract on non-blocking sockets
driven by the server's tick and uses only the crate's wire codec.

## License

MIT OR Apache-2.0, at your option. Pumpkin is GPL-3.0 and is only built as a test harness in a temporary
folder; nothing of it is in this repository.
