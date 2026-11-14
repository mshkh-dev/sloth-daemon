#!/usr/bin/env bash
# Phase 9 live check: ban, permit, protected names, persistence, malformed file.
# DRY RUN only: nothing is written to cgroups. Takes about 3 minutes.
# Run from the repo root as your normal user:
#   cargo build -p sloth-daemon && ./scripts/overrides-check.sh
# Needs python3. The overrides file lives in a temp dir, not /var/lib.

set -u
# use the NEWEST of the release and debug binaries, so a stale one is never tested
BIN=$(ls -t ./target/release/sloth-daemon ./target/debug/sloth-daemon 2>/dev/null | head -1)
SOCK=/run/sloth/sloth.sock
WORK=$(mktemp -d)
CFG=$WORK/sloth.toml
LOG=$WORK/daemon.log
OVR=$WORK/overrides.toml
PIDFILE=$WORK/daemon.pid
YES_PID=""; DAEMON_PID=""
declare -a RESULTS=()
record() { RESULTS+=("$1|$2|$3"); }
die() { echo "ABORT: $*" >&2; exit 1; }

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
ovr() { ask "{\"proto\":1,\"cmd\":\"override\",\"name\":\"$1\",\"mode\":\"$2\"}" 2>&1; }
ok() { echo "$1" | grep -q '"ok":true'; }
# rejected(): a real rejection, not "unknown command" (which means the binary has no override support)
rejected() { echo "$1" | grep -q '"ok":false' && ! echo "$1" | grep -q 'unknown command'; }

