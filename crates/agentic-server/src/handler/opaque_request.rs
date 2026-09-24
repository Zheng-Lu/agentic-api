//! Transport-level wire guard for the closed opaque reasoning candidate.
//!
//! The ordinary Responses decoder intentionally tolerates provider extensions.
//! Only the selected opaque profile needs to reject fields that decoder would
//! otherwise discard before the executor's typed parameter preflight.

use serde::Deserialize;
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, Visitor};
use serde_json::Deserializer;

use agentic_core::types::reasoning_replay::ReasoningReplayError;

/// The WebSocket transport adds three envelope keys to the Responses request.
#[derive(Clone, Copy)]
pub(super) enum OpaqueRequestTransport {
    Http,
    WebSocket,
}

const REQUEST_FIELDS: &[&str] = &[
    "model",
    "input",
    "instructions",
    "previous_response_id",
    "conversation_id",
    "tools",
    "tool_choice",
    "stream",
    "store",
    "include",
    "reasoning",
    "text",
    "temperature",
    "top_p",
    "max_output_tokens",
    "ignore_eos",
    "truncation",
    "metadata",
    "parallel_tool_calls",
    "cache_salt",
    "context_management",
];
const WEBSOCKET_FIELDS: &[&str] = &["type", "stream_id", "generate"];

/// Reject unknown and duplicate top-level keys before `RequestPayload` drops them.
/// The body/frame byte limits are enforced by the transport before this scan.
pub(super) fn validate_opaque_request_fields(
    bytes: &[u8],
    transport: OpaqueRequestTransport,
) -> Result<(), ReasoningReplayError> {
    let mut deserializer = Deserializer::from_slice(bytes);
    RequestFields(transport)
        .deserialize(&mut deserializer)
        .and_then(|()| deserializer.end())
        .map_err(|_| ReasoningReplayError::UnsupportedWireField)
}

struct RequestFields(OpaqueRequestTransport);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReasoningFields {
    context: Option<IgnoredAny>,
    effort: Option<IgnoredAny>,
    generate_summary: Option<IgnoredAny>,
    mode: Option<IgnoredAny>,
    summary: Option<IgnoredAny>,
}

impl<'de> DeserializeSeed<'de> for RequestFields {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for RequestFields {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a Responses request with supported, unique fields")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
        let mut seen = 0_u32;
        while let Some(key) = map.next_key::<String>()? {
            let index = REQUEST_FIELDS
                .iter()
                .position(|field| *field == key)
                .or_else(|| match self.0 {
                    OpaqueRequestTransport::Http => None,
                    OpaqueRequestTransport::WebSocket => WEBSOCKET_FIELDS
                        .iter()
                        .position(|field| *field == key)
                        .map(|index| REQUEST_FIELDS.len() + index),
                });
            let Some(index) = index else {
                return Err(serde::de::Error::custom("unsupported request field"));
            };
            let bit = 1_u32 << index;
            if seen & bit != 0 {
                return Err(serde::de::Error::custom("duplicate request field"));
            }
            seen |= bit;
            if key == "reasoning" {
                let reasoning = map.next_value::<Option<ReasoningFields>>()?;
                if let Some(reasoning) = reasoning {
                    let _ = (
                        reasoning.context,
                        reasoning.effort,
                        reasoning.generate_summary,
                        reasoning.mode,
                        reasoning.summary,
                    );
                }
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{OpaqueRequestTransport as Transport, validate_opaque_request_fields as validate};

    #[test]
    fn selected_profile_rejects_unknown_and_duplicate_top_level_fields() {
        for request in [
            br#"{"model":"m","input":"hi","unqualified":true}"#.as_slice(),
            br#"{"model":"m","model":"n","input":"hi"}"#.as_slice(),
            br#"{"model":"m","input":"hi","\u0073tore":true,"store":false}"#.as_slice(),
        ] {
            assert!(validate(request, Transport::Http).is_err());
        }
    }

    #[test]
    fn websocket_envelope_is_allowed_only_on_websocket() {
        let request = br#"{"type":"response.create","stream_id":"lane","generate":false,"model":"m","input":"hi"}"#;
        assert!(validate(request, Transport::WebSocket).is_ok());
        assert!(validate(request, Transport::Http).is_err());
        assert!(validate(br#"{"type":"response.create","type":"other"}"#, Transport::WebSocket).is_err());
    }

    #[test]
    fn reasoning_object_rejects_discarded_fields() {
        assert!(validate(br#"{"reasoning":{"effort":"low"}}"#, Transport::Http).is_ok());
        assert!(validate(br#"{"reasoning":{"effort":"low","surprise":true}}"#, Transport::Http).is_err());
        assert!(validate(br#"{"reasoning":{"effort":"low","effort":"high"}}"#, Transport::Http).is_err());
    }

    #[test]
    fn only_one_complete_json_object_is_accepted() {
        assert!(validate(br#"{"model":"m","input":"hi"}"#, Transport::Http).is_ok());
        assert!(validate(br#"{"model":"m"} {}"#, Transport::Http).is_err());
        assert!(validate(br#"["not an object"]"#, Transport::Http).is_err());
    }
}
