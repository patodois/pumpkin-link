#!/usr/bin/env bash
# End-to-end test of the plugin against real peers over loopback.
#
#   scripts/live-test.sh --server sandbox   # the plugin's code in a WASI sandbox with Pumpkin's rules (no game data needed)
#   scripts/live-test.sh --server pumpkin   # a real Pumpkin server built from source (needs YOUR copy of the game's server jar)
#
# What it does, in a temporary folder outside this repository (nothing is written to the repository):
#   1. builds the plugin (wasm32-wasip2) and, for rewrite-bridge 0.3.0 from crates.io, its examples
#      `fake_guest`, `fake_host`, `interop_peer` and `conformance`;
#   2. `--server pumpkin`: clones Pumpkin at the pinned commit and builds it; installs the plugin;
#      `--server sandbox`: builds tools/sandbox-host and the `sandbox_server` example;
#   3. starts the server headless once per case, runs the peers, stops it, and checks the answers:
#        host role   : no token / token (endpoint file) / wrong token / no token while one is required
#        guest role  : no token / token (endpoint file) / a host that requires a token the server lacks
#   4. prints a table and exits 1 if any case failed.
#
# Game data: Pumpkin's build needs the game's data, which this script never downloads. For
# `--server pumpkin` set PUMPKIN_MINECRAFT_SERVER_JAR to a server jar you already have; Pumpkin's own
# build step reads it (it would otherwise fetch it itself, which is why the script insists).
#
# `--server pumpkin` needs no player and no Minecraft client. The server is told, in ITS OWN throw-away folder
# ($WORK/server/pumpkin.toml, written by this script), to: bind the Java port to 127.0.0.1 only (JAVA_PORT, default
# 25599), run offline mode, turn Bedrock and telemetry off, and pre-approve the plugin's four permissions (the
# permission prompt needs a terminal). The link is started and stopped from the server console (`link host`,
# `link guest`, `link status`, `link stop`, `stop`). With nobody online the server has no players to publish and no
# collision centre, so the checks that need them (peers seeing players, a collision snapshot) are reported as
# NEEDS-CLIENT and are not counted as passes; they do not fail the run.
#
# Environment: LINK_PORT (default 47811), JAVA_PORT (default 25599), KEEP=1 (keep the temporary folder), WORK (use
# this folder).
set -u
set -o pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# The Pumpkin release whose plugin API crate (pumpkin-plugin-api 0.2.1+26.3-26.51, which carries the
# `pumpkin:plugin@0.1.0` ABI) this plugin is built against: the tag 0.2.1+26.3-26.51.
PUMPKIN_REPO="https://github.com/Pumpkin-MC/Pumpkin.git"
PUMPKIN_COMMIT="224f9f8144ca53a8d0800acb87607d714196e2a6"
RB_VERSION="0.3.0"
PORT="${LINK_PORT:-47811}"
JAVA_PORT="${JAVA_PORT:-25599}"
SERVER="sandbox"
while [ $# -gt 0 ]; do
  case "$1" in
    --server) SERVER="${2:-}"; shift 2 ;;
    -h|--help) sed -n '2,32p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
case "$SERVER" in sandbox|pumpkin) ;; *) echo "--server must be sandbox or pumpkin" >&2; exit 2 ;; esac

if [ "$SERVER" = pumpkin ] && [ -z "${PUMPKIN_MINECRAFT_SERVER_JAR:-}" ]; then
  echo "--server pumpkin needs PUMPKIN_MINECRAFT_SERVER_JAR=/path/to/your/server.jar (this script never downloads game files)." >&2
  exit 2
fi

