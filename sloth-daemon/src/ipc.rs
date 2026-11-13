//! IPC Server and snapshot management for sloth-daemon (`sloth-daemon/src/ipc.rs`).
//!
//! Provides a headless local Unix socket server handling JSON-lines protocol:
//! `status`, `decisions {limit}`, `cages`, `override {name, mode}`, and `overrides`.

use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sloth_proto::{
    CageRecord, DecisionRecord, OverrideEntry, OverrideMode, Request, RequestEnvelope, Response,
    StatusData,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, info, warn};

pub const DEFAULT_SOCKET_PATH: &str = "/run/sloth/sloth.sock";
pub const DEFAULT_OVERRIDES_PATH: &str = "/var/lib/sloth-daemon/overrides.toml";
pub const DEFAULT_HISTORY_CAP: usize = 256;
pub const MAX_REQUEST_BYTES: usize = 4096;
pub const MAX_CONCURRENT_CLIENTS: usize = 8;
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
/// Maximum number of override entries.
pub const MAX_OVERRIDES: usize = 200;
/// Max length of an override name.
pub const MAX_NAME_LEN: usize = 64;

/// IPC configuration section `[ipc]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct IpcConfig {
    /// Enable or disable IPC control socket.
    pub enabled: bool,
    /// Path to Unix domain socket file.
    pub socket_path: PathBuf,
    /// Ring buffer capacity for policy decisions.
    pub history: usize,
    /// Optional Unix group allowed to access the socket.
    pub group: Option<String>,
    /// Path to the overrides persistence file.
    pub overrides_path: PathBuf,
}

impl Default for IpcConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            socket_path: PathBuf::from(DEFAULT_SOCKET_PATH),
            history: DEFAULT_HISTORY_CAP,
            group: None,
            overrides_path: PathBuf::from(DEFAULT_OVERRIDES_PATH),
        }
    }
}

// ============================================================================
// Override Store
// ============================================================================

/// On-disk TOML format for the overrides file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OverridesFile {
    #[serde(default)]
    overrides: Vec<OverrideEntry>,
}

/// Runtime overrides store: persists to and loads from a TOML file.
/// All I/O is done outside the lock; only the swap is done under the lock.
#[derive(Debug, Clone, Default)]
pub struct OverrideStore {
    permits: std::collections::HashSet<String>,
    bans: std::collections::HashSet<String>,
}

impl OverrideStore {
    /// Returns `true` if `comm` has a permit override.
    pub fn is_permitted(&self, comm: &str) -> bool {
        self.permits.contains(comm)
    }

    /// Returns `true` if `comm` has a ban override.
    pub fn is_banned(&self, comm: &str) -> bool {
        self.bans.contains(comm)
    }

    /// Total number of override entries.
    pub fn len(&self) -> usize {
        self.permits.len() + self.bans.len()
    }

    /// Snapshot of all current overrides as a vec.
    pub fn entries(&self) -> Vec<OverrideEntry> {
        let mut v: Vec<OverrideEntry> = self
            .permits
            .iter()
            .map(|n| OverrideEntry {
                name: n.clone(),
                mode: OverrideMode::Permit,
            })
            .chain(self.bans.iter().map(|n| OverrideEntry {
                name: n.clone(),
                mode: OverrideMode::Ban,
            }))
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    fn from_entries(entries: Vec<OverrideEntry>) -> Self {
        let mut s = Self::default();
        for e in entries {
            match e.mode {
                OverrideMode::Permit => {
                    s.permits.insert(e.name);
                }
                OverrideMode::Ban => {
                    s.bans.insert(e.name);
                }
                OverrideMode::Clear => {}
            }
        }
        s
    }
}

/// Validate a process name for use as an override key.
/// Returns `Err` with a short message if invalid.
pub fn validate_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return Err("name must be 1-64 characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
    {
        return Err("name contains invalid characters (allowed: A-Za-z0-9._+-)");
    }
    Ok(())
}

/// Load overrides from the TOML file.
/// On parse error: copies the file to `<path>.bad`, WARNs, returns empty store.
pub fn load_overrides(path: &Path) -> OverrideStore {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return OverrideStore::default(),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "Cannot read overrides file; starting with no overrides");
            return OverrideStore::default();
        }
    };

    match toml::from_str::<OverridesFile>(&content) {
        Ok(f) => OverrideStore::from_entries(f.overrides),
        Err(e) => {
            let bad_path = path.with_extension("toml.bad");
            warn!(
                path = %path.display(),
                bad_path = %bad_path.display(),
                error = %e,
                "Malformed overrides file; copying to .bad and starting with no overrides"
            );
            let _ = fs::copy(path, &bad_path);
            OverrideStore::default()
        }
    }
}

