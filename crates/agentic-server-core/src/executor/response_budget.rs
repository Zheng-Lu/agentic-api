use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::types::io::OutputItem;
use crate::types::request_response::ResponsePayload;

#[cfg(test)]
pub use crate::config::DEFAULT_MAX_RETAINED_RESPONSE_BYTES;
#[cfg(test)]
pub(super) const MAX_EXECUTOR_RESPONSE_BYTES: usize = DEFAULT_MAX_RETAINED_RESPONSE_BYTES;
pub(super) const RETAINED_CONTAINER_OVERHEAD_BYTES: usize = 32;

#[derive(Clone, Debug)]
pub(super) struct ExecutorResponseBudget {
    limit: usize,
    used: Arc<AtomicUsize>,
}

impl ExecutorResponseBudget {
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self::with_limit(MAX_EXECUTOR_RESPONSE_BYTES)
    }

    pub(super) fn with_limit(limit: usize) -> Self {
        Self {
            limit,
            used: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[cfg(test)]
    pub(super) fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    pub(super) fn consume(&self, bytes: usize) -> ExecutorResult<()> {
        let limit = self.limit;
        self.used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                let next = used.checked_add(bytes)?;
                if next > limit { None } else { Some(next) }
            })
            .map(|_| ())
            .map_err(|_| ExecutorError::ResourceLimitExceeded {
                limit: crate::executor::error::ResourceLimit::ResponseBudget,
                max_bytes: limit,
            })
    }
}

pub(in crate::executor) fn retained_output_item_bytes(item: &OutputItem) -> usize {
    match item {
        OutputItem::Message(msg) => {
            let mut bytes = RETAINED_CONTAINER_OVERHEAD_BYTES + msg.id.len();
            for part in &msg.content {
                bytes += RETAINED_CONTAINER_OVERHEAD_BYTES + part.text.len();
            }
            bytes
        }
        OutputItem::FunctionCall(call) => {
            RETAINED_CONTAINER_OVERHEAD_BYTES
                + call.id.len()
                + call.call_id.len()
                + call.name.len()
                + call.arguments.len()
        }
        OutputItem::CustomToolCall(call) => {
            RETAINED_CONTAINER_OVERHEAD_BYTES + call.id.len() + call.call_id.len() + call.name.len() + call.input.len()
        }
        OutputItem::ShellCall(call) => {
            let mut bytes =
                RETAINED_CONTAINER_OVERHEAD_BYTES + call.id.as_ref().map_or(0, String::len) + call.call_id.len();
            for cmd in &call.action.commands {
                bytes += RETAINED_CONTAINER_OVERHEAD_BYTES + cmd.len();
            }
            bytes
        }
        OutputItem::Reasoning(reasoning) => {
            let mut bytes = RETAINED_CONTAINER_OVERHEAD_BYTES + reasoning.id.len();
            if let Some(enc) = &reasoning.encrypted_content {
                bytes += enc.as_str().map_or(32, str::len);
            }
            for part in &reasoning.content {
                bytes += RETAINED_CONTAINER_OVERHEAD_BYTES + part.text.len();
            }
            for part in &reasoning.summary {
                bytes += RETAINED_CONTAINER_OVERHEAD_BYTES;
                if let Some(text) = part.get("text").and_then(serde_json::Value::as_str) {
                    bytes += text.len();
                }
            }
            bytes
        }
        OutputItem::ToolSearchCall(call) => {
            let args_len = if let Some(map) = call.arguments.as_object() {
                map.iter().map(|(k, v)| k.len() + v.as_str().map_or(32, str::len)).sum()
            } else {
                call.arguments.as_str().map_or(32, str::len)
            };
            RETAINED_CONTAINER_OVERHEAD_BYTES + call.id.len() + call.call_id.len() + args_len
        }
        OutputItem::WebSearchCall(call) => RETAINED_CONTAINER_OVERHEAD_BYTES + call.id.len(),
        OutputItem::McpCall(call) => {
            RETAINED_CONTAINER_OVERHEAD_BYTES
                + call.id.len()
                + call.server_label.len()
                + call.name.len()
                + call.arguments.len()
        }
        OutputItem::McpListTools(list) => RETAINED_CONTAINER_OVERHEAD_BYTES + list.id.len() + list.server_label.len(),
        OutputItem::Compaction(compaction) => {
            RETAINED_CONTAINER_OVERHEAD_BYTES + compaction.id.as_ref().map_or(0, String::len)
        }
        OutputItem::Unknown => RETAINED_CONTAINER_OVERHEAD_BYTES,
    }
}

pub(in crate::executor) fn retained_response_parts_bytes(response_id: &str, output: &[OutputItem]) -> usize {
    let mut bytes = RETAINED_CONTAINER_OVERHEAD_BYTES + response_id.len();
    for item in output {
        bytes += retained_output_item_bytes(item);
    }
    bytes
}

#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::executor) fn retained_response_bytes(response: &ResponsePayload) -> usize {
    retained_response_parts_bytes(&response.id, &response.output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_response_bytes_accounts_for_id_and_items() {
        let payload = ResponsePayload {
            id: "resp_test".to_owned(),
            object: "response".to_owned(),
            created_at: 1000,
            model: "test".to_owned(),
            status: "completed".to_owned(),
            output: vec![OutputItem::Unknown],
            usage: None,
            incomplete_details: None,
            error: None,
            previous_response_id: None,
            conversation_id: None,
            instructions: None,
            tools: None,
            tool_choice: None,
        };
        let expected = RETAINED_CONTAINER_OVERHEAD_BYTES + "resp_test".len() + RETAINED_CONTAINER_OVERHEAD_BYTES;
        assert_eq!(retained_response_bytes(&payload), expected);
        assert_eq!(retained_response_parts_bytes(&payload.id, &payload.output), expected);
    }
}