WORK="${WORK:-$(mktemp -d "${TMPDIR:-/tmp}/pumpkin-link-live.XXXXXX")}"
case "$WORK" in "$REPO"|"$REPO"/*) echo "WORK must be outside the repository" >&2; exit 2 ;; esac
mkdir -p "$WORK/logs"
echo "work folder: $WORK"
export CARGO_TARGET_DIR="$WORK/target"

SERVER_PID=""
cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null
  [ -n "${HOLD_PID:-}" ] && kill "$HOLD_PID" 2>/dev/null
  # The temporary folder is left in place (the script never deletes anything); it is outside the repository.
  return 0
}
trap cleanup EXIT

step() { echo; echo "== $*"; }
RESULTS=()
FAILED=0
record() { # name status detail
  RESULTS+=("$(printf '%-52s %-5s %s' "$1" "$2" "$3")")
  case "$2" in PASS|NEEDS-CLIENT) ;; *) FAILED=1 ;; esac
}

# ---------------------------------------------------------------------------------------------
step "build the plugin and the peers"
(cd "$REPO" && cargo build --release --target wasm32-wasip2) > "$WORK/logs/build-plugin.log" 2>&1 \
  || { tail -20 "$WORK/logs/build-plugin.log"; echo "plugin build failed"; exit 1; }
# Copied at once: the sandbox example below is built from the same library without the plugin glue and
# writes the same file name.
cp "$WORK/target/wasm32-wasip2/release/pumpkin_link.wasm" "$WORK/pumpkin_link.plugin.wasm"
PLUGIN_WASM="$WORK/pumpkin_link.plugin.wasm"
ls -l "$PLUGIN_WASM"

mkdir -p "$WORK/rb"
if [ ! -d "$WORK/rb/rewrite-bridge-$RB_VERSION" ]; then
  curl -fsSL "https://crates.io/api/v1/crates/rewrite-bridge/$RB_VERSION/download" -o "$WORK/rb.tgz" \
    && tar xzf "$WORK/rb.tgz" -C "$WORK/rb" || { echo "cannot fetch rewrite-bridge $RB_VERSION"; exit 1; }
fi
RB="$WORK/rb/rewrite-bridge-$RB_VERSION"
(cd "$RB" && cargo build --release --features link --example fake_guest --example fake_host --example interop_peer --example conformance) \
  > "$WORK/logs/build-rb.log" 2>&1 || { tail -20 "$WORK/logs/build-rb.log"; echo "rewrite-bridge examples failed"; exit 1; }
(cd "$REPO" && cargo build --release --no-default-features --example linkpeer) \
  > "$WORK/logs/build-peer.log" 2>&1 || { tail -20 "$WORK/logs/build-peer.log"; echo "linkpeer build failed"; exit 1; }
BIN="$WORK/target/release/examples"          # linkpeer
RBBIN="$RB/target/release/examples"          # rewrite-bridge examples
# CARGO_TARGET_DIR applies to the rewrite-bridge build too
[ -x "$RBBIN/fake_guest" ] || RBBIN="$WORK/target/release/examples"

# ---------------------------------------------------------------------------------------------
if [ "$SERVER" = sandbox ]; then
  step "build the sandbox host and the sandbox_server example"
  (cd "$REPO/tools/sandbox-host" && CARGO_TARGET_DIR="$WORK/target-sandbox" cargo build --release) \
    > "$WORK/logs/build-sandbox.log" 2>&1 || { tail -20 "$WORK/logs/build-sandbox.log"; exit 1; }
  (cd "$REPO" && cargo build --release --target wasm32-wasip2 --no-default-features --example sandbox_server) \
    > "$WORK/logs/build-sandbox-example.log" 2>&1 || { tail -20 "$WORK/logs/build-sandbox-example.log"; exit 1; }
  SANDBOX_HOST="$WORK/target-sandbox/release/sandbox-host"
  SANDBOX_WASM="$WORK/target/wasm32-wasip2/release/examples/sandbox_server.wasm"
  DATA="$WORK/sandbox-data"
else
  step "build Pumpkin at $PUMPKIN_COMMIT"
  if [ ! -d "$WORK/pumpkin/.git" ]; then
    git clone --quiet "$PUMPKIN_REPO" "$WORK/pumpkin" && git -C "$WORK/pumpkin" checkout --quiet "$PUMPKIN_COMMIT" \
      || { echo "cannot clone Pumpkin"; exit 1; }
  fi
  # Pumpkin pins extra rustup components; the stable toolchain is enough to build it.
  printf '[toolchain]\nchannel = "stable"\n' > "$WORK/pumpkin/rust-toolchain.toml"
  (cd "$WORK/pumpkin" && CARGO_TARGET_DIR="$WORK/target-pumpkin" cargo build --release -p pumpkin) \
    > "$WORK/logs/build-pumpkin.log" 2>&1 || { tail -30 "$WORK/logs/build-pumpkin.log"; echo "Pumpkin build failed"; exit 1; }
  PUMPKIN_BIN="$WORK/target-pumpkin/release/pumpkin"
  RUN="$WORK/server"
  mkdir -p "$RUN/plugins"
  cp "$PLUGIN_WASM" "$RUN/plugins/pumpkin_link.wasm"
  DATA="$RUN/plugins/data/pumpkin-link"
  # Server settings, written before the first start (Pumpkin fills in every other key with its default).
  #  - java address 127.0.0.1: nothing but this machine can connect; offline mode, so no account server is asked
  #  - bedrock off: it would bind 0.0.0.0 (and its own sockets) otherwise
  #  - telemetry off: the default sends a heartbeat to a server of the Pumpkin project
  #  - allowed_permissions: the plugin's four permissions are pre-approved, because the prompt needs a terminal
  #    and a denied answer is remembered in plugins/permission_cache.json
  cat > "$RUN/pumpkin.toml" <<TOML
[telemetry]
enabled = false

[networking.java]
address = "127.0.0.1:$JAVA_PORT"
online_mode = false
encryption = false

[networking.bedrock]
enabled = false

[plugins.overrides.pumpkin-link]
allowed_permissions = ["network.tcp", "network.udp", "network.loopback", "fs.write.data"]
TOML
fi
mkdir -p "$DATA"

# ---------------------------------------------------------------------------------------------
new_case() {
  if [ "$SERVER" = sandbox ]; then
    # a fresh data folder per case, so no stop marker or endpoint file of an earlier case is there
    mkdir -p "$WORK/cases"; DATA="$(mktemp -d "$WORK/cases/case.XXXXXX")"
  else
    mkdir -p "$DATA"
  fi
}
write_config() { # json body (without braces)
  printf '{ %s }\n' "$1" > "$DATA/pumpkin-link.json"
}

start_server() { # logfile
  SERVER_LOG="$1"
  if [ "$SERVER" = sandbox ]; then
    # umask 077: the sandbox cannot set file modes, so the endpoint file gets the process's umask
    # (README, "Token")
    ( umask 077; exec "$SANDBOX_HOST" --data "$DATA" "$SANDBOX_WASM" --seconds 90 --players 2 ) > "$SERVER_LOG" 2>&1 &
    SERVER_PID=$!
  else
    FIFO="$RUN/stdin"
    [ -p "$FIFO" ] || mkfifo "$FIFO"
    ( exec sleep 100000 > "$FIFO" ) & HOLD_PID=$!   # exec: killing HOLD_PID must kill the sleep itself
    ( cd "$RUN" && umask 077 && exec "$PUMPKIN_BIN" < "$FIFO" ) > "$SERVER_LOG" 2>&1 &
    SERVER_PID=$!
  fi
}

stop_server() {
  [ -n "$SERVER_PID" ] || return 0
  if [ "$SERVER" = sandbox ]; then
    : > "$DATA/stop"
  else
    echo "stop" > "$RUN/stdin" 2>/dev/null
  fi
  for _ in $(seq 1 100); do kill -0 "$SERVER_PID" 2>/dev/null || break; sleep 0.1; done
  kill "$SERVER_PID" 2>/dev/null; wait "$SERVER_PID" 2>/dev/null
  [ -n "${HOLD_PID:-}" ] && { kill "$HOLD_PID" 2>/dev/null; wait "$HOLD_PID" 2>/dev/null; HOLD_PID=""; }
  SERVER_PID=""
}

wait_port() { # up to 180 s for the control port to accept
  for _ in $(seq 1 1800); do
    (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && return 0
    kill -0 "$SERVER_PID" 2>/dev/null || return 1
    sleep 0.1
  done
  return 1
}

console() { echo "$*" > "$RUN/stdin"; }   # a line typed at the Pumpkin console

wait_ready() { # Pumpkin has loaded its plugins and says it is running
  for _ in $(seq 1 600); do
    grep -q "Server is now running" "$SERVER_LOG" 2>/dev/null && return 0
    kill -0 "$SERVER_PID" 2>/dev/null || return 1
    sleep 0.1
  done
  return 1
}

# Starts the server, and for Pumpkin types `link host` / `link guest` at its console (the sandbox starts the link
# itself from the config, `auto_start`).
start_linked() { # logfile role
  start_server "$1"
  if [ "$SERVER" = pumpkin ]; then
    wait_ready || return 1
    console "link $2"
  fi
}

# Arguments that make linkpeer ask nothing of players and collision (a Pumpkin server with nobody online has neither).
PEERARGS=""
[ "$SERVER" = pumpkin ] && PEERARGS="--players 0 --collision 0"

wait_endpoint() { for _ in $(seq 1 300); do [ -s "$DATA/endpoint.json" ] && return 0; sleep 0.1; done; return 1; }

check() { # name command...   (the command's last output line is the detail)
  local name="$1"; shift
  sleep 1.5 # the server ends the previous peer's session on its next ticks; a peer that comes sooner gets "busy"
  local out; out="$("$@" 2>&1)"; local code=$?
  echo "$out" > "$WORK/logs/$(echo "$name" | tr -c 'A-Za-z0-9\n' '_').log"
  local last; last="$(echo "$out" | tail -1)"
  if [ $code -eq 0 ]; then record "$name" PASS "${last:0:70}"; else record "$name" FAIL "${last:0:70}"; fi
}

grep_log() { # name pattern file   (colour codes of the server's log are removed first)
  if sed 's/\x1b\[[0-9;]*m//g' "$3" | grep -q -- "$2"; then record "$1" PASS "found: $2"; else record "$1" FAIL "missing in server log: $2"; fi
}

# ---------------------------------------------------------------------------------------------
AUTO=true; [ "$SERVER" = pumpkin ] && AUTO=false   # Pumpkin: the link is started by `link host` at the console
HOSTCFG="\"auto_start\": $AUTO, \"role\": \"host\", \"port\": $PORT, \"collision_radius\": 2, \"collision_interval_ms\": 200"
GUESTCFG="\"auto_start\": $AUTO, \"role\": \"guest\", \"port\": $PORT, \"collision_radius\": 2"

run_host_case() { # label extra-config
  local label="$1" extra="$2"
  step "host role: $label"
  new_case
  write_config "$HOSTCFG$extra"
  if ! start_linked "$WORK/logs/server-host-$label.log" host; then
    record "host/$label: server listens" FAIL "server did not come up (see $SERVER_LOG)"; stop_server; return
  fi
  if ! wait_port; then record "host/$label: server listens" FAIL "no control port (see $SERVER_LOG)"; stop_server; return; fi
  record "host/$label: server listens" PASS "127.0.0.1:$PORT"
  if [ "$SERVER" = pumpkin ] && [ "$label" = plain ]; then
    grep_log "plugin: Pumpkin loads it (name, version)" "Loaded pumpkin-link (0.1.0)" "$SERVER_LOG"
    grep_log "plugin: permissions Pumpkin reports" 'uses the following permissions: \["network.tcp", "network.udp", "network.loopback", "fs.write.data"\]' "$SERVER_LOG"
    grep_log "server: Java port bound to 127.0.0.1 only" "Java Edition: 127.0.0.1:$JAVA_PORT" "$SERVER_LOG"
  fi
  case "$label" in
    plain)
      check "host/plain: linkpeer connects, sends its entity" "$BIN/linkpeer" guest --port "$PORT" --expect ok $PEERARGS
      if [ "$SERVER" = pumpkin ]; then
        # `link status` while the peer is connected: typed 4 s from now, the peer (6 s) is still there
        ( sleep 4; console "link status" ) & STJOB=$!
      fi
      # the output is collected first: `| grep -q` would end fake_guest (SIGPIPE) at its first line
      check "host/plain: rewrite-bridge fake_guest connects" bash -c \
        "out=\$('$RBBIN/fake_guest' --port $PORT --seconds 6 2>&1); echo \"\$out\" >&2; echo \"\$out\" | grep -q 'connected: host' && echo 'fake_guest connected'"
      [ "$SERVER" = pumpkin ] && wait "$STJOB"   # never a bare `wait`: the console's stdin holder runs for good
      ;;
    token)
      wait_endpoint || record "host/token: endpoint file appears" FAIL "no $DATA/endpoint.json"
      check "host/token: linkpeer with the endpoint file" "$BIN/linkpeer" guest --port "$PORT" --endpoint "$DATA/endpoint.json" --expect ok $PEERARGS
      check "host/token: linkpeer without a token is refused" "$BIN/linkpeer" guest --port "$PORT" --expect unauthorized --seconds 8
      check "host/token: linkpeer with a wrong token is refused" "$BIN/linkpeer" guest --port "$PORT" --token 0123456789abcdef --expect unauthorized --seconds 8
      check "host/token: interop_peer --wrong-token" "$RBBIN/interop_peer" guest --port "$PORT" --label live --endpoint "$DATA/endpoint.json" --wrong-token
      ;;
  esac
  if [ "$SERVER" = pumpkin ]; then
    console "link status"; sleep 2
  fi
  if [ "$label" = token ] && [ -s "$DATA/endpoint.json" ]; then
    TOKEN="$(sed -n 's/.*"token":"\([0-9a-f]*\)".*/\1/p' "$DATA/endpoint.json")"
    cp "$DATA/endpoint.json" "$WORK/logs/last-endpoint-file.txt" 2>/dev/null
  fi
  if [ "$SERVER" = pumpkin ]; then
    # `link stop` at the console: the port is released and no endpoint file is left, with the server still running
    console "link stop"; sleep 2
    if [ -e "$DATA/endpoint.json" ]; then record "host/$label: link stop leaves no endpoint file" FAIL "$DATA/endpoint.json exists"
    else record "host/$label: link stop leaves no endpoint file" PASS "no endpoint.json in the data folder"; fi
    if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then record "host/$label: link stop closes the control port" FAIL "port $PORT still accepts"
    else record "host/$label: link stop closes the control port" PASS "port $PORT closed"; fi
  fi
  stop_server
  if [ "$label" = token ] && [ -n "${TOKEN:-}" ]; then
    if grep -rqF -- "$TOKEN" "$SERVER_LOG" "$WORK/logs"/linkpeer*.log "$WORK/logs"/host_token*.log 2>/dev/null; then
      record "host/token: the token appears in no log" FAIL "token found in a log"
    else record "host/token: the token appears in no log" PASS "not in the server or peer logs"; fi
  fi
  if [ "$SERVER" = sandbox ]; then
    grep_log "host/$label: guest entity shown by the server" "GUEST spawn entity=90 provider=linkpeer" "$SERVER_LOG"
    grep_log "host/$label: guest entity despawned or cleared" "GUEST \(despawn\|clear\)" "$SERVER_LOG"
    if [ "$label" = token ]; then
      if grep -q "refused as unauthorized" "$SERVER_LOG"; then record "host/token: status counts refusals" PASS "refused as unauthorized"; else record "host/token: status counts refusals" FAIL "no count in status"; fi
    fi
  else
    # what Pumpkin itself printed: the plugin's block display for the peer's entity 90, and `/link status`
    grep_log "host/$label: guest entity shown as a block display" "pumpkin-link: guest entity 90 shown as block display" "$SERVER_LOG"
    grep_log "host/$label: guest entity removed again" "pumpkin-link: guest entity 90 removed\\|pumpkin-link: [0-9]* guest entities cleared" "$SERVER_LOG"
    grep_log "host/$label: /link status answers" "link: host on 127.0.0.1:$PORT" "$SERVER_LOG"
    [ "$label" = plain ] && grep_log "host/plain: /link status shows a connected guest" "peer: Fake Guest (guest)" "$SERVER_LOG"
    grep_log "host/$label: no player online, no collision sent" "collision: no snapshot sent yet" "$SERVER_LOG"
    if [ "$label" = token ]; then
      if grep -q "refused as unauthorized" "$SERVER_LOG"; then record "host/token: status counts refusals" PASS "refused as unauthorized"; else record "host/token: status counts refusals" FAIL "no count in status"; fi
    fi
    record "host/$label: peers see players, poses, collision" NEEDS-CLIENT "needs a player on the server"
  fi
}