/// Persist overrides: write to temp file then atomic rename. File mode 0600.
/// Returns `Err` if write fails; caller must leave memory unchanged on error.
pub fn persist_overrides(store: &OverrideStore, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent).map_err(|e| format!("cannot create directory: {e}"))?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                .map_err(|e| format!("cannot set directory mode: {e}"))?;
        }
    }

    let file_data = OverridesFile {
        overrides: store.entries(),
    };
    let toml_str =
        toml::to_string_pretty(&file_data).map_err(|e| format!("serialization error: {e}"))?;

    let parent = path.parent().unwrap_or(Path::new("."));
    let tmp_path = parent.join(format!(".overrides.tmp.{}", std::process::id()));

    fs::write(&tmp_path, toml_str.as_bytes())
        .map_err(|e| format!("write temp file failed: {e}"))?;
    fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod temp file failed: {e}"))?;
    fs::rename(&tmp_path, path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        format!("atomic rename failed: {e}")
    })?;
    Ok(())
}

// ============================================================================
// IPC State
// ============================================================================

/// Thread-safe shared snapshot data accessed by IPC request handlers.
#[derive(Debug)]
pub struct IpcState {
    pub version: String,
    pub dry_run: bool,
    pub start_time: std::time::Instant,
    pub decisions: VecDeque<DecisionRecord>,
    pub max_history: usize,
    pub cages: Vec<CageRecord>,
    pub mode: String,
    pub cap: usize,
    /// Current runtime override store.
    pub overrides: OverrideStore,
    /// Path to the overrides persistence file.
    pub overrides_path: PathBuf,
    /// Combined protected names: BUILTIN_PROTECTED_NAMES + extra_names + never_throttle.
    /// Used by the Override handler to reject attempts to override protected processes.
    pub protected_names: Vec<String>,
}

impl IpcState {
    pub fn new(
        version: String,
        dry_run: bool,
        max_history: usize,
        mode: String,
        cap: usize,
    ) -> Self {
        Self {
            version,
            dry_run,
            start_time: std::time::Instant::now(),
            decisions: VecDeque::with_capacity(max_history.min(4096)),
            max_history: if max_history == 0 {
                DEFAULT_HISTORY_CAP
            } else {
                max_history
            },
            cages: Vec::new(),
            mode,
            cap,
            overrides: OverrideStore::default(),
            overrides_path: PathBuf::from(DEFAULT_OVERRIDES_PATH),
            protected_names: Vec::new(),
        }
    }

    /// Load overrides from the configured path; called after construction.
    pub fn load_overrides_from_disk(&mut self) {
        self.overrides = load_overrides(&self.overrides_path.clone());
    }

    /// Set the combined protected names list (BUILTIN + extra + never_throttle).
    /// Called once at startup from main before the IPC server processes any requests.
    pub fn set_protected_names(&mut self, names: Vec<String>) {
        self.protected_names = names;
    }

    /// Return a snapshot of the current overrides store (cloned, for read-only access outside the lock).
    pub fn overrides_snapshot(&self) -> OverrideStore {
        self.overrides.clone()
    }

    /// Push a decision record, dropping the oldest if capacity is exceeded.
    /// Skips recording repeated "not sustained long enough" decisions to avoid filling buffer with noise.
    pub fn record_decision(&mut self, record: DecisionRecord) {
        if record.reason.contains("not sustained") {
            return;
        }
        if self.decisions.len() >= self.max_history {
            self.decisions.pop_front();
        }
        self.decisions.push_back(record);
    }

    /// Update active cage snapshots.
    pub fn update_cages(&mut self, cages: Vec<CageRecord>) {
        self.cages = cages;
    }
}

/// Injectable peer credential authorizer.
pub type AuthFn = Arc<dyn Fn(u32, u32) -> bool + Send + Sync>;

