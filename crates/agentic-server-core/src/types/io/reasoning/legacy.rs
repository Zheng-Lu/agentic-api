//! Reasoning items in the shape earlier releases accepted.
//!
//! Before typed reasoning, a reasoning item accepted any string content
//! discriminator and untyped summary, opaque state, and status values. Stored
//! history rows, stored response metadata, and lenient upstream ingestion can
//! still carry that shape. This decoder mirrors that schema exactly and keeps
//! every field that satisfies the typed schema. An item it rejects could not have
//! been accepted before either, so callers fail closed or drop it as they did then.

use serde::Deserialize;
use serde_json::Value;

use super::{OpaqueReasoning, ReasoningStatus, ReasoningSummaryContent, ReasoningTextContent};
use crate::types::io::{AgentAttribution, OutputItem, ReasoningOutput};

/// The reasoning item schema before typed reasoning.
#[derive(Deserialize)]
struct LegacyReasoning {
    #[serde(default)]
    agent: Option<AgentAttribution>,
    #[serde(default)]
    id: String,
    #[serde(default)]
    content: Option<Vec<LegacyText>>,
    #[serde(default)]
    summary: Option<Vec<Value>>,
    encrypted_content: Option<Value>,
    status: Option<String>,
}

/// Earlier releases required a string discriminator but accepted any spelling.
#[derive(Deserialize)]
struct LegacyText {
    #[serde(rename = "type")]
    _kind: String,
    text: String,
}

impl ReasoningOutput {
    /// Decode a reasoning item that satisfies the schema earlier releases accepted.
    ///
    /// Plaintext parts become `reasoning_text`. Summary parts, opaque state, and
    /// a status that don't satisfy the typed schema are dropped. Items that are
    /// already typed decode unchanged. Returns `None` for anything else.
    pub(crate) fn from_legacy_value(item: &Value) -> Option<Self> {
        if item.get("type").and_then(Value::as_str) != Some("reasoning") {
            return None;
        }
        let legacy = LegacyReasoning::deserialize(item).ok()?;
        Some(Self {
            agent: legacy.agent,
            id: legacy.id,
            content: legacy
                .content
                .unwrap_or_default()
                .into_iter()
                .map(|part| ReasoningTextContent::new(part.text))
                .collect(),
            summary: legacy
                .summary
                .unwrap_or_default()
                .iter()
                .filter_map(|part| ReasoningSummaryContent::deserialize(part).ok())
                .collect(),
            encrypted_content: match legacy.encrypted_content {
                Some(Value::String(state)) => OpaqueReasoning::try_from(state).ok(),
                _ => None,
            },
            status: legacy
                .status
                .and_then(|status| ReasoningStatus::deserialize(Value::String(status)).ok()),
        })
    }
}

/// Replace each reasoning item in the earlier shape with its typed projection.
///
/// Other items stay unchanged, so a caller that decodes the result still fails
/// closed on anything the projection can't read.
pub(crate) fn upgrade_legacy_reasoning(items: &mut [Value]) {
    for item in items {
        if let Some(reasoning) = ReasoningOutput::from_legacy_value(item)
            && let Ok(upgraded) = serde_json::to_value(OutputItem::Reasoning(reasoning))
        {
            *item = upgraded;
        }
    }
}
