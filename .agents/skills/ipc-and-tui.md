# Skill: IPC Socket and Terminal Client

Read this before writing or changing `sloth-daemon/src/ipc.rs`, `sloth-proto`, `sloth-ui`, or the overrides file.

## Principles
- The daemon is headless. The UI is a separate binary that connects on demand and leaves nothing behind when closed.
- The socket exposes **observation and policy lists**, never actions on PIDs.
- The daemon must work identically if IPC is disabled or fails to start.
- The control loop must never wait on a client.

## Protocol (`sloth-proto`)
- One JSON object per line (`\n` terminated). One request, one response, then the server may close or keep the connection.
- Every message has `"proto": 1`. An unknown `proto` gets an error response, not a crash.
- Requests: `status`, `decisions {limit}`, `cages` (Phase 7); `override {name, mode}` (Phase 9) with `mode` one of `permit`, `ban`, `clear`.
- Responses: `{"proto":1,"ok":true,"data":...}` or `{"proto":1,"ok":false,"error":"short message"}`. Error messages never contain paths or internal details.
- Use `serde` with `#[serde(tag = "cmd")]` style enums. Ignore unknown fields. Clamp `limit` to the ring buffer size.

## Server rules (Tokio, `tokio::net::UnixListener`)
- Create the parent directory (default `/run/sloth`) with mode `0750`, then bind. After binding, set the socket file mode to `0600` (or `0660` plus the configured group). Never leave the socket world-readable or world-writable, even briefly: set the process `umask` appropriately around `bind`, or create inside the restricted directory.
- If the socket path exists on start: check it is a socket, try to connect to it. If nothing answers, it is stale, so remove it and bind. If something answers, another daemon is running: log and skip IPC.
- Read the peer with `SO_PEERCRED` (`UnixStream::peer_cred()`). Phase 7: allow root and the configured group's members. Phase 9 mutating requests: root or the configured group only. Refuse everyone else with an error response and a debug log.
- Limits: request line max 4 KiB, a read timeout of a few seconds, max 8 concurrent connections, and one task per connection. Drop the connection on anything oversized or malformed.
- Share state with the control loop through a small `Arc<Mutex<..>>` or channel holding **snapshots** (the ring buffer, the cage list, status counters). Hold the lock briefly. Never hold a lock across `.await` or across an I/O call. Handlers copy data out and release it.
- Remove the socket file on clean shutdown (SIGINT/SIGTERM) and in the startup cleanup path.

## Decision ring buffer
- `VecDeque<DecisionRecord>` with a fixed cap (`[ipc] history`, default 256). Push drops the oldest.
- Record the decision outcome and its `reason` string exactly as the policy gate produced it. Do not invent reasons. If the evidence brief lacks a signal (TTY focus, I/O), the UI must not display it.
- Skip recording the repeated "not sustained long enough" skips, or the buffer fills with noise.

## Overrides (Phase 9)
- Names only. Validate: 1 to 64 chars, `[A-Za-z0-9._+-]` only. No slashes, spaces, NUL or control characters. Max 200 entries. Reject, don't sanitise.
- `permit` beats `ban`. A ban adds a throttle candidate only: it does not skip the sustained-load, confidence, cap or quota-floor checks. Neither mode can change the protected list.
- Persist with a temp file in the same directory plus `rename` (atomic). File mode `0600`, owner root. Load on start. On a parse error: log a warning, start with no overrides, and do not overwrite the bad file until a valid change is made.
- Apply changes live without a restart.

## Client rules (`sloth-ui`)
- Dependencies: `ratatui`, `crossterm`, `sloth-proto` (plus `tokio` only if needed; blocking `std` sockets are fine for a 1 s poll).
- Poll every 1 s with a fresh connection and a short timeout. If it fails, show "daemon not reachable" and retry. Never panic on a bad or unexpected reply.
- Terminal safety: enter raw mode and the alternate screen on start, and always restore both on exit and on panic (install a panic hook). Test it: after `q` the shell must be normal.
- Views: **Active Cages** and **Audit Stream** only. No CPU/RAM/network graphs. Keep rendering simple: one `Table` per view and a one-line header.
- Show the full reason text of the selected row on `Enter`. Truncate with an ellipsis in the table, never wrap.

## Do not
- Do not add a TCP, HTTP or websocket listener.
- Do not add any request that takes a PID and acts on it.
- Do not compile `ratatui` or `crossterm` into `sloth-daemon`.
- Do not block the sampling loop on a socket read, write or lock.
- Do not let a client-supplied string reach a file path, shell command or cgroup write.
- Do not log full request bodies.

## Testing hints
- Unit-test the protocol (round-trip, unknown fields, wrong `proto`), the ring buffer bound, name validation, and permit-beats-ban.
- Integration: bind in a temp directory in a test, send requests over a real `UnixStream`, assert on the replies, including an oversized line.
- Manual (user runs): `echo '{"proto":1,"cmd":"status"}' | socat - UNIX-CONNECT:/run/sloth/sloth.sock`