/// Resolves a group name to GID using /etc/group without external crates.
pub fn lookup_gid_by_name(group_name: &str) -> Option<u32> {
    let content = fs::read_to_string("/etc/group").ok()?;
    for line in content.lines() {
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() >= 3 && parts[0] == group_name {
            if let Ok(gid) = parts[2].parse::<u32>() {
                return Some(gid);
            }
        }
    }
    None
}

/// Default peer authorizer: root (UID 0) or member of the configured group.
pub fn default_authorizer(allowed_gid: Option<u32>) -> AuthFn {
    Arc::new(move |peer_uid: u32, peer_gid: u32| {
        if peer_uid == 0 {
            return true;
        }
        if let Some(gid) = allowed_gid {
            if peer_gid == gid {
                return true;
            }
        }
        false
    })
}

/// Safe preparation and binding of the Unix domain socket.
///
/// Ensures parent directory mode 0750, verifies existing socket is not active/stale,
/// sets umask, binds, and sets socket file permissions (0600 or 0660 + group).
pub fn bind_unix_listener(socket_path: &Path, group: Option<&str>) -> anyhow::Result<UnixListener> {
    if socket_path == Path::new(DEFAULT_SOCKET_PATH) {
        if let Some(parent) = socket_path.parent() {
            if parent.exists() {
                // Check parent permissions: ensure not group or world writable
                let metadata = fs::metadata(parent)?;
                let mode = metadata.permissions().mode();
                if (mode & 0o022) != 0 {
                    return Err(anyhow::anyhow!(
                        "Socket parent directory {} has insecure permissions {:o} (group or world writable)",
                        parent.display(),
                        mode & 0o777
                    ));
                }
            } else {
                fs::create_dir_all(parent)?;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o750))?;
            }
        }
    } else if let Some(parent) = socket_path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)?;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }

    // Resolve group GID if specified
    let gid_opt = if let Some(grp) = group {
        match lookup_gid_by_name(grp) {
            Some(g) => Some(g),
            None => {
                warn!("Configured IPC group '{}' not found in /etc/group", grp);
                None
            }
        }
    } else {
        None
    };

    // Stale socket cleanup
    if socket_path.exists() {
        let meta = fs::symlink_metadata(socket_path)?;
        use std::os::unix::fs::FileTypeExt;
        if !meta.file_type().is_socket() {
            return Err(anyhow::anyhow!(
                "Path {} exists and is not a socket",
                socket_path.display()
            ));
        }

        // Try to connect to check if another daemon is actively running
        match std::os::unix::net::UnixStream::connect(socket_path) {
            Ok(_) => {
                return Err(anyhow::anyhow!(
                    "Another daemon is already active on {}",
                    socket_path.display()
                ));
            }
            Err(_) => {
                info!("Removing stale socket at {}", socket_path.display());
                fs::remove_file(socket_path)?;
            }
        }
    }

    // Set process umask to 0077 during bind to guarantee socket is never world-accessible
    let prev_umask = unsafe { libc_umask(0o077) };
    let listener_result = std::os::unix::net::UnixListener::bind(socket_path);
    let _ = unsafe { libc_umask(prev_umask) };
    let std_listener = listener_result?;

    // Configure non-blocking for Tokio
    std_listener.set_nonblocking(true)?;

    // Set socket permissions: 0660 if group configured, otherwise 0600
    if let Some(gid) = gid_opt {
        use std::os::unix::fs::chown;
        if let Err(e) = chown(socket_path, None, Some(gid)) {
            warn!("Failed to chown socket to group GID {gid}: {e}");
        }
        let _ = fs::set_permissions(socket_path, fs::Permissions::from_mode(0o660));
    } else {
        let _ = fs::set_permissions(socket_path, fs::Permissions::from_mode(0o600));
    }

    let tokio_listener = UnixListener::from_std(std_listener)?;
    Ok(tokio_listener)
}

extern "C" {
    fn umask(mask: u32) -> u32;
    fn geteuid() -> u32;
}

#[inline]
pub fn current_euid() -> u32 {
    unsafe { geteuid() }
}

#[inline]
unsafe fn libc_umask(mask: u32) -> u32 {
    umask(mask)
}

/// Server handle and shared state returned when IPC starts.
pub type IpcServer = (tokio::task::JoinHandle<()>, Arc<Mutex<IpcState>>);

