---

# v2 Addendum: Control Plane and Audit Log

v1 is finished and verified. v2 adds a **human-in-the-loop control plane**: the daemon stays headless and exposes a local Unix socket, and a separate terminal client shows *why* the daemon acted and lets the user permit or ban programs. It is **not** a system monitor: no CPU bars, RAM gauges or network charts (that is `btop`'s job).

## v2 Objectives
- Make every decision explainable: the last N policy decisions with their reasons are queryable.
- Let the user steer policy at runtime without editing `sloth.toml`.
- Keep the daemon lean and headless. No UI code or UI dependencies in the `sloth-daemon` binary.

## v2 Non-Goals
- No CPU, RAM, disk or network graphs.
- No remote or network access. The socket is a local Unix socket only.
- No new throttling powers. The socket can change **policy lists** only. It can never throttle, release or signal a specific PID.
- No new signals in the evidence brief (TTY focus, I/O) unless a later phase adds them for real. Do not show reasons the daemon cannot produce.

## v2 Architecture
```
sloth-daemon (headless, root)            sloth-ui (client, normal user or group member)
  control loop --> decision ring buffer      connects on demand, closes with no residue
  IPC server  <---- /run/sloth/sloth.sock ----> JSON lines request/response
  overrides.toml (Phase 9)
```
New workspace members: `sloth-proto` (library: request/response types, `serde` only) and `sloth-ui` (binary: `ratatui` + `crossterm`). `sloth-daemon` depends on `sloth-proto` but never on `ratatui` or `crossterm`.

## v2 Phases

### Phase 7: IPC Socket and Audit Log
- **Goal:** The daemon serves read-only status over a local socket, with a ring buffer of recent decisions. No UI yet.
- **Tasks:**
  1. Create the `sloth-proto` crate: `Request`, `Response`, `DecisionRecord { ts, pid, comm, cpu_percent, classification, confidence, action, reason }`, `CageInfo { pid, comm, quota_pct, throttled_secs, reason }`, `Status { version, mode, dry_run, uptime_secs, throttled_count, cap }`. Version the protocol with a `proto: 1` field.
  2. Add a bounded decision ring buffer (default 256 entries, `[ipc] history = 256`). Record every decision that is not a plain "not sustained long enough" skip.
  3. Add an IPC module serving JSON lines on `[ipc] socket_path` (default `/run/sloth/sloth.sock`). Requests: `status`, `decisions {limit}`, `cages`. Read-only snapshots only.
  4. Security and robustness as in `.agents/skills/ipc-and-tui.md`: directory and socket permissions, peer-credential check, request size cap, connection cap, stale-socket handling, cleanup on exit.
  5. If the socket cannot be created, log a warning and continue. The daemon must work exactly as in v1 without IPC.
  6. Add `ipc.enabled` (default `true`) to the config.
- **Exit check:** `cargo test` passes (protocol round-trip, ring buffer bound, oversize request rejected). With the daemon running, `echo '{"proto":1,"cmd":"status"}' | socat - UNIX-CONNECT:/run/sloth/sloth.sock` returns valid JSON; `decisions` shows the throttle of a `yes` loop with its reason; a socket not owned by the right user or mode is refused; after SIGINT the socket file is gone. v1 end-to-end script still passes.

### Phase 8: `sloth-ui` Read-Only Client
- **Goal:** A terminal UI that shows what the daemon is doing and why.
- **Tasks:**
  1. Create the `sloth-ui` crate (`ratatui`, `crossterm`, `tokio` or blocking std, `sloth-proto`).
  2. Two views, switched with Tab: **Active Cages** (PID, name, quota, time throttled, reason) and **Audit Stream** (newest decisions, scrollable). A one-line header shows mode, dry-run and throttled/cap.
  3. Poll the daemon every 1 s over a fresh connection. If the daemon is unreachable, show "daemon not reachable" and keep retrying. Never crash on a bad reply.
  4. Always restore the terminal on exit and on panic.
  5. Keys: `q` quit, `Tab` switch view, arrows scroll, `Enter` shows the full reason of the selected row.
- **Exit check:** `cargo build -p sloth-ui` succeeds, clippy clean. User runs `sloth-ui` while a `yes` loop is throttled and sees it in Active Cages and the decision in the Audit Stream. Stopping the daemon shows the "not reachable" state and the terminal is left clean after `q`. `sloth-daemon` still builds with no `ratatui` or `crossterm` in its dependency tree (`cargo tree -p sloth-daemon`).

### Phase 9: Runtime Overrides
- **Goal:** Let the user permit or ban programs by name from the UI, with the safety gate unchanged.
- **Tasks:**
  1. Add request `override {name, mode}` where `mode` is `permit` (never throttle this program name), `ban` (always treat as a throttle candidate), or `clear`.
  2. Persist overrides to `[ipc] overrides_path` (default `/etc/sloth-daemon/overrides.toml`) using temp file + atomic rename. Load on start. Reload live without restart.
  3. Validation: names only, max 64 chars, characters `A-Za-z0-9._+-`, max 200 entries. Reject everything else.
  4. **A ban only adds a candidate.** The policy gate still requires sustained load, a runaway/background verdict at the confidence threshold, the cap and the quota floor. A permit wins over a ban. Neither can touch the protected list, and no override can remove a protected name.
  5. Mutating requests require the peer to be root or the configured group. Read-only requests follow Phase 7 rules.
  6. UI: Audit Stream and Active Cages rows get `p` (permit), `b` (ban), `c` (clear), with a one-line confirmation message.
  7. In dry-run mode overrides are still saved, but still no cgroup writes happen.
- **Exit check:** Tests for validation, atomic write, permit-beats-ban, and "protected names cannot be overridden". User flow: ban `yes` in the UI, run a `yes` loop, see it throttled (non-dry-run run, user executes). Permit `yes`: it is never throttled. Restart the daemon: overrides survive. A malformed overrides file is ignored with a warning, not a crash.

### Phase 10 (optional, not started): Learned Evidence
Only after Phase 9 is in real use. Record how often the user permits or bans each program name and feed that count to the evidence brief as one more derived feature. It must never bypass the policy gate. Needs a fresh plan and approval before any work, and a decision on SQLite vs a plain file.

## v2 Definition of Done
Add these to the v1 checklist. Verify by running them.

### IPC and Safety
- [ ] The socket directory and file are not world-accessible, and an unauthorised peer is refused.
- [ ] Oversized or malformed requests are rejected without crashing or blocking the control loop.
- [ ] The socket file is removed on SIGINT/SIGTERM, and a stale one is replaced on start.
- [ ] If IPC fails to start, the daemon still runs as in v1.
- [ ] No request can throttle, release or signal a PID. Only policy lists change.
- [ ] `cargo tree -p sloth-daemon` shows no `ratatui` or `crossterm`.

### UI and Overrides
- [ ] `sloth-ui` shows active cages and the audit stream from a live daemon, and handles the daemon going away.
- [ ] The terminal is always restored on exit.
- [ ] Permit and ban work, survive a restart, and never override the protected list or the policy gate.

### Documentation
- [ ] README documents the socket path, permissions, `sloth-ui`, overrides and the new config keys.