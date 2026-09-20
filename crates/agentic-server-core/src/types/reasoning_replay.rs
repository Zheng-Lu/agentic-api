//! Server-owned reasoning replay policy and versioned, non-wire provenance.
//!
//! Provenance records an observation, not permission to replay opaque state.
//! It is persisted separately from public items and cannot be supplied over HTTP/WS.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum serialized provenance per stored reasoning item, including JSON overhead.
pub const MAX_REASONING_PROVENANCE_BYTES: usize = 512;

/// Reasoning projection selected by the server, never by a request's model name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningReplayPolicy {
    /// Existing vLLM plaintext projection; opaque-only continuations are rejected.
    #[default]
    VllmPlaintext,
    /// Reserved for qualified opaque Responses replay. Not executable yet.
    OpaqueResponses,
}

impl ReasoningReplayPolicy {
    /// Validate availability before storage, tool discovery, or upstream inference.
    ///
    /// # Errors
    /// Returns an error for an unqualified replay policy. Defining a policy or
    /// retaining provenance does not enable its execution.
    pub fn validate(self) -> Result<(), ReasoningReplayError> {
        match self {
            Self::VllmPlaintext => Ok(()),
            Self::OpaqueResponses => Err(ReasoningReplayError::OpaqueNotEnabled),
        }
    }
}

/// Replay failures deliberately contain neither credentials nor opaque state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ReasoningReplayError {
    #[error("opaque reasoning replay is not enabled; provider qualification is incomplete")]
    OpaqueNotEnabled,
}

/// Fixed-size fingerprint of the effective upstream routing and model identity.
///
/// The executor binds policy, endpoint, effective bearer credential, requested model,
/// and optional authoritative reported model (currently unavailable in ingestion).
/// This is not an authentication credential or a cross-provider compatibility claim.
/// No original identity components are retained here.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReasoningReplayIdentity([u8; 32]);

impl ReasoningReplayIdentity {
    pub(crate) fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }
}

impl fmt::Debug for ReasoningReplayIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ReasoningReplayIdentity(<redacted>)")
    }
}

/// Where this particular reasoning item entered canonical history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReasoningSource {
    /// Manually supplied input or externally committed output, not gateway-observed.
    /// Successful inference must not upgrade this to `Upstream`.
    ClientSubmitted {},
    /// Observed through this gateway's inference path under the recorded policy.
    Upstream {
        policy: ReasoningReplayPolicy,
        identity: ReasoningReplayIdentity,
    },
}

/// Versioned per-item provenance stored outside the public Responses schema.
///
/// SQL NULL on legacy items means unknown provenance, not client-submitted or
/// provider-issued state. Unknown versions and fields must fail closed on load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "version", deny_unknown_fields)]
pub enum ReasoningProvenance {
    #[serde(rename = "1")]
    V1 { source: ReasoningSource },
}

impl ReasoningProvenance {
    #[must_use]
    pub const fn client_submitted() -> Self {
        Self::V1 {
            source: ReasoningSource::ClientSubmitted {},
        }
    }

    pub(crate) fn upstream(policy: ReasoningReplayPolicy, identity: ReasoningReplayIdentity) -> Self {
        Self::V1 {
            source: ReasoningSource::Upstream { policy, identity },
        }
    }
}

/// Reserve fixed inline provenance space even before the engine records its origin.
/// This charge also covers the optional discriminant for legacy/missing provenance.
pub const REASONING_PROVENANCE_RETAINED_BYTES: usize = std::mem::size_of::<Option<ReasoningProvenance>>();