/// Start IPC server from configuration with a custom authorizer (e.g. for testing).
/// Returns the join handle and shared state if enabled and successfully bound.
pub fn start_ipc_server(
    config: &IpcConfig,
    dry_run: bool,
    version: &str,
    mode: &str,
    cap: usize,
    authorizer: AuthFn,
) -> Result<Option<IpcServer>, anyhow::Error> {
    if !config.enabled {
        warn!(
            socket = %config.socket_path.display(),
            reason = "ipc.enabled is false",
            "IPC control socket disabled by configuration (ipc.enabled=false)"
        );
        return Ok(None);
    }

    let state = Arc::new(Mutex::new(IpcState::new(
        version.to_string(),
        dry_run,
        config.history,
        mode.to_string(),
        cap,
    )));

    // Load overrides from disk before starting the server.
    {
        let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
        guard.overrides_path = config.overrides_path.clone();
        guard.load_overrides_from_disk();
    }

    match bind_unix_listener(&config.socket_path, config.group.as_deref()) {
        Ok(listener) => {
            info!(
                socket = %config.socket_path.display(),
                group = ?config.group,
                "IPC listening on {}",
                config.socket_path.display()
            );
            let handle = tokio::spawn(run_ipc_server(listener, state.clone(), authorizer));
            Ok(Some((handle, state)))
        }
        Err(err) => {
            warn!(
                socket = %config.socket_path.display(),
                reason = %err,
                "IPC bind failed; daemon continuing without IPC: {err}"
            );
            Err(err)
        }
    }
}

/// Runs the IPC listener loop accepting connections up to MAX_CONCURRENT_CLIENTS.
pub async fn run_ipc_server(
    listener: UnixListener,
    state: Arc<Mutex<IpcState>>,
    authorizer: AuthFn,
) {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_CLIENTS));

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                debug!("IPC accept error: {e}");
                break;
            }
        };

        let permit = match semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                warn!("IPC server reached concurrency limit ({MAX_CONCURRENT_CLIENTS}); dropping connection");
                continue;
            }
        };

        let state_clone = state.clone();
        let auth_clone = authorizer.clone();

        tokio::spawn(async move {
            let _permit = permit;
            handle_connection(stream, state_clone, auth_clone).await;
        });
    }
}

