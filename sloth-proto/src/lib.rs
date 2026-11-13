//! Wire protocol contracts for sloth IPC (`sloth-proto`).
//!
//! One JSON object per line (`\n` terminated).
//! Every message has `"proto": 1`.

use serde::{Deserialize, Serialize};

pub const PROTO_VERSION: u32 = 1;

/// Override mode for a process name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverrideMode {
    /// Never throttle this name (beats ban).
    Permit,
    /// Treat as throttle candidate (all gate checks still apply).
    Ban,
    /// Remove any existing override.
    Clear,
}

/// A single persisted override entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverrideEntry {
    pub name: String,
    pub mode: OverrideMode,
}

/// Request messages sent by clients to sloth-daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Request current daemon status.
    Status,
    /// Request the recent decision audit log.
    Decisions {
        /// Maximum number of records to return.
        #[serde(default)]
        limit: Option<usize>,
    },
    /// Request currently throttled cgroups/cages.
    Cages,
    /// Set or clear a runtime override for a process name.
    Override {
        /// Process name (comm, exact case-sensitive match).
        name: String,
        /// Override mode: permit, ban, or clear.
        mode: OverrideMode,
    },
    /// List all current runtime overrides (read-only).
    Overrides,
    /// Unknown command — returns an error reply.
    #[serde(other)]
    Unknown,
}

/// Generic protocol envelope for requests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub proto: u32,
    #[serde(flatten)]
    pub request: Request,
}

/// Generic protocol response envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response<T> {
    pub proto: u32,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl<T> Response<T> {
    pub fn ok(data: T) -> Self {
        Self {
            proto: PROTO_VERSION,
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            proto: PROTO_VERSION,
            ok: false,
            data: None,
            error: Some(message.into()),
        }
    }
}

/// Parse a single JSON line into a validated RequestEnvelope.
/// Returns an error if JSON is malformed.
pub fn parse_request_line(line: &str) -> Result<RequestEnvelope, serde_json::Error> {
    serde_json::from_str(line)
}

/// Serialize a response to a single JSON line without trailing newline.
pub fn serialize_response<T: Serialize>(
    response: &Response<T>,
) -> Result<String, serde_json::Error> {
    serde_json::to_string(response)
}

/// Snapshot status data returned for `cmd: "status"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusData {
    pub version: String,
    pub dry_run: bool,
    pub active_cages: usize,
    pub uptime_secs: u64,
    pub decisions_recorded: usize,
    /// Operating mode: e.g. "rules" or "ollama"
    #[serde(default = "default_status_mode")]
    pub mode: String,
    /// Count of actively throttled processes
    #[serde(default)]
    pub throttled_count: usize,
    /// Maximum concurrent throttles allowed by policy
    #[serde(default)]
    pub cap: usize,
}

fn default_status_mode() -> String {
    "rules".to_string()
}

/// Cage record returned for `cmd: "cages"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CageRecord {
    pub pid: u32,
    pub comm: String,
    pub quota_pct: u32,
    pub throttled_secs: u64,
    pub ttl_remaining_secs: u64,
    pub reason: String,
}

/// Decision audit record returned for `cmd: "decisions"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub timestamp_epoch_secs: u64,
    pub pid: u32,
    pub comm: String,
    pub cpu_percent: f32,
    pub action: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_roundtrip_status() {
        let req_json = r#"{"proto":1,"cmd":"status"}"#;
        let parsed: RequestEnvelope = serde_json::from_str(req_json).unwrap();
        assert_eq!(parsed.proto, 1);
        assert_eq!(parsed.request, Request::Status);
    }

    #[test]
    fn test_request_roundtrip_decisions() {
        let req_json = r#"{"proto":1,"cmd":"decisions","limit":50}"#;
        let parsed: RequestEnvelope = serde_json::from_str(req_json).unwrap();
        assert_eq!(parsed.proto, 1);
        assert_eq!(parsed.request, Request::Decisions { limit: Some(50) });
    }

    #[test]
    fn test_request_unknown_fields_ignored() {
        let req_json = r#"{"proto":1,"cmd":"cages","unexpected":"ignored"}"#;
        let parsed: RequestEnvelope = serde_json::from_str(req_json).unwrap();
        assert_eq!(parsed.request, Request::Cages);
    }

    #[test]
    fn test_request_override_roundtrip() {
        let req_json = r#"{"proto":1,"cmd":"override","name":"yes","mode":"ban"}"#;
        let parsed: RequestEnvelope = serde_json::from_str(req_json).unwrap();
        assert_eq!(
            parsed.request,
            Request::Override {
                name: "yes".to_string(),
                mode: OverrideMode::Ban,
            }
        );
    }

    #[test]
    fn test_request_overrides_list() {
        let req_json = r#"{"proto":1,"cmd":"overrides"}"#;
        let parsed: RequestEnvelope = serde_json::from_str(req_json).unwrap();
        assert_eq!(parsed.request, Request::Overrides);
    }

    #[test]
    fn test_unknown_cmd_returns_unknown_variant() {
        let req_json = r#"{"proto":1,"cmd":"frobnicate"}"#;
        let parsed: RequestEnvelope = serde_json::from_str(req_json).unwrap();
        assert_eq!(parsed.request, Request::Unknown);
    }

    #[test]
    fn test_response_serialization() {
        let resp = Response::ok(StatusData {
            version: "0.1.0".to_string(),
            dry_run: true,
            active_cages: 2,
            uptime_secs: 120,
            decisions_recorded: 15,
            mode: "rules".to_string(),
            throttled_count: 2,
            cap: 5,
        });
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains(r#""proto":1"#));
        assert!(s.contains(r#""ok":true"#));
        assert!(s.contains(r#""dry_run":true"#));
    }

    #[test]
    fn test_error_response_serialization() {
        let resp: Response<()> = Response::err("short message");
        let s = serde_json::to_string(&resp).unwrap();
        assert_eq!(s, r#"{"proto":1,"ok":false,"error":"short message"}"#);
    }
}
