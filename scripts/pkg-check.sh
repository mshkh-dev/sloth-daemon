#!/usr/bin/env bash
# Phase 10 package check.
#   Static part : inspects the built .pkg.tar.zst (no root, nothing installed).
#   Runtime part: only if that exact package version is installed; runs it under systemd (dry run).
# Run from the repo root:
#   ./scripts/pkg-check.sh [path/to/sloth-daemon-0.2.0-1-x86_64.pkg.tar.zst]
# Install the package with `sudo pacman -U <file>` between the two parts, then run this again.

set -u
PKG=${1:-$(ls -t packaging/aur/sloth-daemon-[0-9]*.pkg.tar.zst 2>/dev/null | head -1)}   # never the -debug split package
SOCK=/run/sloth/sloth.sock
declare -a RESULTS=()
record() { RESULTS+=("$1|$2|$3"); }
die() { echo "ABORT: $*" >&2; exit 1; }
[ -n "$PKG" ] && [ -f "$PKG" ] || die "no package found; build it first or pass the path"
command -v bsdtar >/dev/null || die "bsdtar not found (pacman -S libarchive)"
echo "Package: $PKG"

INFO=$(bsdtar -xOf "$PKG" .PKGINFO)
FILES=$(bsdtar -tf "$PKG" | grep -v '^\.' )
pk() { echo "$INFO" | grep -E "^$1 = " | sed "s/^$1 = //"; }

# ---------------- static checks ----------------
VER=$(pk pkgver)    # .PKGINFO stores version and release together, e.g. 0.2.0-1
case "$(pk pkgname)" in sloth-daemon) ;; *) die "this is '$(pk pkgname)', not the main package; pass the right file";; esac
case "$VER" in 0.2.0-*) record "version is 0.2.0" PASS "$VER";; *) record "version is 0.2.0" FAIL "$VER";; esac
[ "$(pk license)" = "AGPL-3.0-only" ] && record "license AGPL-3.0-only" PASS "AGPL-3.0-only" || record "license AGPL-3.0-only" FAIL "$(pk license)"
LIC=$(bsdtar -xOf "$PKG" usr/share/licenses/sloth-daemon/LICENSE 2>/dev/null)
echo "$LIC" | grep -q 'GNU AFFERO GENERAL PUBLIC LICENSE' && record "shipped LICENSE text is AGPL" PASS "ok" || record "shipped LICENSE text is AGPL" FAIL "LICENSE file is not the AGPL text"
pk optdepend | grep -q '^ollama' && record "ollama is optional" PASS "optdepends" || record "ollama is optional" FAIL "not in optdepends"
pk depend | grep -q '^ollama' && record "ollama is not a hard dep" FAIL "in depends" || record "ollama is not a hard dep" PASS "ok"

OUT=$(echo "$FILES" | grep -vE '^(usr|etc)(/|$)' || true)
[ -z "$OUT" ] && record "only /usr and /etc touched" PASS "ok" || record "only /usr and /etc touched" FAIL "$OUT"

for f in usr/bin/sloth-daemon usr/bin/sloth-ui usr/lib/systemd/system/sloth-daemon.service \
         etc/sloth-daemon/sloth.toml usr/share/licenses/sloth-daemon/LICENSE; do
    echo "$FILES" | grep -qx "$f" && record "contains $f" PASS "" || record "contains $f" FAIL "missing"
done
echo "$FILES" | grep -q 'wants/' && record "service not enabled by package" FAIL "wants/ symlink shipped" || record "service not enabled by package" PASS "ok"
echo "$INFO" | grep -q '^backup = etc/sloth-daemon/sloth.toml' && record "config is in backup=()" PASS "ok" || record "config is in backup=()" FAIL "missing"

UNIT=$(bsdtar -xOf "$PKG" usr/lib/systemd/system/sloth-daemon.service 2>/dev/null)
ACTIVE=$(echo "$UNIT" | grep -vE '^[[:space:]]*#')    # ignore comment lines
if echo "$ACTIVE" | grep -q '^ExecStart=/usr/bin/sloth-daemon ' && ! echo "$ACTIVE" | grep -q '/usr/local'; then record "unit uses /usr/bin" PASS "ok"
else record "unit uses /usr/bin" FAIL "$(echo "$ACTIVE" | grep -E 'ExecStart|local')"; fi
echo "$UNIT" | grep -qE '^[[:space:]]*#.*/usr/local' && record "unit comments mention /usr/local" WARN "install-instructions comment shipped in the package (cosmetic)"
for k in RuntimeDirectory=sloth StateDirectory=sloth-daemon; do
    echo "$UNIT" | grep -qx "$k" && record "unit has $k" PASS "" || record "unit has $k" FAIL "missing"
