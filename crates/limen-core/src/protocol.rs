//! The node protocol (spec §4): one JSON request on the gate's stdin, one JSON answer on its stdout.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;

/// The node protocol versions this build speaks.
pub const PROTOCOL_VERSIONS: &[i64] = &[1];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    BadRequest,
    Denied,
    NotFound,
    Unavailable,
    Timeout,
    UnsupportedVersion,
    Internal,
    // Only the hub produces these two: they are what `ssh` itself says when it cannot reach the gate.
    Unreachable,
    HostKeyMismatch,
}

impl ErrorCode {
    pub fn wire(self) -> &'static str {
        match self {
            Self::BadRequest => "bad_request",
            Self::Denied => "denied",
            Self::NotFound => "not_found",
            Self::Unavailable => "unavailable",
            Self::Timeout => "timeout",
            Self::UnsupportedVersion => "unsupported_version",
            Self::Internal => "internal",
            Self::Unreachable => "unreachable",
            Self::HostKeyMismatch => "host_key_mismatch",
        }
    }
}

/// What every fallible step of limen fails with: a code the wire knows and a message a person can act on.
#[derive(Debug, Clone, PartialEq)]
pub struct LimenError {
    pub code: ErrorCode,
    pub message: String,
    pub versions: Option<Vec<i64>>,
}

impl LimenError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), versions: None }
    }
}

impl fmt::Display for LimenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LimenError {}

pub type Result<T> = std::result::Result<T, LimenError>;

pub fn bad_request(message: impl Into<String>) -> LimenError {
    LimenError::new(ErrorCode::BadRequest, message)
}

pub fn error(code: ErrorCode, message: impl Into<String>) -> LimenError {
    LimenError::new(code, message)
}

/// One request. Strict: a field nobody reads is an error, not a silent no-op.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::min_ident_chars, reason = "`v`, the protocol version, is the field's name on the wire (spec §4)")]
pub struct NodeRequest {
    pub v: i64,
    pub request: String,
    #[serde(default)]
    pub args: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub versions: Option<Vec<i64>>,
}

/// The gate's answer. `truncated` says an output limit cut the data (spec §1, principle 5); it is there only then.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<NodeError>,
}

impl NodeResponse {
    pub fn success(data: Value, truncated: bool) -> Self {
        Self { ok: true, data: Some(data), truncated, error: None }
    }

    pub fn failure(cause: &LimenError) -> Self {
        Self {
            ok: false,
            data: None,
            truncated: false,
            error: Some(NodeError {
                code: cause.code.wire().into(),
                message: cause.message.clone(),
                versions: cause.versions.clone(),
            }),
        }
    }
}

/// JSON with two-space indentation, as `limen call` and the CLI print it.
pub fn pretty(value: &impl Serialize) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}
