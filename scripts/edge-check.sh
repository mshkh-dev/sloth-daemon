#!/usr/bin/env bash
# Edge cases for the IPC socket. DRY RUN only. Takes about 40 seconds.
#   A. a stale socket (left by a crash) is replaced on start
#   B. with [ipc] enabled = false the daemon still runs, with no socket
#   C. idle clients holding connections do not stall the control loop
# Run from the repo root as your normal user. Stop any running service first:
#   sudo systemctl stop sloth-daemon
set -u
BIN=$(ls -t ./target/release/sloth-daemon ./target/debug/sloth-daemon 2>/dev/null | head -1)
[ -n "$BIN" ] || BIN=/usr/bin/sloth-daemon
SOCK=/run/sloth/sloth.sock
WORK=$(mktemp -d)
CFG=$WORK/sloth.toml; CFG2=$WORK/sloth-noipc.toml
declare -a RESULTS=()
record() { RESULTS+=("$1|$2|$3"); }
die() { echo "ABORT: $*" >&2; exit 1; }
DPID=""; YES_PID=""; HOLD_PID=""
cleanup() { [ -n "$YES_PID" ] && kill "$YES_PID" 2>/dev/null; [ -n "$HOLD_PID" ] && sudo kill "$HOLD_PID" 2>/dev/null; [ -n "$DPID" ] && sudo kill -INT "$DPID" 2>/dev/null; sleep 1; sudo rm -f "$SOCK" 2>/dev/null; }
trap cleanup EXIT

ask() { sudo python3 - "$SOCK" "$1" <<'PY'
import socket, sys
s = socket.socket(socket.AF_UNIX); s.settimeout(5)
s.connect(sys.argv[1]); s.sendall((sys.argv[2] + "\n").encode())
buf = b""
while not buf.endswith(b"\n"):
    c = s.recv(65536)
    if not c: break
    buf += c
print(buf.decode().strip())
PY
}
start() {  # start <config> <logfile>
    rm -f "$WORK/pid"
    sudo bash -c 'echo $$ > "$1"; exec "$2" --config "$3"' _ "$WORK/pid" "$BIN" "$1" >"$2" 2>&1 &
    for _ in $(seq 1 20); do [ -s "$WORK/pid" ] && break; sleep 0.25; done
    DPID=$(cat "$WORK/pid" 2>/dev/null)
}
strip() { sed 's/\x1b\[[0-9;]*m//g' "$1"; }

command -v python3 >/dev/null || die "python3 not found"
[ -f sloth.toml ] || die "run from the repo root"
sudo -v || die "sudo is required"
sudo test -e "$SOCK" && die "$SOCK exists; stop any running daemon first"
echo "Binary: $BIN (built $(date -r "$BIN" '+%H:%M:%S'))"

cp sloth.toml "$CFG"
sed -i -e 's/^provider = .*/provider = "rules"/' -e 's/^throttle_candidates = .*/throttle_candidates = ["yes"]/' "$CFG"
grep -q '^dry_run = true' "$CFG" || die "dry_run must be true"
cp "$CFG" "$CFG2"; printf '\n[ipc]\nenabled = false\n' >> "$CFG2"
printf '\n[ipc]\noverrides_path = "%s/overrides.toml"\n' "$WORK" >> "$CFG"

# ---- A. stale socket ----
start "$CFG" "$WORK/a1.log"
for _ in $(seq 1 30); do sudo test -S "$SOCK" && break; sleep 0.5; done
sudo kill -9 "$DPID"; sleep 1
if sudo test -S "$SOCK"; then
    start "$CFG" "$WORK/a2.log"
    sleep 4
    R=$(ask '{"proto":1,"cmd":"status"}' 2>&1)
    echo "$R" | grep -q '"ok":true' && record "A. stale socket replaced on start" PASS "new daemon answers" || { record "A. stale socket replaced on start" FAIL "$R"; strip "$WORK/a2.log" | tail -5; }
else
    record "A. stale socket replaced on start" SKIP "kill -9 left no socket (daemon cleaned up?), nothing to replace"
fi

# ---- C. idle clients must not stall the control loop (daemon from A is running) ----
if sudo test -S "$SOCK" && [ -d "/proc/$DPID" ]; then
    yes > /dev/null & YES_PID=$!
    sleep 6
    N1=$(strip "$WORK/a2.log" | grep -c 'Policy decision')
    sudo python3 - "$SOCK" <<'PY' &
import socket, sys, time
conns = []
for _ in range(12):               # more than the 8-connection cap, all idle
    try:
        s = socket.socket(socket.AF_UNIX); s.settimeout(2); s.connect(sys.argv[1]); conns.append(s)
    except Exception: pass
time.sleep(9)
PY
    HOLD_PID=$!
    sleep 8
    N2=$(strip "$WORK/a2.log" | grep -c 'Policy decision')
    kill "$YES_PID" 2>/dev/null; YES_PID=""
    R=$(ask '{"proto":1,"cmd":"status"}' 2>&1)
    if [ "$((N2 - N1))" -ge 3 ]; then record "C. control loop keeps running with idle clients" PASS "$((N2 - N1)) decisions logged meanwhile"
    else record "C. control loop keeps running with idle clients" FAIL "only $((N2 - N1)) decisions in 8s ($N1 -> $N2)"; fi
    wait "$HOLD_PID" 2>/dev/null; HOLD_PID=""
    echo "$R" | grep -q '"ok":true' && record "C. server answers after idle clients leave" PASS "ok" || record "C. server answers after idle clients leave" FAIL "$R"
else
    record "C. control loop keeps running with idle clients" SKIP "daemon from A not running"
fi
sudo kill -INT "$DPID" 2>/dev/null; sleep 2; DPID=""; sudo rm -f "$SOCK"

# ---- B. IPC disabled ----
start "$CFG2" "$WORK/b.log"
sleep 5
if [ -d "/proc/$DPID" ] && strip "$WORK/b.log" | grep -q 'sampling loop'; then
    if sudo test -e "$SOCK"; then record "B. ipc.enabled=false: no socket, daemon runs" FAIL "socket exists"
    else record "B. ipc.enabled=false: no socket, daemon runs" PASS "running, no socket"; fi
    strip "$WORK/b.log" | grep -qi 'ipc' && record "B. disabled IPC is logged" PASS "$(strip "$WORK/b.log" | grep -i ipc | head -1 | cut -c1-110)" || record "B. disabled IPC is logged" FAIL "no IPC line in log"
else
    record "B. ipc.enabled=false: no socket, daemon runs" FAIL "daemon not running or never reached the sampling loop"; strip "$WORK/b.log" | tail -5
fi

echo; echo "================ EDGE CHECK RESULT ================"
for r in "${RESULTS[@]}"; do IFS='|' read -r n s d <<<"$r"; printf '%-4s  %-50s %s\n' "$s" "$n" "$d"; done
echo "==================================================="