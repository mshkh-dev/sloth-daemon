# Sloth Daemon

**Slow down runaway processes instead of killing them.**

Sloth Daemon is a small Linux daemon written in Rust. It watches CPU use with an eBPF probe, works out whether a busy process is a genuine runaway or legitimate heavy work (a compile, a video encode), and caps the runaways with cgroup v2 `cpu.max`. It never kills, signals or renices anything, and it lifts the cap when the process calms down.

> **Status: v0.2.0, tested on one machine** (Arch Linux, kernel 7.2.11). It runs in dry-run mode by default, so you can watch what it *would* do before letting it act. See [Tested on](#tested-on) and [Limitations](#limitations).

## Contents

- [Why it exists](#why-it-exists)
- [Install](#install)
- [Quick start (5 minutes, safe)](#quick-start-5-minutes-safe)
- [How it works](#how-it-works)
- [Safety model](#safety-model)
- [Configuration](#configuration)
- [Running as a service](#running-as-a-service)
- [sloth-ui and runtime overrides](#sloth-ui-and-runtime-overrides)
- [Verifying it works](#verifying-it-works)
- [Why not an existing tool?](#why-not-an-existing-tool)
- [Tested on](#tested-on) · [Limitations](#limitations) · [Troubleshooting](#troubleshooting) · [Contributing](#contributing) · [License](#license)

## Why it exists

A stuck script, a runaway browser helper or a forgotten build loop can pin a CPU core and make a machine crawl. The usual answers are blunt: kill it, hope `nice` helps, or wait. Sloth Daemon takes a gentler route: give the process a smaller slice of CPU, keep it alive, and hand the slice back once it calms down.

Everything runs on your machine. In the default mode there is no network access at all. In the optional LLM mode, the only network call is to your own local Ollama server.

## Install

### Arch Linux

An AUR package is **planned and not published yet**. A package can be built from this repository today:

```bash
git clone https://github.com/mshkh-dev/sloth-daemon.git
cd sloth-daemon/packaging/aur
makepkg -si
```

The package installs `sloth-daemon` and `sloth-ui` into `/usr/bin/`, the default config to `/etc/sloth-daemon/sloth.toml`, and the systemd unit to `/usr/lib/systemd/system/sloth-daemon.service`. **It does not enable or start the service.** To build, it needs `rustup` and `bpf-linker` (both in the official repos or installed by the PKGBUILD's makedepends).

### From source (any Linux)

You need Linux with **cgroup v2** and **BTF**, a pinned **nightly** Rust toolchain with `rust-src` (for the eBPF program), and `bpf-linker`.

```bash
stat -fc %T /sys/fs/cgroup        # expect: cgroup2fs
ls /sys/kernel/btf/vmlinux        # expect: the file exists
```

```bash
git clone https://github.com/mshkh-dev/sloth-daemon.git
cd sloth-daemon

# One-time toolchain setup. The repo pins its nightly in rust-toolchain.toml.
rustup toolchain install nightly-2026-10-23 --profile minimal --component rust-src
sudo pacman -S bpf-linker            # Arch. Elsewhere: cargo install bpf-linker

# Build as your normal user, not root.
cargo build --package sloth-ebpf --target bpfel-unknown-none -Z build-std=core --release
cargo build --release -p sloth-daemon -p sloth-ui
```

The binaries are in `target/release/`. (If you change the pinned nightly, change it in `rust-toolchain.toml` and in the PKGBUILD together.)

## Quick start (5 minutes, safe)

Dry-run is the default, so none of this writes to your cgroups. If you installed the Arch package, use `sloth-daemon` and `/etc/sloth-daemon/sloth.toml`. From a source build, use `sudo ./target/release/sloth-daemon --config sloth.toml` and `sudo ./target/release/sloth-ui`.

**1. Check your system:**

```bash
sudo sloth-daemon --check
```

**2. Start the daemon** (terminal 1):

```bash
sudo systemctl start sloth-daemon         # package install
# or from a source build:
sudo ./target/release/sloth-daemon --config sloth.toml --debug
```

**3. Open the terminal UI** (terminal 2):

```bash
sudo sloth-ui
```

**4. Make some noise** (terminal 3). In the default rules mode, **nothing is throttled until you say which programs may be**, so for this demo tell it that `yes` may be. In the UI you can do that by pressing `b` on a `yes` row once it appears, or add `throttle_candidates = ["yes"]` to the config. Then:

```bash
yes > /dev/null
```

After about 30 seconds of sustained load the daemon decides. In dry-run mode you will see lines like these in the log (from a real run):

```text
Policy decision pid=57451 comm=yes cpu_percent=104.5 classification=Background confidence=0.85 action=None dry_run=true reason=cpu not sustained long enough
Policy decision pid=57451 comm=yes cpu_percent=99.9 classification=Runaway confidence=0.85 action=Throttle { pid: 57451, quota_pct: 20 } dry_run=true reason=valid runaway workload justifies throttle
DRY-RUN: would throttle process to cgroup cpu.max pid=57451 comm="yes" quota_pct=20 dry_run=true
```

Press `Ctrl+C` on `yes` when you are done. When you are happy with what you see, set `dry_run = false` in the config, list the programs you are happy to slow down in `throttle_candidates`, and restart.

## How it works

1. **Observe.** An eBPF program (aya) on the `sched:sched_switch` tracepoint accumulates on-CPU time per task in a kernel map. No `/proc` polling.
2. **Sample.** The daemon reads that map every couple of seconds and turns the deltas into CPU percentages per process.
3. **Gather evidence.** A deterministic *evidence broker* builds a compact `ProcessBrief` for every process that stays busy: CPU trend and burstiness, how long it has been busy, program and parent category (compiler, encoder, browser helper, ...), thread count, memory bucket, session kind, system CPU pressure, and a preliminary rule verdict. **Command lines, environment variables, file paths and usernames are never included.**
4. **Classify.** The brief becomes a typed verdict (`essential`, `interactive`, `background`, `runaway` or `unknown`), by rules alone or with a local LLM (below).
5. **Decide.** A deterministic *policy gate* makes the final call. The AI is advisory: it can confirm or veto a throttle, never start one, and never override the protected list.
6. **Enforce.** Approved targets are moved into a child cgroup under `/sys/fs/cgroup/sloth-daemon/` with a `cpu.max` quota, then released later.

### Two classification modes

| Mode | Setting | Needs | What decides |
|:--|:--|:--|:--|
| Rules (default) | `provider = "rules"` | Nothing | The broker's preliminary verdict, from your `never_throttle` and `throttle_candidates` lists plus the process evidence |
| Local LLM | `provider = "ollama"` | Ollama and a small model | A local model reads the `ProcessBrief` and returns a typed verdict. It can confirm or lower a throttle, never raise one or override the protected list |

If Ollama is unreachable or returns bad output, the verdict is `unknown` and nothing is throttled, unless you set `fallback_to_rules = true`.

**Default behaviour:** in rules mode, nothing is throttled until you add program names to `throttle_candidates` (or ban them from `sloth-ui`). A busy process that is not on the list is classified `unknown` and left alone, so a fresh install is safe even with `dry_run = false`.

### LLM mode (optional)

```bash
sudo pacman -S ollama                    # Arch. Elsewhere: the installer from ollama.com
sudo systemctl enable --now ollama
ollama pull qwen2.5:0.5b                 # about 400 MB, the recommended minimum
curl -s http://localhost:11434/api/tags  # should list the model
```

Then set `provider = "ollama"`, `model = "qwen2.5:0.5b"` and `timeout_secs = 60` in the config. The first request after a cold start can be slow on old CPUs, so keep the timeout generous. Only the Ollama HTTP API is supported (not llama.cpp server or vLLM). To try it safely (dry run, one test process) run `./scripts/llm-check.sh`. Stop Ollama afterwards with `sudo systemctl stop ollama` to free memory.

## Safety model

- **Dry run by default.** Nothing is written to `/sys/fs/cgroup` unless `dry_run = false` is set explicitly.
- **No killing.** No signals, no `nice`, no CPU affinity changes. Only cgroup `cpu.max` and moving PIDs between cgroups.
- **Protected processes.** PID 1, the daemon itself, kernel threads, and core services (`systemd`, `sshd`, display servers, audio, `ollama`, ...) are never throttled. Nothing, including the AI and your overrides, can change that.
- **Limits on every throttle.** A minimum quota of 5% of one core, a cap on how many processes can be throttled at once, and an automatic expiry time.
- **No flapping.** A throttled process is released only after it stays calm (well below its quota) for a cool-down period, so the cap itself cannot trigger an immediate release.
- **Fail safe.** Errors, timeouts, malformed model output or missing data mean the process is left alone.
- **Clean exit and crash recovery.** On shutdown, `SIGINT` or `SIGTERM`, every throttle is released and the daemon's cgroups are removed. If the daemon is killed, leftovers are cleaned on the next start. Because the original cgroup is only remembered in memory, recovered processes are moved to the root cgroup.
- **Private by design.** Prompts contain only derived `ProcessBrief` facts.
- **A local control socket that cannot act on processes.** See [below](#sloth-ui-and-runtime-overrides).

## Configuration

Settings live in `sloth.toml` (`/etc/sloth-daemon/sloth.toml` for the package or service install). Every value has a default in code.

```toml
dry_run = true                 # keep true until you trust the decisions

[sampling]
interval_secs = 2              # how often CPU is measured
cpu_threshold_pct = 90.0       # a process must exceed this (% of one core)...
sustained_secs = 30            # ...for this long before it is considered

[throttle]
quota_pct = 20                 # CPU cap applied, in % of one core (minimum 5)
max_concurrent = 5             # most processes throttled at once
ttl_secs = 600                 # automatic release after this long
cool_down_samples = 5          # release after this many calm samples in a row
                               # (calm = CPU below 50% of the quota)

[ai]
provider = "rules"             # "rules" (no Ollama needed) or "ollama"
fallback_to_rules = false      # if true, fall back to the rules verdict when Ollama fails
ollama_url = "http://localhost:11434"
model = "qwen2.5:0.5b"         # recommended minimum model for LLM mode
timeout_secs = 60              # generous: a cold first request can be slow on old CPUs
min_confidence = 0.8           # verdicts below this are ignored
never_throttle = []            # extra names that are NEVER throttled
throttle_candidates = []       # rules mode: names it may throttle. Empty = nothing is throttled

[protected]
extra_names = []               # added to the built-in protected list

[ipc]
enabled = true                 # enable or disable the control socket
socket_path = "/run/sloth/sloth.sock"
history = 256                  # how many recent decisions are kept for the UI
group = ""                     # optional Unix group allowed to use the socket (default: root only)
overrides_path = "/var/lib/sloth-daemon/overrides.toml"
```

**Built-in always-protected names:** `systemd`, `systemd-*`, `sshd`, `dbus-daemon`, `Xorg`, `Xwayland`, `gnome-shell`, `kwin_*`, `pipewire`, `pulseaudio`, `NetworkManager`, `ollama`.

## Running as a service

**With the Arch package** the unit is already installed:

```bash
sudo systemctl enable --now sloth-daemon
journalctl -u sloth-daemon -f
```

**From a source build**, install the binary, config and unit by hand:

```bash
sudo cp target/release/sloth-daemon target/release/sloth-ui /usr/local/bin/
sudo mkdir -p /etc/sloth-daemon
sudo cp sloth.toml /etc/sloth-daemon/sloth.toml
sudo cp sloth-daemon.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now sloth-daemon
```

The unit runs as root with only `CAP_BPF` and `CAP_PERFMON`, and has been tested under systemd (throttle, release, `SIGTERM` shutdown, no leftovers). Cgroup operations need no extra capability because they are checked by file permissions. The unit creates `/run/sloth` and `/var/lib/sloth-daemon` through `RuntimeDirectory=` and `StateDirectory=`, because `ProtectSystem=strict` makes the rest of the filesystem read-only. If your system behaves differently, `journalctl -u sloth-daemon` will say which operation failed.

## sloth-ui and runtime overrides

The daemon is headless. It serves a local Unix socket at `/run/sloth/sloth.sock`, and `sloth-ui` is a separate terminal client that connects to it on demand. Nothing UI-related runs inside the daemon.

```bash
sudo sloth-ui
```

The socket directory is mode `0750` and the socket `0600`, both owned by `root` (systemd creates the directory when it runs as a service), so `sloth-ui` needs `sudo` unless you configure a socket `group`. If the socket cannot be created, or `[ipc] enabled = false`, the daemon runs normally without it.

**It is not a system monitor.** It has no CPU bars or process list. That is what `htop` and `btop` are for. `sloth-ui` explains what the *daemon* did and why, and lets you steer it:

| View | Shows |
|:--|:--|
| **Active Cages** | Processes currently throttled: quota, time throttled, time left, the reason |
| **Audit Stream** | Recent decisions, newest first, with the reason for each. Only processes that crossed the CPU threshold appear, which is why a normal idle process is never listed |

Keys: `Tab` switches view, `↑`/`↓` select, `Enter` shows the full reason, `q` quits, and on a selected process:

| Key | Override | Effect |
|:--|:--|:--|
| `p` | **permit** | Never throttle this program name (marked `[P]`). Beats `ban`. |
| `b` | **ban** | Treat this program as a throttle candidate (marked `[B]`). Sustained load, confidence, the cap and the quota floor all still apply. |
| `c` | **clear** | Remove the override. |

Protected processes cannot be overridden. Overrides take effect immediately, and survive restarts in `/var/lib/sloth-daemon/overrides.toml` (mode `0600`, written atomically).

**The socket cannot act on a process.** By design it can only change these policy lists. It can never throttle, release or signal a PID. Because you can only press a key on a process that has appeared in a view, you can also set an override for a program that is not busy right now over the socket directly. The socket is root-only, so these need `sudo`; `socat` is optional and `python3` works too:

```bash
# Status
sudo python3 -c "import socket; s=socket.socket(socket.AF_UNIX); s.connect('/run/sloth/sloth.sock'); s.sendall(b'{\"proto\":1,\"cmd\":\"status\"}\n'); print(s.recv(4096).decode())"

# Permit a program, with socat
echo '{"proto":1,"cmd":"override","name":"ffmpeg","mode":"permit"}' | sudo socat - UNIX-CONNECT:/run/sloth/sloth.sock

# List overrides
echo '{"proto":1,"cmd":"overrides"}' | sudo socat - UNIX-CONNECT:/run/sloth/sloth.sock
```

## Verifying it works

The scripts below need `sudo`, run in dry-run mode unless noted, and only ever involve a harmless `yes` loop. Build as your normal user first, because root usually has no Rust toolchain.

| Script | Checks |
|:--|:--|
| `./scripts/e2e-phase5.sh --build` | Throttle, CPU drop, release, cleanup (this one performs a real throttle of the `yes` process) |
| `sudo ./scripts/crash-recovery.sh` | `SIGKILL` the daemon, restart, check cleanup |
| `./scripts/ipc-check.sh` | Socket permissions, status, bad requests, non-root refusal, cleanup |
| `./scripts/overrides-check.sh` | Ban, permit, protected names, persistence, malformed file |
| `./scripts/edge-check.sh` | Stale socket, IPC disabled, idle clients |
| `./scripts/service-check.sh` | Runs under systemd with the repo's unit file |
| `./scripts/pkg-check.sh` | Inspects (and tests, once installed) the built Arch package |
| `./scripts/llm-check.sh` | Live Ollama check |

To check by hand:

```bash
yes > /dev/null &
pgrep -n yes                                           # note the PID
cat /sys/fs/cgroup/sloth-daemon/pid-<PID>/cpu.max      # e.g. "20000 100000"
top -b -n 2 -d 2 -p <PID> | tail -2                    # roughly 20% CPU
```

After stopping the daemon, confirm cleanup (`ls -d /sys/fs/cgroup/sloth-daemon/pid-*` should match nothing) and `kill <PID>` your test process.

## Why not an existing tool?

These tools are mature, widely used and well tested. Sloth Daemon is new and has been tested on one machine. Where they already do the job you need, use them:

| Tool | What it does | Where Sloth Daemon differs |
|:--|:--|:--|
| `systemd-run` / `CPUQuota=` | Applies a CPU quota to a unit or a transient scope you choose, with nothing extra to install | Sloth watches all processes and applies a time-limited cap automatically, then releases it |
| `cpulimit` | Limits a PID you name by alternating `SIGSTOP` and `SIGCONT` | Sloth uses cgroup CPU bandwidth, with no signals, so no stop-start stutter, and needs no PID from you |
| `ananicy` / `ananicy-cpp` | Applies `nice` and scheduler settings from community rules | `nice` only matters when cores are contended. A niced process can still use idle cores. Sloth caps maximum usage |
| `earlyoom` / `systemd-oomd` | Kill processes under memory pressure | Different problem (memory), and they kill. Sloth only throttles CPU |

What Sloth Daemon adds is automatic, reversible throttling with a hard protected list and caps, crash cleanup, a decision log that explains every action, an optional local model that can only veto, and a human override.

## Tested on

- Arch Linux, cgroup v2, kernel `7.2.11-arch1-1`: unit tests, live throttle and release, crash recovery.
- systemd service: tested live (throttle, release, `SIGTERM`, no leftovers).
- v0.2.0: control socket, `sloth-ui`, runtime overrides and the Arch package tested live on the same machine (see `scripts/`).
- Rules mode tested live. LLM mode tested live in dry-run with `qwen2.5:0.5b` and `gemma3:1b` on Ollama (about 10 s per verdict on an old laptop CPU) and against a mock Ollama server.

## Limitations

- CPU only. No memory, disk or network throttling.
- Linux with pure cgroup v2 only. No cgroup v1 or hybrid setups.
- Tested on one machine and one kernel. Please report what happens on yours.
- The LLM sees only derived facts and small local models are weak. The evidence brief, the policy gate, the protected list and dry-run mode are the safety net.
- `sloth-ui` needs `sudo` because the socket is root-only (a socket `group` can be configured).
- Building from source needs a pinned nightly Rust and `bpf-linker`, for the eBPF program.

## Troubleshooting

| Symptom | What to check |
|:--|:--|
| "cgroup v2 not found" | Run `stat -fc %T /sys/fs/cgroup`. It must print `cgroup2fs`. Hybrid or v1 systems are not supported. |
| eBPF fails to load | Run as root or grant the capabilities. Check `/sys/kernel/btf/vmlinux` exists and `ls /sys/kernel/tracing/events/sched/`. |
| `sloth-ui`: "run with sudo" or "daemon not running" | The socket is root-only: use `sudo sloth-ui`. Check `systemctl status sloth-daemon` and that `[ipc] enabled` is `true`. |
| Ollama warnings in LLM mode | Ollama is not running or the model is not pulled. Run `curl http://localhost:11434/api/tags`. Nothing is throttled in this state unless `fallback_to_rules = true`. |
| Nothing gets throttled | Check that the process stays above the threshold for the full sustained time, is not protected, is allowed by `throttle_candidates` or a `ban` override (an empty list means nothing is throttled in rules mode), and that `dry_run` is `false`. |
| `cargo` fails under sudo | Root has no default toolchain. Build as your normal user, then run only the binary or scripts with sudo. |
| Leftover `sloth-daemon` cgroups | Restart the daemon. It cleans up leftovers on startup. |
| Old `/usr/local/bin/sloth-daemon` or a unit in `/etc/systemd/system/` shadows the package | Remove them: the `/usr/local/bin` copy comes first in `PATH`, and a unit in `/etc` overrides the packaged one in `/usr/lib`. |

## Project structure

```
sloth-daemon/
├── sloth-ebpf/                                      # kernel probe (aya-ebpf, no_std)
├── sloth-daemon/                                    # daemon: broker.rs, policy.rs, ai_classifier.rs,
│                                                    # cgroup_manager.rs, ipc.rs, main.rs
├── sloth-proto/                                     # shared IPC protocol types
├── sloth-ui/                                        # terminal client (ratatui)
├── packaging/aur/                                   # PKGBUILD and install hooks for Arch
├── scripts/                                         # live checks (see "Verifying it works")
├── sloth.toml                                       # example configuration
├── sloth-daemon.service                             # systemd unit
├── blueprint.md, instructions.md, .agents/skills/   # the plan and rules used to build it with a coding agent
└── rust-toolchain.toml                              # pinned nightly for the eBPF crate
```

## Contributing

Issues and pull requests are welcome, especially reports from other distributions and kernels. Before sending a change, run `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`, and the relevant scripts in `scripts/`. The plan and the rules the project was built under are in `blueprint.md` and `instructions.md`; if you use a coding agent, point it at those first. Parts of the code were written with a coding agent, and every phase was verified by running it, as the scripts in `scripts/` show.

This daemon runs as root with eBPF, so please report a security issue privately (contact@mshkh.is-a.dev) and not in a public issue.

## License

Distributed under the **AGPL-3.0-only** license. See [`LICENSE`](LICENSE).