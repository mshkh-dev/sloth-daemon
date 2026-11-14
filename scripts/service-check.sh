#!/usr/bin/env bash
# Phase 9 service check: run sloth-daemon under systemd with the repo's unit file.
# Verifies the IPC socket and overrides file work under ProtectSystem=strict.
# Run from the repo root as your normal user:
#   cargo build --release -p sloth-daemon && ./scripts/service-check.sh
#
# What it changes on this machine (all reversible):
#   /usr/local/bin/sloth-daemon, /etc/systemd/system/sloth-daemon.service (overwritten),
#   /etc/sloth-daemon/sloth.toml (only created if missing, never overwritten).
# It leaves the service STOPPED when it finishes (an earlier "enable" is not undone).
# DRY RUN: /etc/sloth-daemon/sloth.toml must have dry_run = true.

set -u
BIN=./target/release/sloth-daemon
UNIT=/etc/systemd/system/sloth-daemon.service
CONF=/etc/sloth-daemon/sloth.toml
SOCK=/run/sloth/sloth.sock
OVRFILE=/var/lib/sloth-daemon/overrides.toml
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
wait_socket() { for _ in $(seq 1 30); do sudo test -S "$SOCK" && return 0; sleep 0.5; done; return 1; }
cleanup() { sudo systemctl stop sloth-daemon 2>/dev/null; }
trap cleanup EXIT

[ -f sloth-daemon.service ] || die "run from the repo root"
[ -x "$BIN" ] || die "no release binary; run: cargo build --release -p sloth-daemon"
command -v python3 >/dev/null || die "python3 not found"
sudo -v || die "sudo is required"
sudo systemctl stop sloth-daemon 2>/dev/null
sudo test -e "$SOCK" && die "$SOCK exists; a daemon is running outside systemd. Stop it first."

NEWER=$(find sloth-daemon/src sloth-proto/src -name '*.rs' -newer "$BIN" 2>/dev/null | head -1)
[ -z "$NEWER" ] || die "release binary is older than the source ($NEWER). Run: cargo build --release -p sloth-daemon"
echo "Binary: $BIN (built $(date -r "$BIN" '+%H:%M:%S'))"
sudo install -m 0755 "$BIN" /usr/local/bin/sloth-daemon || die "install binary failed"
sudo mkdir -p /etc/sloth-daemon
sudo test -f "$CONF" || sudo cp sloth.toml "$CONF"
sudo grep -q '^dry_run = true' "$CONF" || die "$CONF must have dry_run = true for this check"
sudo cp sloth-daemon.service "$UNIT" && sudo systemctl daemon-reload

# 1. unit file parses cleanly
V=$(sudo systemd-analyze verify "$UNIT" 2>&1 | grep -iE 'sloth-daemon' | grep -iE 'unknown|invalid|ignoring|failed|not allowed' || true)
[ -z "$V" ] && record "unit file verifies cleanly" PASS "no warnings" || record "unit file verifies cleanly" FAIL "$V"

# 2. the unit file really has the 4 new directives and exactly two capabilities
for k in RuntimeDirectory=sloth RuntimeDirectoryMode=0750 StateDirectory=sloth-daemon StateDirectoryMode=0700; do
    grep -qx "$k" "$UNIT" 2>/dev/null || sudo grep -qx "$k" "$UNIT" || { record "unit has $k" FAIL "missing"; }
done
CAPS=$(systemctl show sloth-daemon -p CapabilityBoundingSet --value | tr ' ' '\n' | sort | tr '\n' ' ')
[ "$CAPS" = "cap_bpf cap_perfmon " ] && record "exactly two capabilities" PASS "$CAPS" || record "exactly two capabilities" FAIL "$CAPS"

# 3. start under systemd
sudo systemctl start sloth-daemon
sleep 1
if wait_socket; then record "service starts, socket appears" PASS "$(systemctl is-active sloth-daemon)"
else
    record "service starts, socket appears" FAIL "no socket"
    echo "---- journal ----"; sudo journalctl -u sloth-daemon --no-pager -n 25 | sed 's/\x1b\[[0-9;]*m//g'
fi

# 4. directory and socket modes
D=$(sudo stat -c '%a %U' /run/sloth 2>/dev/null); S=$(sudo stat -c '%a %U' "$SOCK" 2>/dev/null)
[ "$D" = "750 root" ] && [ "$S" = "600 root" ] && record "/run/sloth 750, socket 600" PASS "dir $D, socket $S" || record "/run/sloth 750, socket 600" FAIL "dir '$D', socket '$S'"
SD=$(sudo stat -c '%a %U' /var/lib/sloth-daemon 2>/dev/null)
[ "$SD" = "700 root" ] && record "/var/lib/sloth-daemon 700" PASS "$SD" || record "/var/lib/sloth-daemon 700" FAIL "'$SD'"

# 5. status works
R=$(ask '{"proto":1,"cmd":"status"}' 2>&1)
echo "$R" | grep -q '"ok":true' && record "status over the socket" PASS "$(echo "$R" | cut -c1-100)" || record "status over the socket" FAIL "$R"

# 6. an override can be written under ProtectSystem=strict, and survives a service restart
R=$(ask '{"proto":1,"cmd":"override","name":"svc-check-name","mode":"ban"}' 2>&1)
echo "$R" | grep -q '"ok":true' && record "override accepted under systemd" PASS "ok" || record "override accepted under systemd" FAIL "$R"
sudo test -f "$OVRFILE" && record "overrides file written to /var/lib" PASS "$(sudo stat -c '%a %U' "$OVRFILE")" || record "overrides file written to /var/lib" FAIL "missing"
sudo systemctl restart sloth-daemon
wait_socket
L=$(ask '{"proto":1,"cmd":"overrides"}' 2>&1)
echo "$L" | grep -q 'svc-check-name' && record "override survives service restart" PASS "persisted" || record "override survives service restart" FAIL "$L"
ask '{"proto":1,"cmd":"override","name":"svc-check-name","mode":"clear"}' >/dev/null 2>&1

# 7. journal: IPC started, no WARN/ERROR
J=$(sudo journalctl -u sloth-daemon --no-pager -b 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g')
echo "$J" | grep -qi 'IPC listening' && record "journal shows IPC listening" PASS "found" || record "journal shows IPC listening" FAIL "no 'IPC listening' line"
BAD=$(echo "$J" | grep -E ' (WARN|ERROR) ' | grep -v 'sloth-daemon\[.*Override rejected' | tail -5)
[ -z "$BAD" ] && record "no WARN/ERROR in journal" PASS "none" || record "no WARN/ERROR in journal" FAIL "$(echo "$BAD" | wc -l) line(s), shown below"

# 8. stop cleans up
sudo systemctl stop sloth-daemon; sleep 1
sudo test -e "$SOCK" && record "socket gone after stop" FAIL "still there" || record "socket gone after stop" PASS "gone"
ls -d /sys/fs/cgroup/sloth-daemon/pid-* >/dev/null 2>&1 && record "no leftover cgroups" FAIL "pid-* exist" || record "no leftover cgroups" PASS "none"

echo; echo "============== SERVICE CHECK RESULT =============="
for r in "${RESULTS[@]}"; do IFS='|' read -r n s d <<<"$r"; printf '%-4s  %-40s %s\n' "$s" "$n" "$d"; done
echo "=================================================="
[ -n "${BAD:-}" ] && { echo "---- WARN/ERROR lines ----"; echo "$BAD"; }
echo "Service left stopped. Boot-enable state is unchanged ($(systemctl is-enabled sloth-daemon 2>/dev/null)). To run it: sudo systemctl start sloth-daemon"