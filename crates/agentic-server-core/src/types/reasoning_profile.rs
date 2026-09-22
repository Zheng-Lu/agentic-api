//! Closed, server-owned candidate profiles for opaque reasoning replay.
//!
//! A profile names a gateway compatibility contract, not a provider ciphertext
//! version or a claim of qualification. No profile is executable yet.

use serde::{Deserialize, Serialize};

use super::reasoning_replay::ReasoningReplayError;

/// A pinned candidate. Aliases, regional endpoints, proxies with another URL,
/// compatible model families, and arbitrary user-defined profiles are not implied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpaqueReasoningProfile {
    /// `OpenAI` Responses `reasoning.encrypted_content`, exact snapshot, contract v1.
    /// This revision describes the gateway contract; the opaque bytes are never decoded.
    #[serde(rename = "openai_gpt_5_4_2026_03_05_v1")]
    OpenAiGpt54_20260305V1,
}

impl OpaqueReasoningProfile {
    /// Exact configured endpoint. Redirects must be disabled before qualification.
    #[must_use]
    pub const fn endpoint(self) -> &'static str {
        match self {
            Self::OpenAiGpt54_20260305V1 => "https://api.openai.com/v1/responses",
        }
    }

    /// Both the requested model and consistently reported terminal model must match.
    #[must_use]
    pub const fn model(self) -> &'static str {
        match self {
            Self::OpenAiGpt54_20260305V1 => "gpt-5.4-2026-03-05",
        }
    }

    /// Stable identity domain. A changed wire contract requires a new profile.
    pub(crate) const fn identity_domain(self) -> &'static [u8] {
        match self {
            Self::OpenAiGpt54_20260305V1 => b"openai/responses/reasoning.encrypted_content/gpt-5.4-2026-03-05/v1",
        }
    }

    /// Check exact routing without retaining or echoing potentially sensitive URLs.
    ///
    /// # Errors
    /// Returns a redacted error for any endpoint or requested-model mismatch.
    pub fn validate_target(self, endpoint: &str, model: &str) -> Result<(), ReasoningReplayError> {
        if endpoint != self.endpoint() {
            return Err(ReasoningReplayError::EndpointMismatch);
        }
        if model != self.model() {
            return Err(ReasoningReplayError::ModelMismatch);
        }
        Ok(())
    }
}