/// Handles a single client connection.
async fn handle_connection(
    mut stream: UnixStream,
    state: Arc<Mutex<IpcState>>,
    authorizer: AuthFn,
) {
    // Peer credentials verification
    let cred = match stream.peer_cred() {
        Ok(c) => c,
        Err(e) => {
            debug!("Failed to read SO_PEERCRED from peer: {e}");
            return;
        }
    };

    let peer_uid = cred.uid();
    let peer_gid = cred.gid();
    if !authorizer(peer_uid, peer_gid) {
        debug!(peer_uid, peer_gid, "Peer unauthorized");
        let resp: Response<()> = Response::err("unauthorized");
        if let Ok(line) = serde_json::to_string(&resp) {
            let _ = stream.write_all(format!("{line}\n").as_bytes()).await;
        }
        return;
    }

    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();

    // Read with timeout and max size limit
    let read_result = tokio::time::timeout(REQUEST_TIMEOUT, async {
        // Read up to 4 KiB max
        let mut total_read = 0;
        loop {
            let mut chunk = String::new();
            let n = buf_reader.read_line(&mut chunk).await?;
            if n == 0 {
                break; // EOF
            }
            total_read += n;
            if total_read > MAX_REQUEST_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "request exceeds 4 KiB limit",
                ));
            }
            line.push_str(&chunk);
            if line.ends_with('\n') {
                break;
            }
        }
        Ok::<(), std::io::Error>(())
    })
    .await;

    if read_result.is_err() {
        debug!("IPC read timed out or failed");
        return;
    }
    if let Ok(Err(e)) = read_result {
        debug!("IPC read error: {e}");
        let resp: Response<()> = Response::err("request too large or invalid");
        if let Ok(s) = serde_json::to_string(&resp) {
            let _ = writer.write_all(format!("{s}\n").as_bytes()).await;
        }
        return;
    }

    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }

    // Parse request JSON
    let envelope: RequestEnvelope = match serde_json::from_str(trimmed) {
        Ok(env) => env,
        Err(_) => {
            let resp: Response<()> = Response::err("malformed json request");
            if let Ok(s) = serde_json::to_string(&resp) {
                let _ = writer.write_all(format!("{s}\n").as_bytes()).await;
            }
            return;
        }
    };

    // Protocol version check
    if envelope.proto != sloth_proto::PROTO_VERSION {
        let resp: Response<()> = Response::err("unsupported proto version");
        if let Ok(s) = serde_json::to_string(&resp) {
            let _ = writer.write_all(format!("{s}\n").as_bytes()).await;
        }
        return;
    }

    // Dispatch request while holding the lock briefly
    let response_str = match envelope.request {
        Request::Status => {
            let (version, dry_run, uptime_secs, decisions_recorded, active_cages, mode, cap) = {
                let guard = state.lock().unwrap_or_else(|e| e.into_inner());
                (
                    guard.version.clone(),
                    guard.dry_run,
                    guard.start_time.elapsed().as_secs(),
                    guard.decisions.len(),
                    guard.cages.len(),
                    guard.mode.clone(),
                    guard.cap,
                )
            };
            let resp = Response::ok(StatusData {
                version,
                dry_run,
                active_cages,
                uptime_secs,
                decisions_recorded,
                mode,
                throttled_count: active_cages,
                cap,
            });
            serde_json::to_string(&resp)
        }
        Request::Decisions { limit } => {
            let records: Vec<DecisionRecord> = {
                let guard = state.lock().unwrap_or_else(|e| e.into_inner());
                let max_take = limit
                    .unwrap_or(guard.decisions.len())
                    .min(guard.decisions.len());
                // Return latest N decisions
                guard
                    .decisions
                    .iter()
                    .rev()
                    .take(max_take)
                    .cloned()
                    .collect()
            };
            let resp = Response::ok(records);
            serde_json::to_string(&resp)
        }
        Request::Cages => {
            let cages: Vec<CageRecord> = {
                let guard = state.lock().unwrap_or_else(|e| e.into_inner());
                guard.cages.clone()
            };
            let resp = Response::ok(cages);
            serde_json::to_string(&resp)
        }
        // Read-only: list all current overrides. Phase 7 auth rules apply (already checked above).
        Request::Overrides => {
            let entries: Vec<OverrideEntry> = {
                let guard = state.lock().unwrap_or_else(|e| e.into_inner());
                guard.overrides.entries()
            };
            let resp = Response::ok(entries);
            serde_json::to_string(&resp)
        }
        // Mutating: requires root or configured group.
        Request::Override { name, mode } => {
            // Phase 9 mutating requests require root (uid 0) or the configured group.
            // The outer authorizer already admitted only root/group, so re-use it here
            // rather than hard-coding uid==0.
            if !authorizer(peer_uid, peer_gid) {
                warn!(peer_uid, name = %name, "Override request rejected: peer not authorized");
                let resp: Response<()> = Response::err("override requires root or configured group");
                serde_json::to_string(&resp)
            } else {
                handle_override_request(state, name, mode).await
            }
        }
        Request::Unknown => {
            let resp: Response<()> = Response::err("unknown command");
            serde_json::to_string(&resp)
        }
    };

    if let Ok(s) = response_str {
        let _ = writer.write_all(format!("{s}\n").as_bytes()).await;
    }
}

