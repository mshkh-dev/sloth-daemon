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