run_guest_case() { # label extra-config
  local label="$1" extra="$2"
  step "guest role: $label"
  new_case
  write_config "$GUESTCFG$extra"
  case "$label" in
    plain)
      "$BIN/linkpeer" host --port "$PORT" --seconds 40 $PEERARGS > "$WORK/logs/peer-host-plain.log" 2>&1 &
      PEER=$!
      sleep 1; start_linked "$WORK/logs/server-guest-plain.log" guest
      wait "$PEER"; code=$?
      [ $code -eq 0 ] && record "guest/plain: linkpeer host: the server connects as guest" PASS "$(tail -1 "$WORK/logs/peer-host-plain.log" | cut -c1-70)" \
        || record "guest/plain: linkpeer host: the server connects as guest" FAIL "$(tail -1 "$WORK/logs/peer-host-plain.log" | cut -c1-70)"
      stop_server
      new_case; write_config "$GUESTCFG"   # a fresh data folder: the old one holds the stop marker
      "$RBBIN/fake_host" --port "$PORT" --seconds 12 > "$WORK/logs/fake-host.log" 2>&1 &
      PEER=$!
      sleep 1; start_linked "$WORK/logs/server-guest-fakehost.log" guest
      STJOB=""; if [ "$SERVER" = pumpkin ]; then ( sleep 5; console "link status" ) & STJOB=$!; fi
      wait "$PEER"
      if grep -q 'connected: guest' "$WORK/logs/fake_host.log" 2>/dev/null || grep -q 'connected: guest' "$WORK/logs/fake-host.log"; then
        record "guest/plain: rewrite-bridge fake_host sees the server" PASS "connected: guest"
      else record "guest/plain: rewrite-bridge fake_host sees the server" FAIL "$(tail -1 "$WORK/logs/fake-host.log" | cut -c1-70)"; fi
      if [ "$SERVER" = pumpkin ]; then
        grep_log "guest/plain: /link status says guest, with the peer" "link: guest, connecting to 127.0.0.1:$PORT" "$SERVER_LOG"
        grep_log "guest/plain: /link status shows the host peer" "peer: Fake Host (host)" "$SERVER_LOG"
        record "guest/plain: fake_host saw the players spawn" NEEDS-CLIENT "needs a player on the server"
      else
        grep -q 'guest spawned entity' "$WORK/logs/fake-host.log" \
          && record "guest/plain: fake_host saw the players spawn" PASS "guest spawned entity" \
          || record "guest/plain: fake_host saw the players spawn" FAIL "no spawn line"
      fi
      [ -n "$STJOB" ] && wait "$STJOB"
      stop_server
      ;;
    token)
      # the peer host writes the endpoint file; the server (token_auth guest) reads it
      EP="$(mktemp -d "$WORK/ep.XXXXXX")/endpoint.json"   # a fresh path: an old file must not be mistaken for the new one
      "$BIN/linkpeer" host --port "$PORT" --endpoint "$EP" --seconds 40 $PEERARGS > "$WORK/logs/peer-host-token.log" 2>&1 &
      PEER=$!
      for _ in $(seq 1 50); do [ -s "$EP" ] && break; sleep 0.1; done
      chmod 600 "$EP"
      write_config "$GUESTCFG, \"token_auth\": true, \"endpoint_file\": \"peer-endpoint.json\""
      cp -p "$EP" "$DATA/peer-endpoint.json"
      start_linked "$WORK/logs/server-guest-token.log" guest
      wait "$PEER"; code=$?
      [ $code -eq 0 ] && record "guest/token: server reads the endpoint file and connects" PASS "$(tail -1 "$WORK/logs/peer-host-token.log" | cut -c1-70)" \
        || record "guest/token: server reads the endpoint file and connects" FAIL "$(tail -1 "$WORK/logs/peer-host-token.log" | cut -c1-70)"
      stop_server
      ;;
    refused)
      # a host that requires a token, a server that has none: the server is refused and no session starts
      EP="$(mktemp -d "$WORK/ep.XXXXXX")/endpoint.json"
      "$RBBIN/interop_peer" host --port "$PORT" --label live --endpoint "$EP" --expect-unauthorized > "$WORK/logs/interop-host.log" 2>&1 &
      PEER=$!
      sleep 1; start_linked "$WORK/logs/server-guest-refused.log" guest
      wait "$PEER"; code=$?
      [ $code -eq 0 ] && record "guest/refused: interop_peer host --expect-unauthorized" PASS "$(tail -1 "$WORK/logs/interop-host.log" | cut -c1-70)" \
        || record "guest/refused: interop_peer host --expect-unauthorized" FAIL "$(tail -1 "$WORK/logs/interop-host.log" | cut -c1-70)"
      stop_server
      ;;
  esac
}

