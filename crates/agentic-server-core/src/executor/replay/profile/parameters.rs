//! Request-surface validation for the exact pinned opaque profile.
//!
//! Only preflight calls this. It never normalizes or mutates public input, and
//! must run before rehydration or gateway tool discovery can do external work.

use crate::types::io::ToolChoice;
use crate::types::reasoning_profile::{OpaqueReasoningProfile, OpaqueReplayRequestField as Field};
use crate::types::reasoning_replay::ReasoningReplayError;
use crate::types::request_response::RequestPayload;
use crate::types::tools::ResponsesTool;

fn unsupported(field: Field) -> ReasoningReplayError {
    ReasoningReplayError::UnsupportedParameter(field)
}

pub(in crate::executor::replay) fn validate(
    profile: OpaqueReasoningProfile,
    request: &RequestPayload,
) -> Result<(), ReasoningReplayError> {
    match profile {
        OpaqueReasoningProfile::OpenAiGpt54_20260305V1 => validate_gpt54(request),
    }
}

fn validate_gpt54(request: &RequestPayload) -> Result<(), ReasoningReplayError> {
    if let Some(reasoning) = request.reasoning.as_deref() {
        if reasoning.context.is_some() {
            return Err(unsupported(Field::ReasoningContext));
        }
        if reasoning.effort.as_deref().is_some_and(|effort| effort != "low") {
            return Err(unsupported(Field::ReasoningEffort));
        }
        if reasoning.generate_summary.is_some() {
            return Err(unsupported(Field::ReasoningGenerateSummary));
        }
        if reasoning.mode.is_some() {
            return Err(unsupported(Field::ReasoningMode));
        }
        if reasoning.summary.as_deref().is_some_and(|summary| summary != "concise") {
            return Err(unsupported(Field::ReasoningSummary));
        }
    }
    if request
        .include
        .as_ref()
        .is_some_and(|include| include.iter().any(|item| item != "reasoning.encrypted_content"))
    {
        return Err(unsupported(Field::Include));
    }
    if request.text.is_some() {
        return Err(unsupported(Field::Text));
    }
    if request.temperature.is_some() {
        return Err(unsupported(Field::Temperature));
    }
    if request.top_p.is_some() {
        return Err(unsupported(Field::TopP));
    }
    if request
        .max_output_tokens
        .is_some_and(|tokens| !(1..=128_000).contains(&tokens))
    {
        return Err(unsupported(Field::MaxOutputTokens));
    }
    if request.ignore_eos.is_some() {
        return Err(unsupported(Field::IgnoreEos));
    }
    if request.truncation.as_deref().is_some_and(|mode| mode != "disabled") {
        return Err(unsupported(Field::Truncation));
    }
    if request.metadata.is_some() {
        return Err(unsupported(Field::Metadata));
    }
    if request.parallel_tool_calls == Some(true) {
        return Err(unsupported(Field::ParallelToolCalls));
    }
    if request.cache_salt.is_some() {
        return Err(unsupported(Field::CacheSalt));
    }
    if request.tools.as_ref().is_some_and(|tools| {
        tools.iter().any(|tool| match tool {
            ResponsesTool::Function(function) => function.defer_loading == Some(true) || !function.extra.is_empty(),
            ResponsesTool::Mcp(mcp) => {
                mcp.defer_loading == Some(true) || mcp.require_approval.as_deref() != Some("never")
            }
            _ => true,
        })
    }) {
        return Err(unsupported(Field::Tools));
    }
    if request.tool_choice.as_ref().is_some_and(|choice| {
        !matches!(
            choice,
            ToolChoice::Auto | ToolChoice::None | ToolChoice::Required | ToolChoice::Function { namespace: None, .. }
        )
    }) {
        return Err(unsupported(Field::ToolChoice));
    }
    Ok(())
}
