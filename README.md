# pumpkin-link

A plugin for the [Pumpkin](https://github.com/Pumpkin-MC/Pumpkin) Minecraft server that speaks the
rewrite-bridge passthrough link protocol (`docs/CONTRACT.md`, revision 6, with the optional token of
section 6.2, in [rewrite-bridge](https://github.com/patodois/rewrite-bridge)). With it a server shares
what a server holds, its players' poses and the collision of the blocks around a player, with another
game or rewrite on the same machine, and shows that program's entities in the world.

**NOT AN OFFICIAL MINECRAFT PRODUCT. NOT APPROVED BY OR ASSOCIATED WITH MOJANG OR MICROSOFT.**

> **Status.** The protocol, the mapping, the collision snapshots, the config and the token are tested natively
> and inside a WASI sandbox with the rules Pumpkin gives a plugin. The plugin has also run in a real Pumpkin server
> with a real Minecraft 26.3 client on a local test world: poses, exact collision, the guest entity, `/link status`
> and `/link stop`, and a soak of about 11 minutes. See "How it is tested".

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
| `endpoint_file` | `""` | with `token_auth`: the name of the endpoint file **in the plugin's data folder** (a plugin sees no other folder, so a path is refused); empty = `endpoint.json` |

Not carried over from minecraft-link, because they belong to a client: `show_frames`,
`interp_delay_ms` (guest entities are placed at their latest pose, not interpolated).

## Coordinates (CONTRACT section 4)

Same rules as minecraft-link: a block is a metre, Minecraft's axes are already right-handed with +Y up,
so with the defaults a position maps to itself. Directions are converted: yaw 0 faces +Z (south) and
grows clockwise, pitch +90 looks down, the entity rotation is `Ry(-yaw) * Rx(pitch)` so its front (+Z)
is the view direction. A pose is the feet position, the view rotation, the displacement since the last
tick divided by the time between ticks as velocity, the yaw change as angular velocity about +Y, and the
`TELEPORT` flag on a player's first pose and after a jump of more than 8 blocks.

A jump of 8 blocks or less (a console `tp` of a few blocks, say) carries no `TELEPORT` flag, as in
minecraft-link, so for one pose it looks like fast motion: its velocity is the jump divided by the time between two
ticks, and a guest that extrapolates with it overshoots for a moment.

**What a guest entity's pose is.** The block display is drawn centred on the pose's translation (a 1 m block, translation
-0.5 on all three axes), as `minecraft-link`'s `RemoteEntities` does. CONTRACT section 9.4 says only "world position", and the
`fake_guest` example puts its cube's *centre* at the pose (0.8 m above the ground for a 1.6 m cube), so a block resting on
the ground is posed 0.5 m above it. The pose's `scale` is not applied. With a mirror
(`flip_x` xor `flip_z`) the facing is mirrored and the triangles' winding is reversed so they stay
counter-clockwise from outside. All of it is in `src/mapping.rs` with its tests.

## Guest displays

The guest's entities are block displays in the server's first world. Each one is tagged in the entity's own
persistent data (`pumpkin-link` / `guest` = `<session>:<guest entity>`), which the server saves with the entity, and
the plugin sweeps the tagged entities of every world once a second while a link runs (every two seconds
otherwise). That is what keeps a display from outliving its session:

* a chunk that unloads and loads again brings the display back as a new entity: the plugin takes it over (the
  poses move it again at once) and removes any duplicate;
* `/link stop`, a lost session and unloading the plugin remove the displays, the reloaded copies too;
* a display left in a saved world by an earlier run (the server killed, a crash) is removed when its chunk loads;
* an entity without our tag (a map maker's or another plugin's block displays) is never touched.

Pumpkin only lists entities of loaded chunks, so a leftover in a chunk nobody has loaded is removed when the chunk loads.

## Collision

A snapshot is the collision boxes the server already holds for each block state (the plugin API's
`collision-shapes`), turned into triangles: faces hidden behind a full block are left out, so solid ground
costs about its top. The world is read one horizontal layer per tick, so a radius of 16 takes about 1.7 s
to rescan; an unchanged result is sent again only every 5 s. A snapshot over the contract's 925,696
triangles is cut to the nearest ones and announces the smaller radius (`/link status` says so).

**After a join.** For a few ticks after a player joins, Pumpkin holds it at a position of its own (the world spawn, `0 100 0` in a
default world) until the client's first position arrives. The plugin publishes that position and may centre one snapshot on it
(on a desert world it was empty, 0 triangles); it cannot tell that state from a player who really stands there, since the plugin API has no
"the client has confirmed its position" call, so it does not hide it. The first pose carries `TELEPORT`, the jump to the real position
is flagged the same way, and the next scan, at most `collision_interval_ms` plus about 1.7 s later, replaces the snapshot.

## Token (optional, CONTRACT 6.2)

`"token_auth": true` on a host makes a new 32-byte token from the system's random source (WASI `random` in
the sandbox) each time the host starts, binds the port, **then** writes the endpoint file `{"control_port":N,"token":"..."}` atomically,
and removes it on stop. A guest with `token_auth` reads the file before every attempt. The token is never
logged, shown or put in a `bye`.

**Where.** CONTRACT 6.2 leaves the place to the application. A plugin's sandbox shows it only its own data
folder, so the file is `plugins/data/pumpkin-link/endpoint.json` (the recommended `rewrite-link` folder under
`$XDG_RUNTIME_DIR` is out of reach); point the other program at that path. A guest-role server reads a file
that its host wrote into the same data folder (copy it there, or have the host write it there).

**Limit: file mode.** A plugin cannot ask for a file mode when it creates a file, so it cannot create the file
`0600` from the start as section 6.2 asks. The file gets what the server process's umask leaves: 0644 under the
usual umask 022, 0600 under 077 (measured under wasmtime). Under 022 other users can read the token, and a guest built on
`rewrite-bridge` refuses the file because group and others can read it, so such a server cannot serve a native
guest. The plugin cannot read the mode back either. Start the server with `umask 077`: that makes the file 0600
and lets both a native guest and the plugin use it. Keeping the data folder to yourself (mode 0700) keeps other users
away from the token, but under umask 022 the file in it is still 0644 and a native guest still refuses it. On
Windows there is no umask; the folder's ACL is what counts.

## Permissions it asks for, and why

| Permission | Why |
|---|---|
| `network.tcp` | listen on, or connect to, the control port |
| `network.udp` | the state datagrams |
| `network.loopback` | restricts both to loopback addresses; the contract allows nothing else |
| `fs.write.data` | the config file and the endpoint file in the plugin's own data folder |

It asks for no outbound network, HTTP, DNS, environment or system information.

## How it is tested

* `cargo test --no-default-features`: unit tests (mapping, config, collision), the rules of the protocol checked with hand-written peers
  on raw sockets, and end-to-end tests of the bridge against real `rewrite-bridge` peers over loopback.
* `scripts/live-test.sh --server sandbox`: runs the same code as a WebAssembly component in
  `tools/sandbox-host` (wasmtime-wasi with Pumpkin's socket and folder rules) and drives it with the
  `rewrite-bridge` examples and `examples/linkpeer.rs`, with and without the token and with a wrong one.
* `scripts/live-test.sh --server pumpkin`: the same cases against a real Pumpkin built from source at the
  pinned release tag in a temporary folder. It needs `PUMPKIN_MINECRAFT_SERVER_JAR`, a server jar you
  already have, because Pumpkin's build reads the game's data from it; the script never downloads it.

## The link

Both roles are `rewrite_bridge::link::Link`, polled once per server tick. On `wasm32-wasip2` there are no
threads, no memory-mapped files and no way to set a file's mode, so there:

* a guest connects inside `poll`, for at most 100 ms: an accept or a refusal on loopback took 0.2 to 0.7 ms in
  the measurements on macOS, but a host that never accepts would leave the connect hanging, and the server's tick
  with it. Where a refusal is slow (about two seconds on Windows) a guest with no host reports `timed out` after
  each 100 ms, not `connection refused`;
* a peer that holds its connection open and stops reading is not waited for: the runtime would hold the
  server's tick until the peer reads or exits (for ever on Linux), so the connection is set aside and closed
  when the peer lets go; with four of them set aside the plugin takes no new connection until one is. The four
  are counted for the whole server plugin, and `/link status` shows `connection: not accepting: …` while it lasts;
* `set_nodelay` does nothing there, so small control lines can be held back a few milliseconds and arrive
  together: a pose can arrive before its `spawn` and be dropped as the pose of an unknown entity, which the
  contract allows;
* the pixel file is not available; the UDP socket is the WASI one (receiving through `std` leaks every
  datagram's payload there), and the 4 MiB receive buffer of CONTRACT section 2 is asked for (under wasmtime on macOS it was granted; not
  measured elsewhere);
* the endpoint file gets the mode of the server's umask (see "Token" above).

This needs a `rewrite-bridge` release that runs on that target; earlier ones panic when a host sees a guest's
`hello` and cannot connect as a guest.

## License

MIT OR Apache-2.0, at your option. Pumpkin is GPL-3.0 and is only built as a test harness in a temporary
folder; nothing of it is in this repository.
