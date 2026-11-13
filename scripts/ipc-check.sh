#!/usr/bin/env bash
# Phase 7 IPC check. Dry run only: nothing is written to cgroups.
# Run from the repo root as your normal user (it uses sudo for the daemon):
#   cargo build -p sloth-daemon && ./scripts/ipc-check.sh
# Uses python3 as the socket client, so socat is not needed.

set -u

BIN=./target/release/sloth-daemon
[ -x "$BIN" ] || BIN=./target/debug/sloth-daemon
SOCK=/run/sloth/sloth.sock
WAIT_SECS=${WAIT_SECS:-70}
WORK=$(mktemp -d)
CFG=$WORK/sloth.toml
LOG=$WORK/daemon.log
PIDFILE=$WORK/daemon.pid
YES_PID=""
DAEMON_PID=""
declare -a RESULTS=()

record() { RESULTS+=("$1|$2|$3"); }
die() { echo "ABORT: $*" >&2; exit 1; }

# ask <json> : send one JSON line to the socket as root, print the reply line
ask() {
    sudo python3 - "$SOCK" "$1" <<'PY'
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

stop_daemon() {
    if [ -n "$DAEMON_PID" ] && [ -d "/proc/$DAEMON_PID" ]; then
        sudo kill -INT "$DAEMON_PID" 2>/dev/null
        for _ in $(seq 1 20); do [ -d "/proc/$DAEMON_PID" ] || return 0; sleep 0.5; done
        return 1
    fi
    return 0
}
cleanup() { [ -n "$YES_PID" ] && kill "$YES_PID" 2>/dev/null; stop_daemon >/dev/null 2>&1; }
trap cleanup EXIT

[ -f sloth.toml ] || die "run from the repo root (sloth.toml not found)"
[ -x "$BIN" ] || die "no daemon binary; run: cargo build -p sloth-daemon"
command -v python3 >/dev/null || die "python3 not found"
sudo -v || die "sudo is required"
sudo test -e "$SOCK" && die "$SOCK already exists; stop any running daemon first"

cp sloth.toml "$CFG"
sed -i -e 's/^provider = .*/provider = "rules"/' \
       -e 's/^throttle_candidates = .*/throttle_candidates = ["yes"]/' "$CFG"
grep -q '^dry_run = true' "$CFG" || die "dry_run must be true in sloth.toml for this check"
grep -q '^throttle_candidates = \["yes"\]' "$CFG" || die "config edit failed"

sudo bash -c 'echo $$ > "$1"; exec "$2" --config "$3"' _ "$PIDFILE" "$BIN" "$CFG" >"$LOG" 2>&1 &
for _ in $(seq 1 20); do [ -s "$PIDFILE" ] && break; sleep 0.25; done
DAEMON_PID=$(cat "$PIDFILE" 2>/dev/null || true)
[ -n "$DAEMON_PID" ] || die "daemon did not start; see $LOG"
for _ in $(seq 1 30); do sudo test -S "$SOCK" && break; [ -d "/proc/$DAEMON_PID" ] || { cat "$LOG"; die "daemon exited early"; }; sleep 0.5; done

# no socket: stop here and show the daemon log instead of cascading failures
if ! sudo test -S "$SOCK"; then
    echo "ABORT: no socket at $SOCK after 15s. Daemon log (first 30 lines):" >&2
    sed 's/\x1b\[[0-9;]*m//g' "$LOG" | head -30 >&2
    echo "--- [ipc] section of your config:" >&2
    grep -n -A6 '^\[ipc\]' sloth.toml >&2 || echo "(no [ipc] section in sloth.toml)" >&2
    exit 1
fi

# 1. socket exists with safe permissions
if sudo test -S "$SOCK"; then
    MODE=$(sudo stat -c '%a %U' "$SOCK"); DMODE=$(sudo stat -c '%a' "$(dirname "$SOCK")")
    if [ "$MODE" = "600 root" ] && [ "$DMODE" = "750" ]; then record "socket created, safe modes" PASS "socket $MODE, dir $DMODE"
    else record "socket created, safe modes" FAIL "socket '$MODE', dir '$DMODE' (want '600 root' and 750)"; fi
else
    record "socket created, safe modes" FAIL "no socket at $SOCK"
fi

# 2. status works
R=$(ask '{"proto":1,"cmd":"status"}' 2>&1)
if echo "$R" | grep -q '"ok":true' && echo "$R" | grep -q '"proto":1'; then record "status returns ok" PASS "$R"
else record "status returns ok" FAIL "$R"; fi

# 3. wrong proto and garbage are rejected, daemon survives
R=$(ask '{"proto":99,"cmd":"status"}' 2>&1)
echo "$R" | grep -q '"ok":false' && record "wrong proto rejected" PASS "error reply" || record "wrong proto rejected" FAIL "$R"
ask 'not json at all' >/dev/null 2>&1
BIG=$(python3 -c 'print("{\"proto\":1,\"cmd\":\"status\",\"x\":\"" + "a"*6000 + "\"}")')
ask "$BIG" >/dev/null 2>&1
R=$(ask '{"proto":1,"cmd":"status"}' 2>&1)
echo "$R" | grep -q '"ok":true' && record "garbage and 6 KB request survived" PASS "still answering" || record "garbage and 6 KB request survived" FAIL "$R"

# 4. non-root is refused
if python3 - "$SOCK" <<'PY' 2>/dev/null
import socket, sys
s = socket.socket(socket.AF_UNIX); s.settimeout(3); s.connect(sys.argv[1])
PY
then record "non-root user refused" FAIL "normal user could connect"; else record "non-root user refused" PASS "connect denied"; fi

# 5. a decision for a busy 'yes' shows up
yes > /dev/null &
YES_PID=$!
echo "yes PID $YES_PID. Waiting up to ${WAIT_SECS}s for a decision in the audit log..."
START=$(date +%s); FOUND=0
while [ $(( $(date +%s) - START )) -lt "$WAIT_SECS" ]; do
    R=$(ask '{"proto":1,"cmd":"decisions","limit":50}' 2>&1)
    if echo "$R" | grep -q '"yes"'; then FOUND=1; break; fi
    sleep 3
done
kill "$YES_PID" 2>/dev/null; YES_PID=""
if [ "$FOUND" -eq 1 ]; then record "decisions lists the yes loop" PASS "after $(( $(date +%s) - START ))s"
else record "decisions lists the yes loop" FAIL "nothing for yes: $(echo "$R" | cut -c1-200)"; fi
SAMPLE=$(echo "$R" | cut -c1-400)

R=$(ask '{"proto":1,"cmd":"cages"}' 2>&1)
echo "$R" | grep -q '"ok":true' && record "cages answers" PASS "$(echo "$R" | cut -c1-120)" || record "cages answers" FAIL "$R"

# 6. dry run wrote nothing; clean exit removes the socket
ls -d /sys/fs/cgroup/sloth-daemon/pid-* >/dev/null 2>&1 && record "dry run wrote no cgroups" FAIL "pid-* dirs exist" || record "dry run wrote no cgroups" PASS "none"
if stop_daemon; then
    if sudo test -e "$SOCK"; then record "socket removed on SIGINT" FAIL "$SOCK still exists"; else record "socket removed on SIGINT" PASS "gone"; fi
else
    record "socket removed on SIGINT" FAIL "daemon did not exit"
fi

echo; echo "================ IPC CHECK RESULT ================"
for r in "${RESULTS[@]}"; do IFS='|' read -r n s d <<<"$r"; printf '%-4s  %-40s %s\n' "$s" "$n" "$d"; done
echo "=================================================="
echo "---- sample decisions reply ----"; echo "${SAMPLE:-none}"
echo "---- WARN/ERROR in daemon log ----"; sed 's/\x1b\[[0-9;]*m//g' "$LOG" | grep -E ' (WARN|ERROR) ' | tail -5
echo "Full log: $LOG"