/// Handle the `override {name, mode}` mutating request.
/// Persist first (temp + rename), then swap in-memory. On write failure, return error
/// and leave the in-memory store unchanged.
async fn handle_override_request(
    state: Arc<Mutex<IpcState>>,
    name: String,
    mode: OverrideMode,
) -> Result<String, serde_json::Error> {
    use crate::cgroup_manager::matches_protected_name;

    // 1. Validate name.
    if let Err(e) = validate_name(&name) {
        warn!(name = %name, reason = %e, "Override rejected: invalid name");
        return serde_json::to_string(&Response::<()>::err(e));
    }

    // 2. Protected names are rejected for both permit and ban before any other check.
    //    Use the combined protected_names list from IpcState (BUILTIN + extra + never_throttle).
    let (all_protected, overrides_path, current_overrides) = {
        let guard = state.lock().unwrap_or_else(|e| e.into_inner());
        (
            guard.protected_names.clone(),
            guard.overrides_path.clone(),
            guard.overrides.clone(),
        )
    };
    if matches_protected_name(&name, &all_protected) {
        warn!(name = %name, "Override rejected: name is in the protected list");
        return serde_json::to_string(&Response::<()>::err(
            "protected names cannot be overridden",
        ));
    }

    // 3. Apply the requested change to the snapshot.
    let mut new_store = current_overrides;
    match &mode {
        OverrideMode::Permit => {
            new_store.bans.remove(&name);
            new_store.permits.insert(name.clone());
        }
        OverrideMode::Ban => {
            new_store.permits.remove(&name);
            new_store.bans.insert(name.clone());
        }
        OverrideMode::Clear => {
            new_store.permits.remove(&name);
            new_store.bans.remove(&name);
        }
    }

    // 4. Check cap (after applying, if adding a new entry).
    if new_store.len() > MAX_OVERRIDES {
        warn!(name = %name, "Override rejected: would exceed max 200 entries");
        return serde_json::to_string(&Response::<()>::err(
            "maximum override entries (200) reached",
        ));
    }

    // 5. Persist FIRST (outside the lock, all I/O here).
    if let Err(e) = persist_overrides(&new_store, &overrides_path) {
        warn!(name = %name, error = %e, "Override persist failed; leaving memory unchanged");
        return serde_json::to_string(&Response::<()>::err("failed to persist override"));
    }

    // 6. Swap in-memory only after successful write.
    {
        let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
        guard.overrides = new_store;
    }

    info!(name = %name, mode = ?mode, "Override applied");
    serde_json::to_string(&Response::ok(format!("override applied: {name} = {mode:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncWriteExt;

    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn get_test_socket_path() -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("sloth_ipc_test_{pid}_{id}.sock"))
    }

    #[test]
    fn test_decision_ring_buffer_cap_and_filter() {
        let mut state = IpcState::new("0.1.0".to_string(), true, 3, "rules".to_string(), 5);
        assert_eq!(state.max_history, 3);

        // Filter out "not sustained"
        state.record_decision(DecisionRecord {
            timestamp_epoch_secs: 100,
            pid: 1234,
            comm: "hog".into(),
            cpu_percent: 95.0,
            action: "None".into(),
            reason: "CPU not sustained long enough".into(),
            confidence: None,
        });
        assert_eq!(state.decisions.len(), 0);

        // Push 4 valid decisions
        for i in 1..=4 {
            state.record_decision(DecisionRecord {
                timestamp_epoch_secs: 100 + i,
                pid: 1000 + i as u32,
                comm: format!("proc{i}"),
                cpu_percent: 80.0,
                action: "Throttle".into(),
                reason: "High CPU usage".into(),
                confidence: Some(0.9),
            });
        }
        // Ring buffer holds at most 3
        assert_eq!(state.decisions.len(), 3);
        assert_eq!(state.decisions[0].pid, 1002);
        assert_eq!(state.decisions[2].pid, 1004);
    }

    #[tokio::test]
    async fn test_ipc_server_status_and_cages() {
        let sock_path = get_test_socket_path();
        let _ = fs::remove_file(&sock_path);

        let listener = bind_unix_listener(&sock_path, None).expect("bind listener");
        let state = Arc::new(Mutex::new(IpcState::new(
            "0.1.0".to_string(),
            true,
            10,
            "rules".to_string(),
            5,
        )));

        // Populate a cage record
        {
            let mut guard = state.lock().unwrap();
            guard.update_cages(vec![CageRecord {
                pid: 4321,
                comm: "busy_worker".into(),
                quota_pct: 20,
                throttled_secs: 15,
                ttl_remaining_secs: 585,
                reason: "Matched throttle candidate".into(),
            }]);
        }

        let my_uid = current_euid();
        let authorizer: AuthFn = Arc::new(move |peer_uid, _| peer_uid == my_uid);

        let srv_handle = tokio::spawn(run_ipc_server(listener, state, authorizer));

        // Connect client
        let mut client = UnixStream::connect(&sock_path).await.expect("connect");

        // Send status request
        client
            .write_all(b"{\"proto\":1,\"cmd\":\"status\"}\n")
            .await
            .unwrap();
        let mut reader = BufReader::new(client);
        let mut response_line = String::new();
        reader.read_line(&mut response_line).await.unwrap();

        let resp: Response<StatusData> = serde_json::from_str(&response_line).unwrap();
        assert!(resp.ok);
        let status = resp.data.unwrap();
        assert_eq!(status.version, "0.1.0");
        assert!(status.dry_run);
        assert_eq!(status.active_cages, 1);

        // Connect new client for cages request
        let mut client2 = UnixStream::connect(&sock_path).await.expect("connect 2");
        client2
            .write_all(b"{\"proto\":1,\"cmd\":\"cages\"}\n")
            .await
            .unwrap();
        let mut reader2 = BufReader::new(client2);
        let mut response_line2 = String::new();
        reader2.read_line(&mut response_line2).await.unwrap();

        let resp_cages: Response<Vec<CageRecord>> = serde_json::from_str(&response_line2).unwrap();
        assert!(resp_cages.ok);
        let cages = resp_cages.data.unwrap();
        assert_eq!(cages.len(), 1);
        assert_eq!(cages[0].pid, 4321);

        srv_handle.abort();
        let _ = fs::remove_file(&sock_path);
    }

    #[tokio::test]
    async fn test_ipc_server_proto_check_and_size_limit() {
        let sock_path = get_test_socket_path();
        let _ = fs::remove_file(&sock_path);

        let listener = bind_unix_listener(&sock_path, None).expect("bind listener");
        let state = Arc::new(Mutex::new(IpcState::new(
            "0.1.0".to_string(),
            true,
            10,
            "rules".to_string(),
            5,
        )));
        let my_uid = current_euid();
        let authorizer: AuthFn = Arc::new(move |peer_uid, _| peer_uid == my_uid);

        let srv_handle = tokio::spawn(run_ipc_server(listener, state, authorizer));

        // Invalid proto test
        let mut client = UnixStream::connect(&sock_path).await.expect("connect");
        client
            .write_all(b"{\"proto\":99,\"cmd\":\"status\"}\n")
            .await
            .unwrap();
        let mut reader = BufReader::new(client);
        let mut response_line = String::new();
        reader.read_line(&mut response_line).await.unwrap();

        let resp: Response<()> = serde_json::from_str(&response_line).unwrap();
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap(), "unsupported proto version");

        // Oversized line test (> 4 KiB)
        let mut client_large = UnixStream::connect(&sock_path).await.expect("connect");
        let large_junk = "a".repeat(4097);
        let req = format!("{{\"proto\":1,\"cmd\":\"status\",\"junk\":\"{large_junk}\"}}\n");
        let _ = client_large.write_all(req.as_bytes()).await;

        let mut reader_large = BufReader::new(client_large);
        let mut response_large = String::new();
        let _ = reader_large.read_line(&mut response_large).await;

        if !response_large.is_empty() {
            let resp: Response<()> = serde_json::from_str(&response_large).unwrap();
            assert!(!resp.ok);
        }

        srv_handle.abort();
        let _ = fs::remove_file(&sock_path);
    }

    #[tokio::test]
    async fn test_startup_wiring_from_config_to_status() {
        let sock_path = get_test_socket_path();
        let _ = fs::remove_file(&sock_path);

        let config = IpcConfig {
            socket_path: sock_path.clone(),
            ..Default::default()
        };

        let my_uid = current_euid();
        let authorizer: AuthFn = Arc::new(move |peer_uid, _| peer_uid == my_uid);

        let (handle, state) = start_ipc_server(&config, true, "0.1.0-test", "rules", 5, authorizer)
            .expect("startup wiring succeeds")
            .expect("server handle present");

        assert!(sock_path.exists(), "Socket file should exist after start");

        // Connect and verify status response
        let mut client = UnixStream::connect(&sock_path).await.expect("connect");
        client
            .write_all(b"{\"proto\":1,\"cmd\":\"status\"}\n")
            .await
            .unwrap();
        let mut reader = BufReader::new(client);
        let mut response_line = String::new();
        reader.read_line(&mut response_line).await.unwrap();

        let resp: Response<StatusData> = serde_json::from_str(&response_line).unwrap();
        assert!(resp.ok);
        let status = resp.data.unwrap();
        assert_eq!(status.version, "0.1.0-test");
        assert!(status.dry_run);

        // Verify shared state is active
        {
            let guard = state.lock().unwrap();
            assert_eq!(guard.version, "0.1.0-test");
        }

        handle.abort();
        let _ = fs::remove_file(&sock_path);
    }
}
