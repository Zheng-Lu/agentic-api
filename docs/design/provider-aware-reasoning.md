# Provider-aware reasoning replay: implementation slices

Tracking issue: [#335](https://github.com/vllm-project/agentic-api/issues/335).

## Completed foundation: typed reasoning

This slice does **not** enable opaque reasoning replay. The executor still uses the
existing vLLM replay policy: join usable plaintext content, omit summaries from the
upstream copy, and reject opaque-only continuation before normal inference.
Canonical public and persisted items retain their complete reasoning representation.

`types/io/reasoning.rs` owns the new pure wire types:

- `ReasoningTextContent` has a closed `reasoning_text` discriminator.
- `ReasoningSummaryContent` has a closed `summary_text` discriminator and string text.
- `ReasoningStatus` accepts `in_progress`, `completed`, and `incomplete`.
- `OpaqueReasoning` preserves `encrypted_content` as an exact string. It has no
  decoding or normalization operation, redacts `Debug`, and rejects values larger
  than 16 MiB of decoded UTF-8. This is a gateway ceiling, not a provider limit.

These types replace arbitrary JSON in `ReasoningOutput`. Existing Rust callers
constructing fields directly must use the new types. Missing or null `content` and
`summary` still deserialize as empty arrays; missing or null encrypted state and
status remain `None`. Ingestion retains the existing completion distinction:
omitted arrays preserve accumulated parts, while explicit null or empty arrays clear
them. Public JSON field names and valid string-valued opaque state do not change.

The existing request/body ceilings and shared retained-response budget still apply.
Reasoning parts, including empty parts, carry a structural charge; all retained text
and opaque bytes are charged through `RetainedSize`. No new queue, task, parser,
delivery path, or inference policy is introduced. The 16 MiB per-value ceiling is not
a claim about aggregate process memory; the usual retained-response budget is smaller.

Both stores now return `StorageError::InvalidHistoryItem` instead of skipping a row
that fails item decoding. Response history also rejects a missing referenced row.
This prevents legacy malformed reasoning from silently disappearing after schema
tightening. Database rows are neither rewritten nor deleted, and no SQL migration
was required for that foundation. Valid existing records keep their wire representation.
Response history references and effective metadata now also decode fallibly. Malformed
JSON, wrong field types, and explicit JSON `null` fail closed instead of becoming empty
history or default settings. SQL NULL retains its existing legacy behavior; it does not
establish replay provenance. A missing captured conversation response or a reference
to another conversation is an error when loading versioned metadata. Parse diagnostics
are intentionally excluded from storage errors because they may echo stored secrets.
These checks do not establish provider identity or authorize opaque replay.

## Completed: server policy and per-item provenance

Opaque replay remains disabled. Server configuration now accepts an explicit typed
`responses.reasoning_replay_policy`; its default and only executable value is
`vllm_plaintext`. The reserved `opaque_responses` value returns a typed core error
before rehydration, tool discovery, inference, compaction, or external commit. Startup
also rejects it before opening storage. No policy is inferred from the request model.

```toml
[responses]
reasoning_replay_policy = "vllm_plaintext"
```

`types/reasoning_replay.rs` owns the versioned `ReasoningProvenance` envelope:

- SQL NULL means unknown legacy origin. It is never backfilled or upgraded implicitly.
- `ClientSubmitted` marks manually supplied reasoning input and externally committed
  output. Receiving a successful upstream response does not upgrade those items.
- `Upstream` marks only output observed through the gateway's inference path. The
  engine attaches it after the existing JSON/SSE ingestion path has assembled output,
  before tool-round history, checkpoints, and persistence consume that output.

Each upstream observation includes its policy and a fixed-size SHA-256 identity
fingerprint. Domain-separated, fixed-width component hashes bind the configured
Responses endpoint, effective per-request credential (including missing vs empty),
requested model, and an optional consistently reported terminal model. The latter now
comes from upstream metadata, separately from the public `response.model` that the
pipeline rebuilds from the request. Unknown and reported model identities hash
differently. Credential rotation, endpoint changes, policy changes,
and requested-model changes also produce distinct identities. Neither credentials
nor original identity strings are stored, and identity `Debug` output is redacted.
This is an equality fingerprint, not an authorization grant, integrity MAC, provider
attestation, or model-family compatibility claim. It observes the configured endpoint,
not any final redirect destination. Redirect policy, approved snapshots, opaque format identity, and
compatible-family rules remain enablement prerequisites.

`ReasoningOutput.replay_provenance` is skipped on both serialization and
deserialization and is absent from OpenAPI. Client or upstream JSON cannot set it.
Internal output-to-input conversion and transient session forks preserve it; manual
resubmission and the public split-execution persistence APIs always demote new output
to client-submitted origin. Existing ancestor items retain their own origin through
ephemeral-to-durable promotion and branching.

Migration `0005_reasoning_provenance.sql` adds nullable `items.reasoning_provenance`
TEXT, separately from public item JSON. It rewrites no existing data and establishes
no provenance for legacy rows. Both stores insert and restore it atomically with each
item, including batched inserts. Unknown versions/fields, malformed or oversized
envelopes (maximum 512 UTF-8 bytes), and provenance on non-reasoning items fail closed
with redacted `InvalidHistoryItem` errors. Missing provenance remains readable under
the default vLLM policy but cannot qualify opaque state for future replay.

Supervisor-managed schemas must apply migration 0005 before this gateway starts.
Startup compatibility checks and readiness probes require the new column. Do not
drop it on rollback: older writers can leave NULL, which must remain unknown to a
future opaque profile. No public JSON field or SSE/WebSocket event is added.

Shared retained-response accounting and session checkpoint budgets charge fixed
inline provenance space for every reasoning item, even before it has an origin.
Checkpoint limits cover serialized bytes plus this fixed non-wire charge, not total
heap memory. Existing ownership, reservation/refund, cancellation and drop behavior
is unchanged. This slice adds no task, queue, parser, or client emission path.

Rust callers using struct literals must supply the new `ResponsesConfig` policy
(or use `..ResponsesConfig::default()`) and `ReasoningOutput` provenance (prefer
`ReasoningOutput::new`). The latter remains re-exported at its existing paths.
Raw storage `Item` rows now include the nullable provenance column; low-level item
insertion is crate-private so callers use typed `ResponseStore`/`ConversationStore`
operations instead of supplying arbitrary serialized SQL item data.

## Completed: upstream-reported model evidence

`types/upstream_identity.rs` defines the bounded `UpstreamModelId`, safe typed
`UpstreamModelError`, and internal `IngestedResponse` result. Model identifiers retain
their exact spelling: there is no alias resolution, normalization, or request-model
fallback. The gateway rejects empty/whitespace-only names and names exceeding 1024
decoded UTF-8 bytes. This ceiling is a gateway limit, not an OpenAI protocol limit.

JSON response bodies and SSE lifecycle response objects use the same typed model
projection. Normalization extracts SSE metadata; synchronous ingestion checks that
all supplied names agree within a round. Malformed metadata returns a redacted
`upstream_error` (HTTP 502 for blocking requests); strict ingestion also rejects a
changed name. Lenient ingestion preserves compatibility but permanently invalidates
model evidence on a conflict. The existing gateway reasoning cassette demonstrates
why: its early events report `gpt-5.6-sol`, while its terminal response echoes the
requested `gpt-5.6` alias. Neither spelling may supply model evidence for that round.
Failed ingestion does not persist a response or publish a session checkpoint. A streaming caller can already
have received earlier valid events before the final error.

Only an explicit terminal JSON status or SSE event with a model supplies evidence.
Missing/null metadata, nonterminal JSON, and lenient completion at EOF remain unknown,
even if earlier metadata or the request names a model. Strict ingestion rejects
post-terminal events as before. Lenient ingestion retains its repeated-snapshot
compatibility but permanently invalidates model evidence after any post-terminal
semantic event. This observation is not a substitute for the strict lifecycle
validation required by a future opaque profile.

One bounded model string is retained and charged once per round under the existing
shared retained-response budget; repeated matching snapshots do not double-charge.
The consuming ingestion result carries it separately to the engine, which includes
it in that round's provenance fingerprint. Inference framing and ordered delivery do
not inspect or decide model identity. No queue, worker, parser, client emission path,
database migration, or public response field is added. Public completed response
model naming remains unchanged; Rust callers constructing `EventPayload::Response`
must now supply its typed optional `model` field.

Old provenance is not rewritten: observations without a reported model retain their
original unknown-model fingerprint. A provider's self-reported name does not attest
its backend, establish compatible model families, or authorize opaque replay.
Opaque replay remains disabled. The metadata projection follows the response objects
in the [official streaming reference](https://developers.openai.com/api/reference/resources/responses/streaming-events).

## Current slice: candidate profile and compatibility preflight

The server now accepts one closed **candidate**, not an enabled capability:

```toml
[responses]
reasoning_replay_policy = "opaque_responses"
reasoning_replay_profile = "openai_gpt_5_4_2026_03_05_v1"
```

**This configuration intentionally fails startup with `OpaqueNotEnabled`.** There
is no environment variable, feature flag, or request field that bypasses the gate.
Omitting the profile under `opaque_responses` fails with `MissingProfile`; adding
one under `vllm_plaintext` fails with `UnexpectedProfile`. Default/generated
configuration remains vLLM-only. The profile's pinned model is listed in the
[official GPT-5.4 model documentation](https://developers.openai.com/api/docs/models/gpt-5.4).

`types/reasoning_profile.rs::OpaqueReasoningProfile` pins the exact endpoint
`https://api.openai.com/v1/responses`, requested model and terminal reported model
`gpt-5.4-2026-03-05`, and the Responses `reasoning.encrypted_content` contract. Its
`v1` is a gateway compatibility revision, **not** a provider encryption-format
version. No ciphertext is decoded, inspected for a format marker, or translated.
Aliases, alternate/regional endpoints, explicit port spellings, trailing slashes,
query strings, and URL credentials do not match. The strict spelling is intentional;
profile validation does not perform URL normalization or model-family inference.

`executor/replay.rs` owns the orchestration checks:

- Before rehydration, validate policy/profile consistency, exact target, and absence
  of local compaction input, triggers, or nonempty `context_management`, then enforce
  availability. Rejection precedes storage lookup and tool discovery.
- After rehydration and before each JSON/SSE inference entry point, the same
  preflight checks the canonical history against the selected profile and effective
  nonempty bearer credential, then enforces availability again. The opaque positive
  path is exercised directly in unit tests only: normal execution still stops at
  the earlier availability gate.
- Per-item checks reject unknown/client-submitted provenance, another policy or
  identity, missing/empty opaque state, and in-progress reasoning. Matching checks
  borrow input without cloning, mutation, filtering, or reordering. Plaintext or a
  summary cannot establish opaque compatibility.
- The engine's post-ingestion observation is fallible for a candidate opaque profile:
  a reasoning-bearing round requires exact, consistently reported terminal model
  evidence before any of its reasoning items receives profile provenance.

Profile identities hash a separate domain, the fixed compatibility-contract domain,
and the existing routing/credential/model observation. Thus old observational
fingerprints do not qualify, even if their endpoint, model, and credential match.
Credential rotation deliberately invalidates compatibility. This fingerprint is
still an equality check, not an authorization token or provider attestation. The
target retains only a fixed-size digest and profile enum; neither credentials nor
opaque strings enter errors or `Debug`. No extra collections, queues, or tasks are
introduced. Existing provenance storage/budget size is unchanged; no migration or
legacy rewrite is needed.

Replay errors use the existing HTTP/SSE error machinery with the machine code
`reasoning_replay_incompatible`. Invalid input/model/credential combinations are
400 errors, invalid reported model evidence is a 502, and server configuration or
unavailable execution is a 500. Messages are static and redact item IDs, URLs,
credentials, model input, and opaque state. Input/model errors identify the relevant
parameter when unambiguous.

This slice does **not** implement opaque request projection, upstream `store: false`,
strict profile ingestion, redirect suppression, or qualification. It does not claim
that a configured HTTP client, proxy, default authorization header, or organization/
project header is covered by the bearer-only fingerprint. Those transport constraints
must be settled before enablement. vLLM projection, public output, persistence,
session behavior, SSE framing/normalization/ingestion/delivery, and cancellation
ownership remain unchanged. Rust struct literals for `ResponsesConfig` must include
the optional profile or use `..ResponsesConfig::default()`.

## Remaining slices before enabling a provider profile

1. Qualify the candidate profile and enforce its transport constraints, including
   redirect suppression and effective credential/header identity. The typed profile
   and pre-inference provenance checks are implemented, but passing them alone does
   not enable execution.
2. Project compatible reasoning only in the upstream request copy. Keep the single
   `OutputItem::to_input_item` conversion and existing ingestion path. Select strict
   terminal validation for the opaque profile rather than introducing a second
   state machine. Retain the vLLM default.
3. Separate local plaintext compaction checkpoints from provider opaque compaction.
   Reject unsupported combinations before inference. Qualify tool-round ordering,
   branching, HTTP/SSE, and transient WebSocket continuations with recorded exchanges.
4. Close the streaming producer abort/join gap with #244 before production enablement.

The upstream contract requires preserving opaque state and limits reasoning reuse
to compatible model families; see the
[official reasoning guide](https://developers.openai.com/api/docs/guides/reasoning).
The gateway must not infer compatibility from the presence of `encrypted_content`.

## Verification

`reasoning_test.rs` replays the existing recorder-generated OpenAI and gateway JSON
and SSE cassettes and checks content, summaries, opaque state, status, identity, and
the output-to-input conversion. `reasoning_types_test.rs` covers schema rejection,
nullability, exact string round trips, redaction, and the decoded-byte ceiling.
Accumulator tests cover malformed strict completion and retained-budget exhaustion,
including arrays of empty typed parts. Storage tests cover invalid and missing rows;
stateful and session tests retain the existing vLLM continuation behavior.
`storage_response_integrity_test.rs` additionally verifies invalid metadata and history
references, legacy SQL NULL handling, missing/foreign captured turns, error redaction,
and refusal to persist a child of an invalid parent. A recorded initial exchange checks
that malformed continuation metadata fails before either JSON or SSE inference starts.

No captured YAML was hand-authored or modified for this slice. Future provider replay
scenarios must use the cassette README's recorder workflow and staged validation.

The `reasoning_provenance_*_test.rs` suites cover policy gating, JSON/OpenAPI exclusion,
closed envelope decoding, exact opaque bytes, mixed-origin storage batches, branches,
corrupt history, and a real pre-0005 SQLite upgrade with repeated startup. Execution
tests replay existing recorder-generated Qwen and OpenAI JSON/SSE exchanges, checking
durable and transient history, cancelled forks, promotion, and external-commit
demotion. Unit tests additionally exercise fingerprint separation and non-wire budget
charges/refunds. These local replays are not live provider qualification.

`upstream_model_provenance_test.rs` replays the recorder-generated Qwen JSON/SSE
exchanges on one endpoint with an unchanged request alias. In-memory fault injection
proves that changing only the reported model changes persisted provenance, JSON/SSE
identities match, missing/null/conflicting metadata stays unknown under lenient
ingestion, and malformed terminal metadata emits an error without storing a response.
Pipeline tests cover malformed
metadata, exact UTF-8 bounds, strict/lenient terminal handling, round isolation,
retained-budget exhaustion, upstream disconnect, and client backpressure/drop.

`reasoning_profile_test.rs`, server TOML tests, and `executor/replay/profile/tests.rs`
cover closed profile selection, exact target rejection before history/tool/inference
work for JSON/SSE requests, availability gating, identity separation from legacy
observations, credential rotation, redacted error envelopes, manual/legacy rejection,
missing opaque state, compaction rejection, and unchanged input bytes/call order.
Profile observation tests prove missing or mismatched terminal evidence cannot stamp
items. These are local compatibility and fault-injection tests, not recorded OpenAI
qualification; they do not call a live provider or create captured YAML.

The workspace suite (including OpenAPI and cassette tests), Clippy with warnings
denied, and formatting checks passed with Rust 1.98. Opt-in ignored tests were not run:

```bash
cargo test --workspace --offline
cargo clippy --workspace --all-targets --offline -- -D warnings
cargo fmt --all -- --check
```

Rust 1.85 verification currently stops at dependency MSRV checks: the existing
lockfile includes dependencies requiring Rust 1.86–1.88. This slice does not change
upstream dependency versions; `sha2` 0.10.9 was already locked and is now also a direct
core dependency. Dependency and baseline language compatibility need a separate
MSRV repair before the repository can claim the documented 1.85 release gate.
