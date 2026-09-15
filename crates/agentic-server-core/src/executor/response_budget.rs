use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::types::io::{McpCallError, OutputItem, WebSearchAction};
#[cfg(test)]
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
        self.used.load(Ordering::Acquire)
    }

    pub(super) fn consume(&self, bytes: usize) -> ExecutorResult<()> {
        let limit = self.limit;
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
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
                bytes += estimate_json_value_bytes(enc);
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
        OutputItem::WebSearchCall(call) => {
            let action_len = match &call.action {
                WebSearchAction::Search(s) => s.queries.iter().map(String::len).sum::<usize>(),
                WebSearchAction::OpenPage(op) => op.url.as_ref().map_or(0, String::len),
                WebSearchAction::FindInPage(fip) => fip.pattern.len() + fip.url.len(),
            };
            RETAINED_CONTAINER_OVERHEAD_BYTES + call.id.len() + action_len
        }
        OutputItem::McpCall(call) => {
            let error_len = match &call.error {
                Some(McpCallError::Text(t)) => t.len(),
                Some(McpCallError::ToolExecution(err)) => err.content.iter().map(|c| c.text.len()).sum::<usize>(),
                Some(McpCallError::Unknown(val)) => estimate_json_value_bytes(val),
                None => 0,
            };
            RETAINED_CONTAINER_OVERHEAD_BYTES
                + call.id.len()
                + call.server_label.len()
                + call.name.len()
                + call.arguments.len()
                + call.output.as_ref().map_or(0, String::len)
                + error_len
        }
        OutputItem::McpListTools(list) => {
            let mut bytes = RETAINED_CONTAINER_OVERHEAD_BYTES + list.id.len() + list.server_label.len();
            if let Some(err) = &list.error {
                bytes += err.len();
            }
            for tool in &list.tools {
                bytes += RETAINED_CONTAINER_OVERHEAD_BYTES
                    + tool.name.len()
                    + tool.description.as_ref().map_or(0, String::len)
                    + estimate_json_value_bytes(&tool.input_schema);
            }
            bytes
        }
        OutputItem::Compaction(compaction) => {
            RETAINED_CONTAINER_OVERHEAD_BYTES + compaction.id.as_ref().map_or(0, String::len)
        }
        OutputItem::Unknown => RETAINED_CONTAINER_OVERHEAD_BYTES,
    }
}

/// Approximates the serialized byte footprint of an arbitrary JSON value (e.g. MCP schemas or errors).
///
/// This provides a heuristic lower-bound estimate to prevent unbounded memory growth from unstructured
/// JSON schemas without incurring the overhead of full re-serialization.
fn estimate_json_value_bytes(val: &serde_json::Value) -> usize {
    match val {
        serde_json::Value::Null => 4,
        serde_json::Value::Bool(_) => 5,
        serde_json::Value::Number(_) => 8,
        serde_json::Value::String(s) => s.len(),
        serde_json::Value::Array(arr) => {
            arr.iter().map(estimate_json_value_bytes).sum::<usize>() + RETAINED_CONTAINER_OVERHEAD_BYTES
        }
        serde_json::Value::Object(map) => {
            map.iter()
                .map(|(k, v)| k.len() + estimate_json_value_bytes(v))
                .sum::<usize>()
                + RETAINED_CONTAINER_OVERHEAD_BYTES
        }
    }
}

pub(in crate::executor) fn retained_response_parts_bytes(response_id: &str, output: &[OutputItem]) -> usize {
    let mut bytes = RETAINED_CONTAINER_OVERHEAD_BYTES + response_id.len();
    for item in output {
        bytes += retained_output_item_bytes(item);
    }
    bytes
}

#[cfg(test)]
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

    #[test]
    fn retained_accounting_for_web_search_mcp_and_reasoning() {
        use crate::types::io::output::{
            McpListTool, McpListTools, ReasoningOutput, ReasoningTextContent, WebSearchActionOpenPage,
            WebSearchActionSearch, WebSearchCall, WebSearchCallStatus,
        };
        use crate::types::io::{McpCall, McpCallStatus};

        // WebSearchCall Search
        let ws_search = OutputItem::WebSearchCall(WebSearchCall {
            id: "ws_1".to_owned(),
            status: WebSearchCallStatus::Completed,
            action: WebSearchAction::Search(
                WebSearchActionSearch::try_new(vec!["query1".to_owned(), "q2".to_owned()], vec![]).unwrap(),
            ),
        });
        assert_eq!(
            retained_output_item_bytes(&ws_search),
            RETAINED_CONTAINER_OVERHEAD_BYTES + "ws_1".len() + ("query1".len() + "q2".len())
        );

        // WebSearchCall OpenPage
        let ws_open = OutputItem::WebSearchCall(WebSearchCall {
            id: "ws_2".to_owned(),
            status: WebSearchCallStatus::Completed,
            action: WebSearchAction::OpenPage(WebSearchActionOpenPage {
                url: Some("https://example.com".to_owned()),
            }),
        });
        assert_eq!(
            retained_output_item_bytes(&ws_open),
            RETAINED_CONTAINER_OVERHEAD_BYTES + "ws_2".len() + "https://example.com".len()
        );

        // McpCall with output and text error
        let mcp_call = OutputItem::McpCall(McpCall {
            id: "mcp_1".to_owned(),
            server_label: "srv".to_owned(),
            name: "tool1".to_owned(),
            arguments: "{}".to_owned(),
            status: Some(McpCallStatus::Completed),
            approval_request_id: None,
            output: Some("result_output".to_owned()),
            error: Some(McpCallError::Text("error_msg".to_owned())),
        });
        assert_eq!(
            retained_output_item_bytes(&mcp_call),
            RETAINED_CONTAINER_OVERHEAD_BYTES
                + "mcp_1".len()
                + "srv".len()
                + "tool1".len()
                + "{}".len()
                + "result_output".len()
                + "error_msg".len()
        );

        // McpListTools
        let mcp_list = OutputItem::McpListTools(McpListTools {
            id: "list_1".to_owned(),
            server_label: "srv".to_owned(),
            tools: vec![McpListTool {
                name: "test_tool".to_owned(),
                description: Some("a tool".to_owned()),
                input_schema: serde_json::json!({"type": "object", "prop": "val"}),
                annotations: None,
            }],
            error: Some("list_err".to_owned()),
        });
        assert!(retained_output_item_bytes(&mcp_list) > RETAINED_CONTAINER_OVERHEAD_BYTES + "list_1".len());

        // Reasoning with encrypted content
        let reasoning = OutputItem::Reasoning(ReasoningOutput {
            id: "rs_1".to_owned(),
            status: Some("completed".to_owned()),
            content: vec![ReasoningTextContent::new("thought")],
            summary: vec![],
            encrypted_content: Some(serde_json::Value::String("encrypted_blob".to_owned())),
        });
        assert_eq!(
            retained_output_item_bytes(&reasoning),
            RETAINED_CONTAINER_OVERHEAD_BYTES
                + "rs_1".len()
                + "encrypted_blob".len()
                + RETAINED_CONTAINER_OVERHEAD_BYTES
                + "thought".len()
        );
    }
}