if [ "$SERVER" = sandbox ]; then
  step "the plugin component itself loads (Pumpkin's imports stubbed as traps) and declares its permissions"
  check "plugin: component loads, metadata and permissions" bash -c \
    "'$SANDBOX_HOST' --inspect '$PLUGIN_WASM' | tee /dev/stderr | grep -q 'permissions.*network.tcp.*network.udp.*network.loopback.*fs.write.data' && echo 'tcp, udp, loopback, fs.write.data'"
fi

step "rewrite-bridge conformance example (a checker for asset providers: it says nothing about the link)"
"$RBBIN/conformance" > "$WORK/logs/conformance.log" 2>&1; ccode=$?
record "info: rewrite-bridge conformance example ran" "$([ $ccode -eq 1 ] && echo PASS || echo FAIL)" "exit $ccode (1 is its normal answer on the toy provider)"

run_host_case plain ""
run_host_case token ", \"token_auth\": true"
run_guest_case plain ""
run_guest_case token ""
run_guest_case refused ""

# ---------------------------------------------------------------------------------------------
echo; echo "================ results ($SERVER) ================"
printf '%s\n' "${RESULTS[@]}"
echo "logs: $WORK/logs"
if [ $FAILED -eq 0 ]; then echo "ALL PASSED"; else echo "SOME FAILED"; fi
exit $FAILED