start_daemon() {
    rm -f "$PIDFILE"
    sudo bash -c 'echo $$ > "$1"; exec "$2" --config "$3"' _ "$PIDFILE" "$BIN" "$CFG" >>"$LOG" 2>&1 &
    for _ in $(seq 1 20); do [ -s "$PIDFILE" ] && break; sleep 0.25; done
    DAEMON_PID=$(cat "$PIDFILE" 2>/dev/null || true)
    for _ in $(seq 1 30); do sudo test -S "$SOCK" && return 0; sleep 0.5; done
    return 1
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

# run yes for N seconds, then return the decisions reply
yes_decisions() {
    yes > /dev/null & YES_PID=$!
    sleep "$1"
    local r; r=$(ask '{"proto":1,"cmd":"decisions","limit":100}' 2>&1)
    kill "$YES_PID" 2>/dev/null; YES_PID=""
    echo "$r"
}

[ -f sloth.toml ] || die "run from the repo root"
[ -x "$BIN" ] || die "no daemon binary; run: cargo build -p sloth-daemon"
command -v python3 >/dev/null || die "python3 not found"
NEWER=$(find sloth-daemon/src sloth-proto/src -name '*.rs' -newer "$BIN" 2>/dev/null | head -1)
[ -z "$NEWER" ] || die "binary $BIN is older than the source ($NEWER). Rebuild first: cargo build -p sloth-daemon"
echo "Testing binary: $BIN (built $(date -r "$BIN" '+%H:%M:%S'))"
sudo -v || die "sudo is required"
sudo test -e "$SOCK" && die "$SOCK exists; stop any running daemon first"

cp sloth.toml "$CFG"
sed -i -e 's/^provider = .*/provider = "rules"/' \
       -e 's/^throttle_candidates = .*/throttle_candidates = ["zzz-none"]/' "$CFG"
grep -q '^dry_run = true' "$CFG" || die "dry_run must be true in sloth.toml"
grep -q '^throttle_candidates = \["zzz-none"\]' "$CFG" || die "config edit failed"
printf '\n[ipc]\noverrides_path = "%s"\n' "$OVR" >> "$CFG"

start_daemon || { sed 's/\x1b\[[0-9;]*m//g' "$LOG" | head -30; die "no socket"; }
echo "Daemon up. yes is NOT a candidate in config, so only a ban can get it throttled."

# 1. validation and protected names
R=$(ovr systemd ban);  rejected "$R" && record "protected name rejected (ban systemd)" PASS "$(echo "$R" | cut -c1-90)" || record "protected name rejected (ban systemd)" FAIL "$R"
R=$(ovr sshd permit);  rejected "$R" && record "protected name rejected (permit sshd)" PASS "$(echo "$R" | cut -c1-90)" || record "protected name rejected (permit sshd)" FAIL "$R"
R=$(ovr '../etc' ban); rejected "$R" && record "bad name rejected (../etc)" PASS "$(echo "$R" | cut -c1-90)" || record "bad name rejected (../etc)" FAIL "$R"
LONG=$(python3 -c 'print("a"*65)')
R=$(ovr "$LONG" ban);  rejected "$R" && record "65-char name rejected" PASS "$(echo "$R" | cut -c1-90)" || record "65-char name rejected" FAIL "$R"

# 2. ban makes a non-candidate throttle-able (about 40s)
R=$(ovr yes ban)
if ok "$R"; then
    echo "yes banned; running yes for 42s..."
    D=$(yes_decisions 42)
    if echo "$D" | grep -q '"comm":"yes"' && echo "$D" | grep -q '"action":"Throttle"'; then record "ban -> yes gets a Throttle decision" PASS "throttle recorded"
    else record "ban -> yes gets a Throttle decision" FAIL "$(echo "$D" | cut -c1-250)"; fi
else
    record "ban -> yes gets a Throttle decision" FAIL "ban rejected: $R"
fi
L=$(ask '{"proto":1,"cmd":"overrides"}' 2>&1)
echo "$L" | grep -q 'yes' && record "overrides list shows yes" PASS "$(echo "$L" | cut -c1-120)" || record "overrides list shows yes" FAIL "$L"

# 3. permit beats ban (about 40s)
R=$(ovr yes permit)
if ok "$R"; then
    echo "yes now banned AND permitted; running yes for 42s..."
    PERMIT_T=$(date +%s)
    D=$(yes_decisions 42)
    # only look at decisions recorded AFTER the permit (older ones are from the ban phase)
    V=$(echo "$D" | python3 -I -c '
import json, sys
t = int(sys.argv[1])
d = json.loads(sys.stdin.read())["data"]
new = [r for r in d if r["comm"] == "yes" and r["timestamp_epoch_secs"] >= t]
thr = [r for r in new if r["action"] == "Throttle"]
perm = [r for r in new if "permitted by user" in r["reason"]]
print("%d new, %d throttle, %d permitted" % (len(new), len(thr), len(perm)))
sys.exit(0 if new and not thr and perm else 1)
' "$PERMIT_T" 2>&1)
    if [ $? -eq 0 ]; then record "permit beats ban" PASS "$V"
    else record "permit beats ban" FAIL "$V"; fi
else
    record "permit beats ban" FAIL "permit rejected: $R"
fi

# 4. file mode and persistence across restart
MODE=$(sudo stat -c '%a %U' "$OVR" 2>/dev/null || echo missing)
[ "$MODE" = "600 root" ] && record "overrides file mode" PASS "$MODE" || record "overrides file mode" FAIL "$MODE"
stop_daemon; sudo test -e "$SOCK" && record "restart: socket cleaned" FAIL "still there" || record "restart: socket cleaned" PASS "gone"
start_daemon || die "daemon did not come back"
L=$(ask '{"proto":1,"cmd":"overrides"}' 2>&1)
echo "$L" | grep -q 'yes' && record "overrides survive restart" PASS "$(echo "$L" | cut -c1-120)" || record "overrides survive restart" FAIL "$L"

# 5. clear
R=$(ovr yes clear); L=$(ask '{"proto":1,"cmd":"overrides"}' 2>&1)
if ok "$R" && ! echo "$L" | grep -q '"yes"'; then record "clear removes the override" PASS "gone from list"
else record "clear removes the override" FAIL "$R / $L"; fi

# 6. malformed file: daemon survives, file backed up
stop_daemon
sudo bash -c "echo 'this is {{ not toml' > '$OVR'"
start_daemon || die "daemon did not start with a malformed overrides file"
R=$(ask '{"proto":1,"cmd":"status"}' 2>&1)
ok "$R" && record "malformed file: daemon still starts" PASS "status ok" || record "malformed file: daemon still starts" FAIL "$R"
sudo test -e "$OVR.bad" && record "malformed file backed up to .bad" PASS "found" || record "malformed file backed up to .bad" FAIL "no $OVR.bad"
CLEAN=$(sed 's/\x1b\[[0-9;]*m//g' "$LOG")
echo "$CLEAN" | grep -E ' WARN ' | grep -qi 'overrid' && record "malformed file logged as WARN" PASS "warned" || record "malformed file logged as WARN" FAIL "no WARN about overrides"

# 7. dry run wrote nothing
ls -d /sys/fs/cgroup/sloth-daemon/pid-* >/dev/null 2>&1 && record "dry run wrote no cgroups" FAIL "pid-* exist" || record "dry run wrote no cgroups" PASS "none"
stop_daemon

echo; echo "============== OVERRIDES CHECK RESULT =============="
for r in "${RESULTS[@]}"; do IFS='|' read -r n s d <<<"$r"; printf '%-4s  %-42s %s\n' "$s" "$n" "$d"; done
echo "===================================================="
echo "---- INFO/WARN lines about overrides ----"
echo "$CLEAN" | grep -iE 'overrid' | tail -12
echo "Full log: $LOG"