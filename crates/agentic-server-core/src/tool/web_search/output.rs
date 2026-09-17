//! Mapping from the model-facing `web_search` tool call and output to the
//! public `web_search_call` output item.
//!
//! Every provider produces the same tool-output JSON, so this mapping is
//! provider-neutral: it reads the queries and result URLs back out of the
//! serialized output and never touches provider responses directly.

use serde_json::Value;

use super::super::handler::ToolOutput;
use crate::types::io::OutputItem;
use crate::types::io::output::{FunctionToolCall, WebSearchCall, WebSearchCallStatus, WebSearchSource};

#[must_use]
pub(crate) fn output_item(
    call: &FunctionToolCall,
    output: &ToolOutput,
    status: WebSearchCallStatus,
) -> Option<OutputItem> {
    let parsed_output = serde_json::from_str::<Value>(&output.output).ok();
    let queries = parsed_output
        .as_ref()
        .and_then(queries_from_value)
        .or_else(|| queries_from_arguments(&call.arguments))
        .unwrap_or_else(|| vec![String::new()]);
    let sources = parsed_output.as_ref().map(sources_from_output).unwrap_or_default();
    WebSearchCall::try_new(call_output_id(call), status, queries, sources)
        .map(OutputItem::WebSearchCall)
        .ok()
}

#[must_use]
pub(crate) fn started_output_item(call: &FunctionToolCall) -> Option<OutputItem> {
    WebSearchCall::try_new(
        call_output_id(call),
        WebSearchCallStatus::InProgress,
        queries_from_arguments(&call.arguments).unwrap_or_else(|| vec![String::new()]),
        Vec::new(),
    )
    .map(OutputItem::WebSearchCall)
    .ok()
}

fn clean_json_str(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn call_output_id(call: &FunctionToolCall) -> String {
    if let Some(suffix) = call.id.strip_prefix("fc_").filter(|suffix| !suffix.is_empty()) {
        return format!("ws_{suffix}");
    }
    if let Some(suffix) = call.call_id.strip_prefix("call_").filter(|suffix| !suffix.is_empty()) {
        return format!("ws_{suffix}");
    }
    crate::utils::uuid7_str("ws_")
}

fn queries_from_value(value: &Value) -> Option<Vec<String>> {
    let queries: Vec<String> = value
        .get("queries")?
        .as_array()?
        .iter()
        .filter_map(|item| clean_json_str(Some(item)))
        .collect();
    (!queries.is_empty()).then_some(queries)
}

fn queries_from_arguments(arguments: &str) -> Option<Vec<String>> {
    let args = serde_json::from_str::<Value>(arguments).ok()?;
    queries_from_value(&args).or_else(|| clean_json_str(args.get("query")).map(|query| vec![query]))
}

fn sources_from_output(output: &Value) -> Vec<WebSearchSource> {
    ["web", "news"]
        .into_iter()
        .filter_map(|section| output.get("results")?.get(section)?.as_array())
        .flat_map(|results| results.iter())
        .filter_map(source_from_result)
        .collect()
}

fn source_from_result(result: &Value) -> Option<WebSearchSource> {
    let url = clean_json_str(result.get("url"))?;
    Some(WebSearchSource {
        url,
        title: clean_json_str(result.get("title")),
    })
}
