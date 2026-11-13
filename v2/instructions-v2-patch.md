# Edits to instructions.md for v2 (user-approved; make exactly these edits, nothing else)

1. In section 1, item 3, add one bullet:
   - Writing or changing the IPC server, `sloth-proto`, `sloth-ui`, or anything touching the control socket or overrides file → `ipc-and-tui.md`

2. In section 3 (Tech Stack), add two rows. These are used only in the `sloth-ui` crate:
   | TUI (client crate only) | `ratatui` and `crossterm` |
   | Shared protocol crate | `sloth-proto` (depends on `serde` and `serde_json` only) |
   And add one sentence below the table: `sloth-daemon` must never depend on `ratatui` or `crossterm`.

3. In section 5, add a new subsection after 5.5:
   ### 5.6 The control socket
   - The socket is a local Unix socket only. Never bind a TCP or network listener.
   - The socket can change policy lists only. No request may throttle, release, signal or otherwise act on a specific PID.
   - Overrides can never remove or bypass the protected list or the policy gate.
   - Treat every request as untrusted input: cap its size, parse strictly, validate names, ignore unknown fields.
   - If IPC fails, log a warning and keep running. The daemon must not depend on IPC.

4. In section 7, add two rows:
   | `sloth-daemon/src/ipc.rs` | Socket server, request handling, decision ring buffer, overrides store | Cgroup writes, HTTP, UI code |
   | `sloth-ui/src/main.rs` | Terminal client and rendering | Any daemon logic or cgroup access |