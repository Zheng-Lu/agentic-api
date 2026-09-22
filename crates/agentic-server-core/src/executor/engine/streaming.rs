//! Stream-owned execution lifecycle, terminal validation, and failure delivery.

mod producer;

use super::run_until_gateway_tools_complete;
use crate::executor::error::ExecutorError;
use crate::executor::gateway_accumulator::{GatewayStreamAccumulator, STREAM_EVENT_BUFFER, StreamEvent};
use crate::executor::inference::{BoxStream, DONE_MARKER};
use crate::executor::persist::persist_if_needed;
use crate::executor::request::{ExecutionContext, RequestContext};
use crate::executor::upstream::agent_pipeline_with_limits;
use crate::tool::ToolSearchState;
use crate::types::request_response::ResponsePayload;
use crate::utils::common::utcnow_str;
use async_stream::stream;
use futures::StreamExt;
use producer::ProducerEvent;
use std::sync::Arc;
use tokio::sync::mpsc;

/// `max_stream_event_bytes` is the effective limit for one serialized client
/// event on the transport that will deliver this stream. The terminal
/// `response.completed` frame is validated against it before the response is
/// persisted or a session checkpoint is published, so a frame the transport
/// cannot deliver never leaves a stored response behind.
pub(super) fn run_stream(
    ctx: RequestContext,
    tool_search_state: Option<ToolSearchState>,
    exec_ctx: Arc<ExecutionContext>,
    auth: Option<String>,
    max_stream_event_bytes: usize,
) -> BoxStream {
    Box::pin(stream! {
        let failure_context = StreamFailureContext::from(&ctx);
        let (event_tx, event_rx) = mpsc::channel(STREAM_EVENT_BUFFER);
        let exec_ctx_for_run = Arc::clone(&exec_ctx);
        let mut agent = agent_pipeline_with_limits(
            ctx,
            tool_search_state,
            Some(event_tx),
            max_stream_event_bytes,
        );
        let run = async move {
            let result = run_until_gateway_tools_complete(
                &mut agent,
                exec_ctx_for_run.as_ref(),
                auth.as_deref(),
                true,
            )
            .await;
            let (ctx, stream_accumulator) = agent.into_parts();
            (result.map(|(payload, metadata)| (payload, ctx, metadata)), stream_accumulator)
        };
        let events = producer::drive(run, event_rx);
        futures::pin_mut!(events);

        let mut next_sequence_number = 0;
        while let Some(event) = events.next().await {
            match event {
                ProducerEvent::Event(event) => {
                    yield consume_stream_event(event, &mut next_sequence_number);
                }
                ProducerEvent::Finished(result) => {
                    match result {
                        Err(e) => {
                            yield GatewayStreamAccumulator::executor_error_chunk_at(&e, next_sequence_number);
                            yield DONE_MARKER.to_string();
                        }
                        Ok((Err(e), mut stream_accumulator)) => {
                            if e.is_invalid_upstream_tool_search() {
                                let payload = failure_context.failed_payload(&e);
                                match stream_accumulator.terminal_response_chunk(&payload) {
                                    Ok(chunk) => yield chunk,
                                    Err(serialize_error) => {
                                        yield GatewayStreamAccumulator::executor_error_chunk_at(
                                            &serialize_error,
                                            next_sequence_number,
                                        );
                                    }
                                }
                            } else {
                                yield GatewayStreamAccumulator::executor_error_chunk_at(&e, next_sequence_number);
                            }
                            yield DONE_MARKER.to_string();
                        }
                        Ok((Ok((payload, ctx, tool_search_metadata)), stream_accumulator)) => {
                            // Codex may close its WebSocket as soon as it receives
                            // `response.completed`. Persist before exposing that
                            // event so a custom call/output continuation cannot be
                            // cancelled by the client disconnect.
                            let ch = exec_ctx.conv_handler.clone();
                            let rh = exec_ctx.resp_handler.clone();
                            let mut terminal_accumulator = stream_accumulator.clone();
                            let terminal_chunk = terminal_accumulator.terminal_response_chunk(&payload);
                            match terminal_chunk {
                                Err(e) => {
                                    yield GatewayStreamAccumulator::executor_error_chunk_at(&e, next_sequence_number);
                                }
                                Ok(chunk) => match persist_if_needed(payload, ctx, tool_search_metadata, ch, rh).await {
                                    Ok(()) => yield chunk,
                                    Err(e) => {
                                        yield GatewayStreamAccumulator::executor_error_chunk_at(
                                            &e,
                                            next_sequence_number,
                                        );
                                    }
                                }
                            }
                            yield DONE_MARKER.to_string();
                        }
                    }
                    break;
                }
            }
        }
    })
}

pub(super) struct StreamFailureContext {
    response_id: String,
    conversation_id: Option<String>,
    model: String,
    previous_response_id: Option<String>,
    instructions: Option<String>,
}

impl From<&RequestContext> for StreamFailureContext {
    fn from(ctx: &RequestContext) -> Self {
        Self {
            response_id: ctx.response_id.clone(),
            conversation_id: ctx.conversation_id.clone(),
            model: ctx.enriched_request.model.clone(),
            previous_response_id: ctx.original_request.previous_response_id.clone(),
            instructions: ctx.original_request.instructions.clone(),
        }
    }
}

impl StreamFailureContext {
    pub(super) fn failed_payload(&self, error: &ExecutorError) -> ResponsePayload {
        ResponsePayload {
            id: self.response_id.clone(),
            object: "response".to_owned(),
            created_at: utcnow_str(),
            model: self.model.clone(),
            status: "failed".to_owned(),
            output: Vec::new(),
            usage: None,
            incomplete_details: None,
            error: Some(serde_json::json!({
                "message": error.error_message(),
                "type": error.error_type(),
                "code": error.error_code(),
            })),
            previous_response_id: self.previous_response_id.clone(),
            conversation_id: self.conversation_id.clone(),
            instructions: self.instructions.clone(),
            tools: None,
            tool_choice: None,
        }
    }
}

pub(super) fn consume_stream_event(event: StreamEvent, next_sequence_number: &mut u64) -> String {
    *next_sequence_number = event.sequence_number.saturating_add(1);
    event.content
}
