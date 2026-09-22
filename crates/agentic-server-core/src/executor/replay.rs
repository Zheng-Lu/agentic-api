//! Server-owned provenance at the inference/continuation boundary.
//!
//! No parsing, output-item assembly, delivery, or opaque replay happens here.

use sha2::{Digest, Sha256};

use super::request::{ExecutionContext, RequestContext};
use crate::types::ResponsePayload;
use crate::types::io::{InputItem, OutputItem, ResponsesInput};
use crate::types::reasoning_replay::{ReasoningProvenance, ReasoningReplayIdentity, ReasoningReplayPolicy};
use crate::types::upstream_identity::UpstreamModelId;

/// Bind exact routing inputs and both model identities without retaining secrets.
/// Nested, fixed-width hashes make field boundaries unambiguous without allocations.
/// Missing auth is distinct from an explicitly supplied empty bearer credential.
pub(super) fn upstream_identity(
    policy: ReasoningReplayPolicy,
    endpoint: &str,
    auth: Option<&str>,
    requested_model: &str,
    reported_model: Option<&str>,
) -> ReasoningReplayIdentity {
    let mut digest = Sha256::new();
    digest.update(b"agentic-api/reasoning-replay/identity/v1\0");
    digest.update([match policy {
        ReasoningReplayPolicy::VllmPlaintext => 0,
        ReasoningReplayPolicy::OpaqueResponses => 1,
    }]);
    digest.update(Sha256::digest(endpoint.as_bytes()));
    digest.update([u8::from(auth.is_some())]);
    digest.update(Sha256::digest(auth.unwrap_or_default().as_bytes()));
    digest.update(Sha256::digest(requested_model.as_bytes()));
    digest.update([u8::from(reported_model.is_some())]);
    digest.update(Sha256::digest(reported_model.unwrap_or_default().as_bytes()));
    ReasoningReplayIdentity::from_digest(digest.finalize().into())
}

/// Ingestion has finished; annotate only items observed from this upstream round.
pub(super) fn record_round_provenance(
    payload: &mut ResponsePayload,
    reported_model: Option<&UpstreamModelId>,
    exec_ctx: &ExecutionContext,
    request: &RequestContext,
    auth: Option<&str>,
) {
    if !payload
        .output
        .iter()
        .any(|item| matches!(item, OutputItem::Reasoning(_)))
    {
        return;
    }
    let policy = exec_ctx.responses_config.reasoning_replay_policy;
    let identity = upstream_identity(
        policy,
        &exec_ctx.responses_url(),
        auth,
        &request.enriched_request.model,
        reported_model.map(UpstreamModelId::as_str),
    );
    record_upstream_provenance(&mut payload.output, policy, identity);
}

/// Apply the round's server-owned observation after successful ingestion.
pub(super) fn record_upstream_provenance(
    output: &mut [OutputItem],
    policy: ReasoningReplayPolicy,
    identity: ReasoningReplayIdentity,
) {
    for item in output {
        if let OutputItem::Reasoning(reasoning) = item {
            reasoning.replay_provenance = Some(ReasoningProvenance::upstream(policy, identity));
        }
    }
}

/// Incoming public items never inherit an internal provenance claim.
pub(super) fn mark_client_input(input: &mut ResponsesInput) {
    if let ResponsesInput::Items(items) = input {
        mark_client_items(items);
    }
}

/// Canonical new inputs can also return from a serialized split-execution context.
pub(super) fn mark_client_items(items: &mut [InputItem]) {
    for item in items {
        if let InputItem::Reasoning(reasoning) = item {
            reasoning.replay_provenance = Some(ReasoningProvenance::client_submitted());
        }
    }
}

/// Split-execution output was not observed by this gateway's inference transport.
pub(super) fn mark_external_output(output: &mut [OutputItem]) {
    for item in output {
        if let OutputItem::Reasoning(reasoning) = item {
            reasoning.replay_provenance = Some(ReasoningProvenance::client_submitted());
        }
    }
}

#[cfg(test)]
mod tests;