done
CONF=$(bsdtar -xOf "$PKG" etc/sloth-daemon/sloth.toml 2>/dev/null)
echo "$CONF" | grep -q '^dry_run = true' && record "shipped config is dry_run = true" PASS "ok" || record "shipped config is dry_run = true" FAIL "check sloth.toml"

TMP=$(mktemp -d); bsdtar -xf "$PKG" -C "$TMP" usr/bin/sloth-daemon usr/bin/sloth-ui 2>/dev/null
for b in sloth-daemon sloth-ui; do
    M=$(stat -c '%a' "$TMP/usr/bin/$b" 2>/dev/null)
    [ "$M" = "755" ] && record "$b mode 755" PASS "" || record "$b mode 755" FAIL "'$M'"
done
"$TMP/usr/bin/sloth-daemon" --help >/dev/null 2>&1 && record "packaged daemon runs (--help)" PASS "ok" || record "packaged daemon runs (--help)" FAIL "does not run"
rm -rf "$TMP"

if command -v namcap >/dev/null; then
    N=$(namcap "$PKG" 2>&1)
    NE=$(echo "$N" | grep ' E: ' || true)
    # known-harmless warnings for Rust binaries: unused ld-linux, glibc implicitly satisfied
    NW=$(echo "$N" | grep ' W: ' | grep -vE "Unused shared library '/usr/lib64/ld-linux|glibc detected and implicitly satisfied" || true)
    if [ -n "$NE" ]; then record "namcap on package" FAIL "$(echo "$NE" | head -3 | tr '\n' ' ')"
    elif [ -n "$NW" ]; then record "namcap on package" WARN "$(echo "$NW" | head -3 | tr '\n' ' ')"
    else record "namcap on package" PASS "no errors (known-harmless warnings ignored)"; fi
else record "namcap on package" SKIP "not installed (sudo pacman -S namcap)"; fi

# ---------------- runtime checks (only if this version is installed) ----------------
INST=$(pacman -Q sloth-daemon 2>/dev/null | awk '{print $2}')
if [ "$INST" = "$VER" ]; then
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
    sudo -v || die "sudo is required for the runtime part"
    sudo systemctl stop sloth-daemon 2>/dev/null
    P=$(command -v sloth-daemon); [ "$P" = "/usr/bin/sloth-daemon" ] && record "PATH resolves to /usr/bin" PASS "$P" || record "PATH resolves to /usr/bin" FAIL "$P (stale copy in /usr/local/bin?)"
    U=$(systemctl show sloth-daemon -p FragmentPath --value); [ "$U" = "/usr/lib/systemd/system/sloth-daemon.service" ] && record "systemd uses the packaged unit" PASS "$U" || record "systemd uses the packaged unit" FAIL "$U"
    sudo grep -q '^dry_run = true' /etc/sloth-daemon/sloth.toml && DRY=1 || DRY=0
    if [ "$DRY" -eq 1 ]; then
        sudo /usr/bin/sloth-daemon --check >/dev/null 2>&1 && record "sloth-daemon --check" PASS "ok" || record "sloth-daemon --check" FAIL "see: sudo sloth-daemon --check"
        sudo systemctl start sloth-daemon; sleep 1
        for _ in $(seq 1 30); do sudo test -S "$SOCK" && break; sleep 0.5; done
        R=$(ask '{"proto":1,"cmd":"status"}' 2>&1)
        echo "$R" | grep -q '"ok":true' && record "service runs, status answers" PASS "ok" || record "service runs, status answers" FAIL "$R"
        R=$(ask '{"proto":1,"cmd":"override","name":"pkg-check-name","mode":"ban"}' 2>&1)
        echo "$R" | grep -q '"ok":true' && record "override works from the package" PASS "ok" || record "override works from the package" FAIL "$R"
        ask '{"proto":1,"cmd":"override","name":"pkg-check-name","mode":"clear"}' >/dev/null 2>&1
        J=$(sudo journalctl -u sloth-daemon --no-pager -b | sed 's/\x1b\[[0-9;]*m//g' | grep -E ' (WARN|ERROR) ' | grep -v 'Override rejected' | tail -3)
        [ -z "$J" ] && record "no WARN/ERROR in journal" PASS "none" || record "no WARN/ERROR in journal" FAIL "$J"
        sudo systemctl stop sloth-daemon; sleep 1
        sudo test -e "$SOCK" && record "socket gone after stop" FAIL "still there" || record "socket gone after stop" PASS "gone"
    else record "runtime checks" SKIP "/etc/sloth-daemon/sloth.toml is not dry_run = true"; fi
else
    record "runtime checks" SKIP "package not installed (sudo pacman -U $PKG), then rerun"
fi

echo; echo "================= PACKAGE CHECK RESULT ================="
for r in "${RESULTS[@]}"; do IFS='|' read -r n s d <<<"$r"; printf '%-4s  %-38s %s\n' "$s" "$n" "$d"; done
echo "========================================================="