//! Provider layer: OpenAI-compatible chat calls with streaming + LiteLLM
//! telemetry extraction (exact per-call cost, served backend, fallbacks).

use crate::config::{Auth, Provider, DEFAULT_ACCOUNT_ID};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::time::Duration;
use tokio::time::{timeout, Instant};

/// Anthropic Messages API version header. Pinned to the value the wire
/// adapter always sends; Anthropic's docs guarantee this version is honored
/// indefinitely. The constant lives here so the request builder and the
/// tests share the exact same string.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Anthropic Messages API wire suffix (the `/v1/messages` path). The
/// existing `endpoint_url` helper adds the `/v1` segment when the configured
/// `base_url` does not already include one, so passing `"messages"` here
/// yields the spec-correct `{base_url}/v1/messages` URL for both
/// `https://api.anthropic.com` and `https://api.anthropic.com/v1`.
const ANTHROPIC_MESSAGES_SUFFIX: &str = "messages";

/// OpenAI Responses API wire suffix (the `/v1/responses` path). Same
/// `/v1`-injection contract as [`ANTHROPIC_MESSAGES_SUFFIX`]: passing
/// `"responses"` to the shared `endpoint_url` helper yields the
/// spec-correct `{base_url}/v1/responses` URL for both
/// `https://api.openai.com` and `https://api.openai.com/v1`. The
/// constant is only used by the `kind == "openai-responses"` branch;
/// the chat-completions branch stays on the literal `"chat/completions"`
/// suffix it was hard-coded to before this slice landed.
const RESPONSES_SUFFIX: &str = "responses";

/// Default overall request budget (seconds). Override: ZODER_TIMEOUT_S.
const DEFAULT_REQUEST_TIMEOUT_S: u64 = 120;
/// Default stream inactivity budget (seconds). A model that stops emitting for
/// this long is treated as stalled. Override: ZODER_IDLE_S.
const DEFAULT_IDLE_TIMEOUT_S: u64 = 25;
/// Max bytes for a single SSE line before we treat the stream as hostile.
const MAX_LINE_BYTES: usize = 1 << 20; // 1 MiB
/// Max bytes buffered without a line terminator before we bail.
const MAX_BUFFER_BYTES: usize = 16 << 20; // 16 MiB
/// Maximum wire bytes accepted on any provider response path.
const MAX_RESPONSE_BYTES: usize = 16 << 20; // 16 MiB
/// Independent ceiling for decoded answer text accumulated across SSE frames.
const MAX_CONTENT_BYTES: usize = 8 << 20; // 8 MiB

fn env_secs(var: &str, default: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Classified failure mode of a single provider call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrKind {
    /// Connect/overall/idle deadline exceeded.
    Timeout,
    /// HTTP 429 (rate-limited / quota).
    RateLimit,
    /// HTTP 5xx (server-side, usually transient).
    Server,
    /// Transport/connection error.
    Network,
    /// HTTP 4xx other than 429 (caller error; not retryable).
    Http,
    /// Malformed/undecodable response.
    Decode,
}

impl Default for ErrKind {
    /// Decode is the most neutral "we don't know what happened" default —
    /// it's a no-information outcome that resolve to `Classification::Error`
    /// via `classify_err`, which is exactly the right fallback when a caller
    /// has to construct a `ProviderError` via `..Default::default()`. Picking
    /// a more specific variant (Timeout / Server / Network) would silently
    /// bias the breaker signal for the "no information" code paths, so we
    /// deliberately pick the most-generic one.
    fn default() -> Self {
        ErrKind::Decode
    }
}

/// A provider call failure with enough structure to drive retries + fallback.
#[derive(Debug, Clone, Default, thiserror::Error)]
#[error("{message}")]
pub struct ProviderError {
    pub message: String,
    pub kind: ErrKind,
    pub status: Option<u16>,
    pub retry_after: Option<Duration>,
    /// True when answer bytes were already streamed to the sink: a retry of the
    /// SAME model would duplicate visible output, so the caller must not retry
    /// it (it may still fall back to a different model).
    pub emitted: bool,
    /// Y-14: raw payload from an Anthropic mid-stream `event: error` frame.
    /// The Anthropic wire protocol surfaces 401/429/529 rejections as an SSE
    /// error frame carried by a 200 response, so the HTTP status we attach
    /// to the *chat* call is `None`. Without carrying the typed envelope
    /// alongside the error, `classify_err` can only see `ErrKind::Http` /
    /// `status: None` and lumps `overloaded_error` and `rate_limit_error`
    /// into the breaker-tripping `Error` bucket. `classify_err` checks this
    /// field FIRST (before `classify_err_kind`) so an
    /// `overloaded_error` envelope classifies as `Capacity` instead of
    /// `Error`, the same way a 529 from the headers does.
    ///
    /// `Some` only when the Anthropic SSE parser surfaced an inline error
    /// frame; `None` for every other path (HTTP headers, OpenAI-style
    /// envelopes, network failures, etc.).
    pub anthropic_error_body: Option<String>,
}

impl ProviderError {
    fn new(kind: ErrKind, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind,
            status: None,
            retry_after: None,
            emitted: false,
            anthropic_error_body: None,
        }
    }
    /// Transient and safe to retry on the same model (nothing emitted yet).
    pub fn retryable(&self) -> bool {
        !self.emitted
            && matches!(
                self.kind,
                ErrKind::Timeout | ErrKind::RateLimit | ErrKind::Server | ErrKind::Network
            )
    }
}

/// Backoff before retry `attempt` (0-based): exponential (base 500ms) capped at
/// 16s, with +/-20% jitter, but never less than a server-provided `Retry-After`.
pub fn backoff_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    use rand::Rng;
    let base_ms = 500u64.saturating_mul(1u64 << attempt.min(5));
    let capped = base_ms.min(16_000);
    let jitter = rand::thread_rng().gen_range(0.8..1.2);
    let mut delay = Duration::from_millis((capped as f64 * jitter) as u64);
    if let Some(ra) = retry_after {
        if ra > delay {
            delay = ra;
        }
    }
    delay
}

/// Upper bound on an honored `Retry-After`: a hostile/misconfigured backend
/// could send an enormous value and pin the CLI in `sleep` for ~forever, so we
/// never wait longer than this (we just retry early / fall back instead).
const MAX_RETRY_AFTER_SECS: u64 = 60;

fn retry_after_header(h: &reqwest::header::HeaderMap) -> Option<Duration> {
    let v = h
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .to_string();
    // Only the numeric-seconds form is honored (HTTP-date form is rare here).
    v.parse::<u64>()
        .ok()
        .map(|s| Duration::from_secs(s.min(MAX_RETRY_AFTER_SECS)))
}

fn classify_reqwest(e: reqwest::Error) -> ProviderError {
    let kind = if e.is_timeout() {
        ErrKind::Timeout
    } else if e.is_connect() {
        ErrKind::Network
    } else if e.is_decode() {
        ErrKind::Decode
    } else {
        ErrKind::Network
    };
    ProviderError::new(kind, e.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub max_tokens: u32,
    /// Optional sampling temperature. `Some(v)` forwards `v` to the
    /// wire so a deterministic reviewer / health-probe call can pin
    /// `Some(0.0)`. `None` lets the model pick its own default
    /// temperature — the Anthropic Messages API, for example, treats
    /// the absence of the field as "use the model default", and a
    /// stale literal `0.0` from a previous request would silently
    /// downgrade the call. The Responses and OpenAI chat paths
    /// forward `Some(_)` verbatim and omit the field on `None` to
    /// match the Anthropic contract.
    pub temperature: Option<f32>,
    pub stream: bool,
    /// Surface model reasoning/thinking in the returned content. Off by default
    /// so the answer surface stays codex-compatible.
    pub show_reasoning: bool,
    /// Requested reasoning effort passed through to the backend (OpenAI-style
    /// `reasoning_effort`: e.g. `minimal` | `low` | `medium` | `high`, or
    /// `none` to ask the model to skip thinking). `None` leaves it unset so the
    /// model uses its own default.
    pub reasoning_effort: Option<String>,
}

/// Telemetry parsed from LiteLLM response headers (authoritative, no guessing).
#[derive(Debug, Clone, Default, Serialize)]
pub struct CallTelemetry {
    pub served_model: Option<String>,
    pub api_base: Option<String>,
    pub attempted_fallbacks: Option<i64>,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<f64>,
    pub key_spend: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct ChatResult {
    pub content: String,
    /// Best-effort output token count: real `completion_tokens` from usage when
    /// the backend reports it, otherwise a streamed-chunk count (see
    /// `completion_tokens` for the authoritative value).
    pub tokens_out: u64,
    /// Authoritative prompt token count from the response `usage`, if present.
    pub prompt_tokens: Option<u64>,
    /// Authoritative completion token count from the response `usage`, if present.
    pub completion_tokens: Option<u64>,
    /// Prompt tokens served from a provider cache, when usage telemetry
    /// reports them. This is a subset of `prompt_tokens`.
    pub cached_prompt_tokens: Option<u64>,
    pub telemetry: CallTelemetry,
}

fn header_f64(h: &reqwest::header::HeaderMap, k: &str) -> Option<f64> {
    h.get(k)?.to_str().ok()?.trim().parse().ok()
}
fn header_i64(h: &reqwest::header::HeaderMap, k: &str) -> Option<i64> {
    h.get(k)?.to_str().ok()?.trim().parse().ok()
}
fn header_str(h: &reqwest::header::HeaderMap, k: &str) -> Option<String> {
    Some(h.get(k)?.to_str().ok()?.to_string())
}

fn telemetry_from_headers(h: &reqwest::header::HeaderMap) -> CallTelemetry {
    CallTelemetry {
        served_model: header_str(h, "x-litellm-model-id"),
        api_base: header_str(h, "x-litellm-model-api-base"),
        attempted_fallbacks: header_i64(h, "x-litellm-attempted-fallbacks"),
        cost_usd: header_f64(h, "x-litellm-response-cost-original"),
        duration_ms: header_f64(h, "x-litellm-response-duration-ms"),
        key_spend: header_f64(h, "x-litellm-key-spend"),
    }
}

#[derive(Deserialize, Default)]
struct Usage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    /// Anthropic-compatible gateways commonly expose this at the usage root.
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

#[derive(Deserialize, Default)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

impl Usage {
    fn is_meaningful(&self) -> bool {
        self.prompt_tokens.is_some()
            || self.completion_tokens.is_some()
            || self.cached_prompt_tokens().is_some()
    }

    fn cached_prompt_tokens(&self) -> Option<u64> {
        self.prompt_tokens_details
            .as_ref()
            .and_then(|details| details.cached_tokens)
            .or(self.cache_read_input_tokens)
    }
}

#[derive(Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}
#[derive(Deserialize)]
struct StreamChoice {
    delta: Delta,
}
#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
}

/// Non-streaming chat-completion response shape (subset we consume).
#[derive(Deserialize)]
struct ChatCompletion {
    #[serde(default)]
    choices: Vec<CompletionChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}
#[derive(Deserialize)]
struct CompletionChoice {
    // The inner `message` field is `Option<CompletionMessage>` so the
    // two invalid shapes serialize/deserialize into distinct values,
    // letting [`CompletionChoice::has_meaningful_message`] reject both:
    //  - absent or `null` -> `message = None`
    //  - present but `{}` -> `message = Some(CompletionMessage::default())`
    //    (every content / reasoning field is None, so the validate
    //    helper still returns false)
    // This mirrors the streaming path's `saw_choice` guard: a 2xx body
    // whose choices array contains only `{}` placeholders MUST surface
    // as a Decode error rather than a successful empty completion, so a
    // `--no-stream` / reviewer / health-probe call cannot exit Ok with
    // empty content and let a billable reservation be reconciled as a
    // no-op.
    #[serde(default)]
    message: Option<CompletionMessage>,
}
#[derive(Deserialize, Default)]
struct CompletionMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
}

impl CompletionChoice {
    /// True when this choice carries an actual message whose content (or
    /// any reasoning field, since `show_reasoning = true` is the most
    /// permissive surface) is non-empty/whitespace-only. Used by the
    /// non-streaming path as the analogue of the streaming `saw_choice`
    /// guard so a 2xx body whose choices array contains only `{}`
    /// placeholders surfaces as a `Decode` error rather than a successful
    /// empty completion.
    fn has_meaningful_message(&self) -> bool {
        let Some(msg) = self.message.as_ref() else {
            return false;
        };
        pick_text(
            msg.content.clone(),
            msg.reasoning_content.clone(),
            msg.reasoning.clone(),
            /* show_reasoning = */ true,
        )
        .chars()
        .any(|c| !c.is_whitespace())
    }
}

/// Pick the answer text honoring the codex-compatible default (no reasoning).
fn pick_text(
    content: Option<String>,
    reasoning_content: Option<String>,
    reasoning: Option<String>,
    show_reasoning: bool,
) -> String {
    if show_reasoning {
        content
            .or(reasoning_content)
            .or(reasoning)
            .unwrap_or_default()
    } else {
        content.unwrap_or_default()
    }
}

/// Cap a server-returned string and scrub anything that looks like a secret, so
/// HTTP error bodies / stream-error frames can't leak prompts or credentials
/// into logs/stderr. Best-effort: caps length + replaces common token shapes.
fn redact(s: &str) -> String {
    let capped: String = s.chars().take(200).collect();
    let mut out = String::with_capacity(capped.len());
    for word in capped.split_whitespace() {
        let core = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        let looks_secret = word.starts_with("sk-")
            || word.starts_with("Bearer")
            || (core.len() >= 24
                && core
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        out.push_str(if looks_secret { "[REDACTED]" } else { word });
        out.push(' ');
    }
    out.trim_end().to_string()
}

/// Normalize an endpoint URL: the configured base may or may not already carry
/// the `/v1` version segment, so we never emit `/v1/v1/...`. `azure-openai`
/// uses its own deployment route + `?api-version`.
///
/// For `kind == "azure-openai"`, the `api_version` argument (when `Some`) is
/// the explicit per-provider override (the `Provider::azure_api_version`
/// field, which already won over the env var at [`OpenAiProvider`] construction
/// time). When `None`, the helper falls back to the `AZURE_OPENAI_API_VERSION`
/// env var, then to the built-in default `"2024-10-21"`. The non-Azure
/// branches ignore this argument.
fn endpoint_url(base_url: &str, kind: &str, suffix: &str, api_version: Option<&str>) -> String {
    let base = base_url.trim_end_matches('/');
    if kind == "azure-openai" {
        let ver = api_version
            .map(|s| s.to_string())
            .or_else(|| std::env::var("AZURE_OPENAI_API_VERSION").ok())
            .unwrap_or_else(|| "2024-10-21".to_string());
        return format!("{base}/{suffix}?api-version={ver}");
    }
    if base.ends_with("/v1") || base.contains("/v1/") {
        format!("{base}/{suffix}")
    } else {
        format!("{base}/v1/{suffix}")
    }
}

/// Strip a `Bearer ` prefix (case-insensitive) from a header value so a
/// `kind == "anthropic"` provider can re-use an [`Auth`] whose
/// [`Auth::header_pair`] returns `Authorization: Bearer <token>`. The
/// Anthropic Messages API wants `x-api-key: <token>` with NO `Bearer `
/// prefix. Pure &str -> &str so callers can decide what to do on a None.
fn strip_bearer_prefix(value: &str) -> &str {
    let trimmed = value.trim_start();
    if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("bearer ") {
        &trimmed[7..]
    } else if trimmed.eq_ignore_ascii_case("bearer") {
        ""
    } else {
        value
    }
}

/// Build the Anthropic Messages API wire body. Anthropic's `messages` array
/// only allows role `user`/`assistant` (no `system` inline), so any leading
/// system-role message in `req.messages` is pulled out and rendered as the
/// top-level `system` string field. All other messages pass through with
/// the same `(role, content)` shape OpenAI uses — the field names line up,
/// so the JSON is structurally compatible. `stream` is preserved so the
/// SSE path can request `stream: true` end-to-end.
///
/// `req.temperature` is forwarded when `Some` so a deterministic reviewer
/// / health-probe call that asks for `temperature = 0` actually gets
/// `temperature = 0` on the wire; `None` omits the field entirely so the
/// model picks its own default (matching the Responses / OpenAI chat
/// contract, which also omit the field on `None`).
///
/// `req.reasoning_effort` is dropped for Anthropic: the Messages API has no
/// `reasoning_effort` field, and silently injecting an OpenAI-shaped key
/// would either be ignored or rejected depending on the gateway. The
/// `show_reasoning` surface is also dropped for the same reason — Anthropic
/// exposes thinking content via `content_block_delta.type == "thinking_delta"`
/// and a separate `thinking` block at the request level, which the wire
/// adapter in this slice does not surface (it is a deliberate out-of-scope
/// follow-up so the slice stays focused on parity with the OpenAI path).
fn anthropic_body(req: &ChatRequest) -> serde_json::Value {
    // Split system vs non-system; preserve the original ordering of the
    // user/assistant half (Anthropic rejects an unsorted messages array
    // only if a `system` string appears AFTER a message, but does reject an
    // inline `role: "system"` entry, so the cleanest contract is "lift
    // every leading system message into the top-level string and concat
    // the rest verbatim").
    let mut system_parts: Vec<&str> = Vec::new();
    let mut rest: Vec<&Message> = Vec::new();
    for msg in &req.messages {
        if msg.role == "system" && rest.is_empty() {
            system_parts.push(&msg.content);
        } else {
            rest.push(msg);
        }
    }
    let mut body = serde_json::json!({
        "model": req.model,
        "max_tokens": req.max_tokens,
        "messages": rest,
        "stream": req.stream,
    });
    if !system_parts.is_empty() {
        // Anthropic accepts a plain string OR a structured
        // `[{"type":"text","text":"..."}]` array; the string form is the
        // common case and keeps the JSON identical to what most SDKs send.
        body["system"] = serde_json::Value::String(system_parts.join("\n\n"));
    }
    if let Some(temp) = req.temperature {
        body["temperature"] = serde_json::json!(temp);
    }
    body
}

/// Anthropic Messages API non-streaming response: pull the first text
/// block out of `content` and translate `usage.input_tokens` /
/// `usage.output_tokens` into the OpenAI-shaped `prompt_tokens` /
/// `completion_tokens` fields that [`ChatResult`] already knows how to
/// surface. The schema-invalid-2xx guard from the OpenAI path is preserved
/// here: a response with no `content` array, or a `content` array whose
/// entries all carry non-text types and therefore leave the answer empty,
/// MUST surface as `ErrKind::Decode` (see `consume_full` for the failure
/// mode the streaming path's `saw_choice` guard rejects).
#[derive(Deserialize)]
struct AnthropicResponse {
    #[serde(default)]
    content: Vec<AnthropicContent>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
    /// `stop_reason` is parsed for parity with future caller code that may
    /// want to surface why Anthropic ended the turn (`end_turn`,
    /// `max_tokens`, `stop_sequence`, `tool_use`); the current wire adapter
    /// does not need it. Allowed-dead-code so the field stays documented
    /// and ready without silencing the warning at the call site.
    #[serde(default)]
    #[allow(dead_code)]
    stop_reason: Option<String>,
}
#[derive(Deserialize)]
struct AnthropicContent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}
#[derive(Deserialize, Default)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    /// Anthropic-specific cache telemetry: `cache_creation_input_tokens`
    /// plus `cache_read_input_tokens` are reported on every cache hit and
    /// map cleanly onto the OpenAI-shaped `cached_prompt_tokens` field the
    /// existing utilization capture paths already aggregate.
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
}

impl AnthropicResponse {
    fn has_meaningful_text(&self) -> bool {
        self.content.iter().any(|block| {
            block.kind == "text"
                && block
                    .text
                    .as_deref()
                    .map(|t| t.chars().any(|c| !c.is_whitespace()))
                    .unwrap_or(false)
        })
    }
    fn joined_text(&self) -> String {
        let mut out = String::new();
        for block in &self.content {
            if block.kind == "text" {
                if let Some(text) = &block.text {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                }
            }
        }
        out
    }
    fn cached_prompt_tokens(&self) -> Option<u64> {
        match (
            self.usage.as_ref().and_then(|u| u.cache_read_input_tokens),
            self.usage
                .as_ref()
                .and_then(|u| u.cache_creation_input_tokens),
        ) {
            (Some(read), Some(create)) => Some(read.saturating_add(create)),
            (Some(read), None) => Some(read),
            (None, Some(create)) => Some(create),
            (None, None) => None,
        }
    }
}

/// Build the OpenAI Responses API wire body. The Responses API uses
/// `input` instead of `messages`, `max_output_tokens` instead of
/// `max_tokens`, and a `reasoning: {effort: "..."}` object instead of the
/// top-level `reasoning_effort` string the chat-completions path emits.
/// `temperature` and `stream` keep their field names.
///
/// System-role handling: a leading system message in `req.messages` is
/// emitted inline as a `role: "system"` item in `input` rather than
/// promoted to a top-level `instructions` string. Both shapes are
/// spec-valid for the Responses API; the inline `input[]` shape is
/// chosen because:
///   * it keeps a single message-processing path with the chat branch
///     (no special-case extraction pass),
///   * it survives a follow-up that decides to support multi-system
///     prompts the same way the chat branch already does
///     (concat → item list), and
///   * operators inspecting the wire body see the same `(role, content)`
///     structure the chat-completions branch emits.
///
/// `developer` messages are passed through the same way — the Responses
/// API treats them as a sibling of `system` and accepts them inline.
///
/// `reasoning_effort` (when set) is emitted as a top-level
/// `reasoning: {effort: "..."}` object (NOT `{"summary":"auto", ...}` —
/// the existing chat path passes the raw string and we mirror that
/// minimal contract here so a future change to either branch touches
/// only one place).
///
/// `show_reasoning` is dropped: the Responses API has no `reasoning_content`
/// field in the same shape the chat-completions response emits, and
/// silently injecting an OpenAI-shaped key would either be ignored or
/// rejected depending on the gateway. The `reasoning` summary block is a
/// separate out-of-scope follow-up (mirrors the deliberate
/// `show_reasoning` drop on the Anthropic branch).
fn responses_body(req: &ChatRequest) -> serde_json::Value {
    let input: Vec<serde_json::Value> = req
        .messages
        .iter()
        .map(|m| {
            // The Responses API accepts `role: "developer"` and
            // `role: "system"` interchangeably — map the chat-style
            // `system` role onto `system` (the spec default) and pass
            // user / assistant through verbatim.
            let role = match m.role.as_str() {
                "system" | "developer" => "system",
                "user" => "user",
                "assistant" => "assistant",
                // Anything else (rare tool-role messages) gets passed
                // through unchanged so the wire adapter doesn't silently
                // drop information the operator explicitly encoded.
                other => other,
            };
            serde_json::json!({
                "role": role,
                "content": m.content,
            })
        })
        .collect();
    let mut body = serde_json::json!({
        "model": req.model,
        "input": input,
        "max_output_tokens": req.max_tokens,
        "stream": req.stream,
    });
    if let Some(temp) = req.temperature {
        body["temperature"] = serde_json::json!(temp);
    }
    if let Some(eff) = &req.reasoning_effort {
        body["reasoning"] = serde_json::json!({ "effort": eff });
    }
    body
}

/// OpenAI Responses API non-streaming response shape (subset we consume).
/// Mirrors the [`AnthropicResponse`] / [`ChatCompletion`] structure: pull
/// the assistant text out of every `output[]` entry whose
/// `content[].type == "output_text"`, and translate the Responses-shaped
/// `usage.input_tokens` / `usage.output_tokens` into the OpenAI-shaped
/// `prompt_tokens` / `completion_tokens` fields the [`ChatResult`]
/// surface already aggregates. The same schema-invalid-2xx guard the
/// Anthropic and chat paths enforce applies: a response whose `output`
/// array is empty (or carries only non-`output_text` content blocks and
/// therefore leaves the answer empty) MUST surface as `ErrKind::Decode`.
#[derive(Deserialize)]
struct ResponsesResponse {
    #[serde(default)]
    output: Vec<ResponsesOutputItem>,
    #[serde(default)]
    usage: Option<ResponsesUsage>,
    #[serde(default)]
    #[allow(dead_code)]
    status: Option<String>,
}
#[derive(Deserialize)]
struct ResponsesOutputItem {
    #[serde(default)]
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    #[serde(rename = "role")]
    role: Option<String>,
    #[serde(default)]
    content: Vec<ResponsesContent>,
}
#[derive(Deserialize)]
struct ResponsesContent {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
}
#[derive(Deserialize, Default)]
struct ResponsesUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    /// Some Responses-shaped bodies also report `total_tokens`; we
    /// ignore it (the wire adapter only ever exposes prompt + completion
    /// upstream) but parse it for future-compat.
    #[serde(default)]
    #[allow(dead_code)]
    total_tokens: Option<u64>,
}

impl ResponsesResponse {
    fn joined_text(&self) -> String {
        let mut out = String::new();
        for item in &self.output {
            // Only `message`-typed items carry the conversational
            // assistant text (`reasoning`-typed items hold reasoning
            // summaries and would require a separate `show_reasoning`
            // contract — see the `responses_body` doc comment for why
            // that is a deliberate out-of-scope follow-up). The
            // `role: "assistant"` filter is belt-and-braces: every
            // well-formed Responses body tags the assistant message
            // item with role `assistant`, and ignoring anything else
            // keeps the parser robust against a future Responses-shaped
            // body that adds an extra non-message output entry.
            if item.kind.as_deref() != Some("message") {
                continue;
            }
            if item.role.as_deref().is_some_and(|r| r != "assistant") {
                continue;
            }
            for block in &item.content {
                if block.kind.as_deref() == Some("output_text") {
                    if let Some(text) = &block.text {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(text);
                    }
                }
            }
        }
        out
    }
    fn has_meaningful_text(&self) -> bool {
        // Whitespace-only text counts as nothing: same contract the
        // Anthropic / chat branches enforce so a billable reservation
        // cannot silently reconcile as a no-op.
        self.joined_text().chars().any(|c| !c.is_whitespace())
    }
}

pub struct OpenAiProvider {
    base_url: String,
    /// Provider wire kind: `openai-chat` | `openai-responses` | `azure-openai`
    /// | `anthropic` | `custom`. Drives endpoint routing (Azure uses a
    /// deployment path + `?api-version`, Anthropic uses the `/v1/messages`
    /// Messages API with `x-api-key` + `anthropic-version` headers, OpenAI
    /// `chat/completions` and Responses variants both use the OpenAI
    /// `/v1/...` convention with `Authorization: Bearer …`).
    ///
    /// `openai-responses` targets the OpenAI Responses API
    /// (`POST {base}/v1/responses`): it replaces the chat `messages` array
    /// with `input`, the `max_tokens` field with `max_output_tokens`, and
    /// the chat-completion `reasoning_effort` top-level string with a
    /// top-level `reasoning: {effort: "..."}` object. System-role messages
    /// are emitted inline as a `role: "system"` item in `input` (the same
    /// way the Responses API spec supports and the goose reference
    /// adapter also does) rather than promoted to a top-level
    /// `instructions` string; this keeps a single message-processing path
    /// and avoids carrying a leading non-message field that operators then
    /// have to reason about when inspecting what the wire adapter sent.
    ///
    /// `custom` still falls through to the `openai-chat` request shape
    /// (documented follow-up; the wire adapter in this slice does not
    /// silently invent behavior for kinds it does not implement).
    kind: String,
    /// Pre-resolved auth header `(name, value)` — `Authorization: Bearer …`
    /// for bearer styles, or a custom `api-key`-style header for enterprise
    /// gateways. `None` when the provider needs no credential.
    auth_header: Option<(String, String)>,
    /// Original [`Auth`] (cloned from the config). Kept alongside
    /// `auth_header` so a `kind == "anthropic"` request can re-resolve the
    /// raw credential into Anthropic's `x-api-key: <token>` shape (the
    /// pre-resolved `auth_header` already wraps a bearer token in
    /// `Authorization: Bearer …`, which Anthropic's wire protocol does not
    /// accept). The `openai-responses` kind keeps the same bearer-shaped
    /// authorization as `openai-chat` (the Responses API uses
    /// `Authorization: Bearer …` verbatim), so it does NOT need to
    /// re-resolve the credential. Anthropic-auth users who configure
    /// `Auth::ApiKeyHeader { header: "x-api-key", … }` see the same header
    /// pair either way, but the bearer path needs the re-resolution.
    auth: Auth,
    /// Provider id from the config (e.g. `"minimax"`). Used by the
    /// counter-fed utilization wire-up to decide whether a response
    /// belongs to a MiniMax provider (which publishes no rate-limit
    /// headers, so its usage has to be counted locally).
    provider_id: String,
    /// Plan label used as the `plan` key in the utilization store. The
    /// catalog tier when the provider has a `subscription.tier`; the
    /// provider id otherwise. Always set so the store key is stable.
    plan_label: String,
    /// KNEMON per-account identity for KNEMON capture paths (header-fed
    /// snapshot recording + counter-fed token accounting). Resolved once
    /// at construction from the configured
    /// [`crate::config::SubscriptionPlan::effective_account_id`] so every
    /// `(provider, account_id, plan)` key the runtime writes is the one
    /// the routing reader looks up. Defaults to [`DEFAULT_ACCOUNT_ID`]
    /// when the provider has no `subscription` (free / metered providers
    /// still produce a stable key, but no record is ever written under
    /// it because capture paths bail before reaching the store).
    account_id: String,
    client: reqwest::Client,
    request_timeout: Duration,
    idle_timeout: Duration,
    /// Resolved Azure OpenAI Data Plane `api-version` for
    /// `kind == "azure-openai"` providers. Populated at construction time
    /// from the precedence documented on [`crate::config::Provider::azure_api_version`]:
    ///   1. `Provider::azure_api_version` (per-provider),
    ///   2. `AZURE_OPENAI_API_VERSION` env var (host-wide),
    ///   3. built-in default `"2024-10-21"`.
    ///
    /// For non-Azure providers this is `None` (the helper still resolves
    /// the same precedence at call time, but storing the resolved value
    /// here keeps `endpoint()` allocation-free for every Azure call once
    /// the provider is constructed).
    azure_api_version: Option<String>,
}

impl OpenAiProvider {
    pub fn new(p: &Provider) -> anyhow::Result<Self> {
        let plan_label = p
            .subscription
            .as_ref()
            .map(|s| s.tier.clone().unwrap_or_else(|| "explicit".to_string()))
            .unwrap_or_else(|| p.id.clone());
        // KNEMON per-account identity: thread the configured
        // `effective_account_id()` through every capture path so two
        // accounts on the same `(provider, tier)` never collide on the
        // literal `"default"` key. Providers with no `subscription`
        // (free / metered) still resolve to `DEFAULT_ACCOUNT_ID` — the
        // capture paths short-circuit before writing anything in that
        // case, so the choice is observable only on subscription wires.
        let account_id = p
            .subscription
            .as_ref()
            .map(|s| s.effective_account_id())
            .unwrap_or_else(|| DEFAULT_ACCOUNT_ID.to_string());
        // Azure Data Plane `api-version` resolution at construction time:
        // per-provider `Provider::azure_api_version` wins, then the
        // host-wide `AZURE_OPENAI_API_VERSION` env var, then the built-in
        // default. Non-Azure providers leave this as `None` — the env-var
        // fallback in `endpoint_url` only fires when `kind == "azure-openai"`
        // anyway, so storing the resolved value (or `None`) here keeps the
        // hot path branch-free for the openai-chat / anthropic / responses
        // cases. Resolved once so every chat call carries the same wire
        // string and a config-field change is picked up on the next
        // `OpenAiProvider::new(&cfg)` cycle (the long-lived provider cache
        // is keyed by `Provider.id`, so a config reload triggers a rebuild).
        let azure_api_version = if p.kind == "azure-openai" {
            Some(
                p.azure_api_version
                    .as_deref()
                    .map(|s| s.to_string())
                    .or_else(|| std::env::var("AZURE_OPENAI_API_VERSION").ok())
                    .unwrap_or_else(|| "2024-10-21".to_string()),
            )
        } else {
            None
        };
        Ok(Self {
            base_url: p.base_url.trim_end_matches('/').to_string(),
            kind: p.kind.clone(),
            auth_header: p.auth.header_pair(),
            // Cloned so the Anthropic path can re-resolve the raw
            // credential into `x-api-key: <token>` without going back
            // through the bearer wrapper that `auth_header_pair` applies.
            // The `header_pair` value above is still the OpenAI/Azure
            // default; the Anthropic branch ignores it entirely.
            auth: p.auth.clone(),
            provider_id: p.id.clone(),
            plan_label,
            account_id,
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .pool_idle_timeout(Duration::from_secs(90))
                .build()?,
            request_timeout: Duration::from_secs(env_secs(
                "ZODER_TIMEOUT_S",
                DEFAULT_REQUEST_TIMEOUT_S,
            )),
            idle_timeout: Duration::from_secs(env_secs("ZODER_IDLE_S", DEFAULT_IDLE_TIMEOUT_S)),
            azure_api_version,
        })
    }

    /// KNEMON per-account identity resolved at construction time from
    /// the configured [`crate::config::SubscriptionPlan::effective_account_id`].
    /// Exposed for tests + the few callers (CLI routes / reports) that
    /// want to key utilization-store lookups by the same id the live
    /// capture path uses, instead of hard-coding the literal `"default"`.
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    /// Plan label resolved at construction time from
    /// [`Provider::subscription`] (catalog tier when present, the
    /// configured plan label `"explicit"` when absent, or the provider
    /// id when the provider has no subscription at all). Stable per
    /// provider so the store key survives re-reads.
    pub fn plan_label(&self) -> &str {
        &self.plan_label
    }

    /// Build an endpoint URL for `suffix` (e.g. `"chat/completions"`,
    /// `"models"`), normalizing so we never emit `/v1/v1/...`. The configured
    /// `base_url` may or may not already include the `/v1` version segment;
    /// `azure-openai` uses its own deployment route (the base already carries
    /// the deployment path) with a `?api-version` (override via
    /// `AZURE_OPENAI_API_VERSION`).
    fn endpoint(&self, suffix: &str) -> String {
        endpoint_url(
            &self.base_url,
            &self.kind,
            suffix,
            self.azure_api_version.as_deref(),
        )
    }

    async fn read_limited_body(
        &self,
        resp: reqwest::Response,
        label: &str,
    ) -> Result<Vec<u8>, ProviderError> {
        if resp
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(ProviderError::new(
                ErrKind::Decode,
                format!("{label} exceeded {MAX_RESPONSE_BYTES} byte response ceiling"),
            ));
        }
        let deadline = Instant::now() + self.request_timeout;
        let mut stream = resp.bytes_stream();
        let mut body = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ProviderError::new(
                    ErrKind::Timeout,
                    format!("request timeout reading {label}"),
                ));
            }
            let next = timeout(remaining, stream.next()).await.map_err(|_| {
                ProviderError::new(ErrKind::Timeout, format!("request timeout reading {label}"))
            })?;
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(classify_reqwest)?;
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(ProviderError::new(
                    ErrKind::Decode,
                    format!("{label} exceeded {MAX_RESPONSE_BYTES} byte response ceiling"),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// Fetch the live set of served model ids from the provider's models route.
    ///
    /// Y-19: `kind == "azure-openai"` is intentionally short-circuited. The
    /// configured `base_url` is the deployment route
    /// (`…/openai/deployments/<dep>`), but the OpenAI-compatible `/models`
    /// endpoint lives at the account scope (`{account}/openai/models`) — so
    /// naively composing `endpoint_url(base, "azure-openai", "models", …)`
    /// would emit the malformed
    /// `…/openai/deployments/<dep>/models?api-version=…` URL and Azure would
    /// 404 it. Until we resolve the account-scope base explicitly, callers
    /// fall back to the operator-configured `model_ids` (probe + consult
    /// already handle an `Err` here as "use configured ids").
    pub async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        if self.is_azure() {
            return Err(ProviderError::new(
                ErrKind::Http,
                "list_models not supported for azure-openai (deployment-scoped \
                 base_url has no /models route; configure model_ids instead)",
            ));
        }
        let mut rb = self.client.get(self.endpoint("models"));
        if let Some((name, value)) = &self.auth_header {
            rb = rb.header(name.as_str(), value.as_str());
        }
        let resp = match timeout(self.request_timeout, rb.send()).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(classify_reqwest(e)),
            Err(_) => return Err(ProviderError::new(ErrKind::Timeout, "models list timeout")),
        };
        let status = resp.status();
        if !status.is_success() {
            return Err(ProviderError {
                message: format!("models HTTP {status}"),
                kind: if status.as_u16() == 429 {
                    ErrKind::RateLimit
                } else if status.is_server_error() {
                    ErrKind::Server
                } else {
                    ErrKind::Http
                },
                status: Some(status.as_u16()),
                retry_after: retry_after_header(resp.headers()),
                emitted: false,
                anthropic_error_body: None,
            });
        }
        let body = self.read_limited_body(resp, "models list").await?;
        let v: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
            ProviderError::new(ErrKind::Decode, format!("malformed models list: {e}"))
        })?;
        let ids = v
            .get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(ids)
    }

    fn body(&self, req: &ChatRequest) -> serde_json::Value {
        if self.is_anthropic() {
            // Anthropic Messages API has no `temperature` default at the
            // request level (it does — but the wire shape differs from
            // OpenAI's), and the system prompt extraction lives in
            // [`anthropic_body`]. Branching here keeps the OpenAI path
            // byte-for-byte unchanged for any non-anthropic provider.
            return anthropic_body(req);
        }
        if self.is_responses() {
            // OpenAI Responses API: `input` instead of `messages`,
            // `max_output_tokens` instead of `max_tokens`,
            // `reasoning: {effort: "..."}` instead of `reasoning_effort`,
            // and an inline `role: "system"` item for any system
            // message. The body helper lives in [`responses_body`].
            // Keeping it behind its own branch preserves the
            // `openai-chat` byte-for-byte contract for every other
            // kind (including `custom` and `azure-openai`).
            return responses_body(req);
        }
        // Azure OpenAI intentionally falls through to the openai-chat
        // body builder: Azure's chat-completions body (`model`,
        // `messages`, `max_tokens`, `temperature`, `stream`,
        // `stream_options.include_usage`, `reasoning_effort`) is
        // structurally identical to OpenAI's, so a parallel
        // `azure_body()` helper would just duplicate the openai-chat
        // builder. The Azure wire adapter therefore differs from the
        // openai-chat branch ONLY in endpoint + auth header (see
        // [`Self::request`]) — every body field is shared.
        let mut body = serde_json::json!({
            "model": req.model,
            "messages": req.messages,
            "max_tokens": req.max_tokens,
            "stream": req.stream,
        });
        if let Some(temp) = req.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        // Ask the backend to emit a final usage chunk so we can record real
        // prompt/completion token counts instead of guessing.
        if req.stream {
            body["stream_options"] = serde_json::json!({ "include_usage": true });
        }
        // Pass reasoning effort straight through (LiteLLM/free-tier translate it to the
        // served model). Left unset => the model's own default behavior.
        if let Some(eff) = &req.reasoning_effort {
            body["reasoning_effort"] = serde_json::json!(eff);
        }
        body
    }

    fn request(&self, req: &ChatRequest) -> reqwest::RequestBuilder {
        if self.is_anthropic() {
            // Anthropic Messages API:
            //   * endpoint: POST {base}/v1/messages
            //   * headers: `x-api-key: <token>` + `anthropic-version: 2023-06-01`
            //   * body: `anthropic_body(req)`
            // No `Authorization: Bearer …` header — Anthropic's wire protocol
            // rejects it with 401. The credential resolution strips a
            // leading `Bearer ` from a pre-resolved `auth_header` value
            // (covers the `Env { var }` / `Bearer { token }` paths); an
            // `ApiKeyHeader { header: "x-api-key", … }` config re-uses its
            // already-stripped value verbatim.
            let mut rb = self
                .client
                .post(self.endpoint(ANTHROPIC_MESSAGES_SUFFIX))
                .header("anthropic-version", ANTHROPIC_VERSION)
                .json(&self.body(req));
            if let Some(token) = self.anthropic_api_key() {
                rb = rb.header("x-api-key", token);
            }
            return rb;
        }
        if self.is_responses() {
            // OpenAI Responses API:
            //   * endpoint: POST {base}/v1/responses
            //   * headers: `Authorization: Bearer …` (same as
            //     chat-completions — the Responses API does not use
            //     `x-api-key` or any custom header), so the
            //     pre-resolved `auth_header` is applied verbatim.
            //   * body: `responses_body(req)`.
            // `reasoning_effort` mapping (`{"effort":"…"}` object)
            // lives inside `responses_body` so the helper stays a
            // pure `ChatRequest -> serde_json::Value` transformer.
            let mut rb = self
                .client
                .post(self.endpoint(RESPONSES_SUFFIX))
                .json(&self.body(req));
            if let Some((name, value)) = &self.auth_header {
                rb = rb.header(name.as_str(), value.as_str());
            }
            return rb;
        }
        if self.is_azure() {
            // Native Azure OpenAI wire adapter:
            //   * endpoint: POST {base}/chat/completions?api-version=…
            //     (the base already encodes the deployment route per the
            //     Azure OpenAI wire contract — `endpoint_url` adds the
            //     `?api-version` from the resolved `azure_api_version`).
            //   * headers: `api-key: <key>` (NOT `Authorization: Bearer …`).
            //     Azure authenticates with the literal `api-key` header on
            //     every Data Plane request; the bearer-shape credential the
            //     pre-resolved `auth_header` would emit is rejected with 401.
            //     `azure_api_key()` resolves the configured credential
            //     verbatim and strips a defensive `Bearer ` prefix if an
            //     operator copy-pasted a JWT into the env var (matches the
            //     Anthropic branch's safety net).
            //   * body: the openai-chat shape — Azure's chat-completions
            //     body is structurally identical to OpenAI's, so `body()`
            //     falls through to the chat-completions builder for
            //     `kind == "azure-openai"` (no parallel `azure_body()`
            //     helper to keep the wire surface in lockstep with the
            //     chat branch — mirrors how the openai-responses slice
            //     avoided duplicating code for the byte-for-byte-shared
            //     auth path).
            let mut rb = self
                .client
                .post(self.endpoint("chat/completions"))
                .json(&self.body(req));
            if let Some(key) = self.azure_api_key() {
                rb = rb.header("api-key", key);
            }
            return rb;
        }
        let mut rb = self
            .client
            .post(self.endpoint("chat/completions"))
            .json(&self.body(req));
        if let Some((name, value)) = &self.auth_header {
            rb = rb.header(name.as_str(), value.as_str());
        }
        rb
    }

    /// `true` when this provider is configured to use the Anthropic
    /// Messages API wire shape. Treats an empty/unset `kind` (the
    /// pre-`#[serde(default = "default_kind")]` legacy shape) as
    /// `openai-chat` so a config that never declared `kind` keeps the
    /// legacy behavior bit-for-bit.
    fn is_anthropic(&self) -> bool {
        self.kind == "anthropic"
    }

    /// `true` when this provider is configured to use the OpenAI
    /// Responses API wire shape (`POST {base}/v1/responses`). Sibling of
    /// [`Self::is_anthropic`]; checked after the Anthropic branch so a
    /// config that wants either shape gets the right wire format and
    /// the `openai-chat` byte-identical contract is preserved for the
    /// default / empty / unset / `azure-openai` / `custom` kinds.
    fn is_responses(&self) -> bool {
        self.kind == "openai-responses"
    }

    /// `true` when this provider is configured to use the native Azure
    /// OpenAI wire shape (`POST {base}/chat/completions?api-version=...`
    /// with an `api-key: <key>` header — NOT `Authorization: Bearer …`).
    /// Sibling of [`Self::is_anthropic`] and [`Self::is_responses`]; the
    /// body builder falls through to the openai-chat shape (Azure's
    /// chat-completions body is structurally identical to OpenAI's — see
    /// [`Self::body`] for the "azure reuses the chat body" contract), and
    /// only the endpoint + auth header differ. The deployment itself is
    /// encoded in the configured `base_url`
    /// (`…/openai/deployments/<deployment>`) per the Azure OpenAI wire
    /// contract, so the adapter does NOT maintain a separate deployment
    /// field — every SDK / curl example builds the URL the same way.
    fn is_azure(&self) -> bool {
        self.kind == "azure-openai"
    }

    /// Resolve the raw credential for an Anthropic provider call. Returns
    /// `None` when no credential is configured (Anthropic will respond 401
    /// with the same classification as an OpenAI 401 — the request still
    /// carries `x-api-key:` with an empty string in that case so the
    /// response shape stays uniform for tests).
    ///
    /// For `Env { var }` / `Bearer { token }` this strips the `Bearer `
    /// wrapper that [`Auth::header_pair`] applies; for `ApiKeyHeader { … }`
    /// the configured header name is irrelevant (Anthropic only honors
    /// `x-api-key`) so we use the raw resolved value directly.
    fn anthropic_api_key(&self) -> Option<String> {
        match &self.auth {
            Auth::None => None,
            Auth::Env { .. } | Auth::Bearer { .. } => self
                .auth
                .resolve()
                .map(|tok| strip_bearer_prefix(&tok).to_string()),
            Auth::ApiKeyHeader { .. } => self.auth.resolve(),
        }
    }

    /// Resolve the raw credential for an Azure OpenAI provider call.
    /// Azure authenticates with an `api-key: <key>` HEADER (NOT
    /// `Authorization: Bearer …`), and the configured credential is the
    /// api-key value verbatim — no `Bearer ` prefix to strip and no
    /// header-name remap. `ApiKeyHeader { header: "api-key", … }` (the
    /// recommended shape in `config.microsoft.toml`) sends the value
    /// as-is. `Env { var }` / `Bearer { token }` also feed the same raw
    /// value through — an operator who stored the Azure key in
    /// `AZURE_OPENAI_API_KEY` and configured `Env { var }` is supported
    /// without ceremony. Returns `None` when no credential is configured
    /// (Azure will respond 401 with the same classification an OpenAI 401
    /// would surface; the existing `classify_err -> from_status(401)`
    /// path picks it up).
    fn azure_api_key(&self) -> Option<String> {
        // `strip_bearer_prefix` is a no-op for the canonical
        // `api-key <32-hex-chars>` shape but is called defensively so a
        // future operator who copy-pastes a `Bearer eyJ...` JWT into the
        // env var still gets the bare token sent to Azure. Mirrors the
        // safety net the Anthropic branch applies for the same reason.
        self.auth
            .resolve()
            .map(|tok| strip_bearer_prefix(&tok).to_string())
    }

    /// Chat call. If `sink` is Some, decoded content is written to it live
    /// (e.g. stdout for `exec`). Streams when `req.stream`, otherwise reads the
    /// full body. Returns the full content + telemetry + usage, or a classified
    /// `ProviderError` the caller can use to drive retries + fallback.
    pub async fn stream_chat(
        &self,
        req: &ChatRequest,
        sink: Option<&mut dyn Write>,
    ) -> Result<ChatResult, ProviderError> {
        tracing::debug!(model = %req.model, stream = req.stream, max_tokens = req.max_tokens, "chat call");
        let resp = match timeout(self.request_timeout, self.request(req).send()).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(classify_reqwest(e)),
            Err(_) => {
                return Err(ProviderError::new(
                    ErrKind::Timeout,
                    format!("request timeout: no response in {:?}", self.request_timeout),
                ))
            }
        };
        let status = resp.status();
        let mut telemetry = telemetry_from_headers(resp.headers());
        // Direct providers (not fronted by a LiteLLM proxy) don't emit the
        // `x-litellm-model-api-base` served-backend header. Fall back to this
        // provider's configured base_url so the served host is still known —
        // the policy gate verifies it against the free-host set (which includes
        // operator-declared-free providers), instead of failing strict mode for
        // lack of any api_base telemetry.
        if telemetry.api_base.is_none() {
            telemetry.api_base = Some(self.base_url.clone());
        }
        // KNEMON live capture: parse rate-limit headers off this response
        // and persist a snapshot to ~/.zoder/utilization.json. The route
        // planner reads that store on the next turn to gate subscriptions
        // against headroom. BEST-EFFORT — any parse / IO error must not
        // poison the request. The snapshot's `account_id` is the configured
        // plan's `effective_account_id` (not the literal `"default"`) so
        // two accounts on the same `(provider, tier)` never collide.
        capture_rate_limit_snapshot(
            resp.headers(),
            &self.base_url,
            &self.kind,
            &self.plan_label,
            &self.account_id,
        );
        if !status.is_success() {
            let code = status.as_u16();
            let retry_after = retry_after_header(resp.headers());
            let kind = match code {
                429 => ErrKind::RateLimit,
                500..=599 => ErrKind::Server,
                _ => ErrKind::Http,
            };
            let body = self
                .read_limited_body(resp, "HTTP error body")
                .await
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_else(|error| format!("[{}]", error.message));
            return Err(ProviderError {
                message: format!("provider HTTP {status}: {}", redact(&body)),
                kind,
                status: Some(code),
                retry_after,
                emitted: false,
                anthropic_error_body: None,
            });
        }
        if req.stream {
            let r = self.consume_stream(req, resp, telemetry, sink).await?;
            // Counter-fed KNEMON capture (Layer 3B) runs after the body
            // is consumed so we have prompt+completion to count. The
            // header-fed capture above already ran on the response
            // headers; for MiniMax that path is a clean no-op, so this
            // is the only signal that provider contributes. Best-effort.
            // `account_id` is the configured `effective_account_id` —
            // NOT the literal `"default"` — so two MiniMax accounts on
            // the same tier keep separate counter buckets.
            capture_counter_usage(
                &self.provider_id,
                &self.base_url,
                &self.plan_label,
                &self.account_id,
                r.prompt_tokens,
                r.completion_tokens,
                chrono::Utc::now(),
            );
            Ok(r)
        } else {
            let r = self.consume_full(req, resp, telemetry, sink).await?;
            capture_counter_usage(
                &self.provider_id,
                &self.base_url,
                &self.plan_label,
                &self.account_id,
                r.prompt_tokens,
                r.completion_tokens,
                chrono::Utc::now(),
            );
            Ok(r)
        }
    }

    /// Non-streaming path: read the whole body and parse the completion object.
    async fn consume_full(
        &self,
        req: &ChatRequest,
        resp: reqwest::Response,
        telemetry: CallTelemetry,
        sink: Option<&mut dyn Write>,
    ) -> Result<ChatResult, ProviderError> {
        if self.is_anthropic() {
            return self.consume_full_anthropic(resp, telemetry, sink).await;
        }
        if self.is_responses() {
            return self.consume_full_responses(resp, telemetry, sink).await;
        }
        let body = self.read_limited_body(resp, "chat completion").await?;
        let parsed: ChatCompletion = serde_json::from_slice(&body).map_err(|e| {
            ProviderError::new(
                ErrKind::Decode,
                format!("malformed chat-completion response: {e}"),
            )
        })?;
        // Schema-invalid 2xx body guard: a response is not "successful" just
        // because the HTTP layer returned 2xx. We require at least one choice
        // whose `message` carries parseable content (or, under
        // `show_reasoning`, a non-empty reasoning field) — mirroring the
        // streaming path's `saw_choice` check. Without this `{"choices":[{}]}`
        // would parse, `pick_text` would silently return `""`, the call would
        // succeed with no content, and a `--no-stream` / reviewer / health-
        // probe run could exit 0 while a billable reservation is reconciled
        // as a paid no-op. That is the failure mode the task pins: schema-
        // invalid wire traffic must surface as a Decode error.
        let choice = parsed.choices.into_iter().next().ok_or_else(|| {
            ProviderError::new(
                ErrKind::Decode,
                "malformed chat-completion response: no completion choices",
            )
        })?;
        if !choice.has_meaningful_message() {
            return Err(ProviderError::new(
                ErrKind::Decode,
                "malformed chat-completion response: empty completion choice/message",
            ));
        }
        let msg = choice
            .message
            .expect("has_meaningful_message guarantees Some(_)");
        let content = pick_text(
            msg.content,
            msg.reasoning_content,
            msg.reasoning,
            req.show_reasoning,
        );
        if content.len() > MAX_CONTENT_BYTES {
            return Err(ProviderError::new(
                ErrKind::Decode,
                format!("decoded content exceeded {MAX_CONTENT_BYTES} byte ceiling"),
            ));
        }
        if let Some(s) = sink {
            let _ = s.write_all(content.as_bytes());
            let _ = s.flush();
        }
        let (prompt_tokens, completion_tokens, cached_prompt_tokens) = match parsed.usage {
            Some(u) => {
                let cached_prompt_tokens = u.cached_prompt_tokens();
                (u.prompt_tokens, u.completion_tokens, cached_prompt_tokens)
            }
            None => (None, None, None),
        };
        let tokens_out = completion_tokens.unwrap_or(0);
        Ok(ChatResult {
            content,
            tokens_out,
            prompt_tokens,
            completion_tokens,
            cached_prompt_tokens,
            telemetry,
        })
    }

    /// Anthropic non-streaming response: parse the Messages API JSON body
    /// (`{"content": [...], "usage": {...}}`) and translate the result into
    /// the same [`ChatResult`] shape the OpenAI path returns. Mirrors the
    /// schema-invalid-2xx guard: a body whose `content` array has no text
    /// block (e.g. only `type: "tool_use"`, or an empty array) MUST surface
    /// as `ErrKind::Decode` so a `--no-stream` / reviewer / health-probe
    /// call cannot exit Ok with an empty content and let a billable
    /// reservation reconcile as a no-op.
    async fn consume_full_anthropic(
        &self,
        resp: reqwest::Response,
        telemetry: CallTelemetry,
        sink: Option<&mut dyn Write>,
    ) -> Result<ChatResult, ProviderError> {
        let body = self.read_limited_body(resp, "anthropic message").await?;
        let parsed: AnthropicResponse = serde_json::from_slice(&body).map_err(|e| {
            ProviderError::new(
                ErrKind::Decode,
                format!("malformed anthropic response: {e}"),
            )
        })?;
        // Same schema-invalid-2xx contract the OpenAI path enforces.
        if !parsed.has_meaningful_text() {
            return Err(ProviderError::new(
                ErrKind::Decode,
                "malformed anthropic response: empty content",
            ));
        }
        let content = parsed.joined_text();
        if content.len() > MAX_CONTENT_BYTES {
            return Err(ProviderError::new(
                ErrKind::Decode,
                format!("decoded content exceeded {MAX_CONTENT_BYTES} byte ceiling"),
            ));
        }
        if let Some(s) = sink {
            let _ = s.write_all(content.as_bytes());
            let _ = s.flush();
        }
        let (prompt_tokens, completion_tokens) = match parsed.usage {
            Some(ref u) => (u.input_tokens, u.output_tokens),
            None => (None, None),
        };
        let cached_prompt_tokens = parsed.cached_prompt_tokens();
        let tokens_out = completion_tokens.unwrap_or(0);
        Ok(ChatResult {
            content,
            tokens_out,
            prompt_tokens,
            completion_tokens,
            cached_prompt_tokens,
            telemetry,
        })
    }

    /// OpenAI Responses API non-streaming response: parse the
    /// `{"output": [...], "usage": {...}}` body and translate into the
    /// same [`ChatResult`] shape the chat and Anthropic paths return.
    /// Mirrors the schema-invalid-2xx guard both other paths enforce:
    /// a body whose `output` array has no `output_text` content block
    /// (e.g. an empty array, only `reasoning`-typed items, or only
    /// whitespace text) MUST surface as `ErrKind::Decode` so a
    /// `--no-stream` / reviewer / health-probe run cannot exit Ok with
    /// an empty content and let a billable reservation reconcile as a
    /// no-op. The Responses API surface has no analogue of
    /// `cached_prompt_tokens` (no `prompt_tokens_details.cached_tokens`
    /// block); `cached_prompt_tokens` is left `None` for the Responses
    /// path, matching the byte-level contract an OpenAI Codex
    /// `openai-responses` proxy would publish.
    async fn consume_full_responses(
        &self,
        resp: reqwest::Response,
        telemetry: CallTelemetry,
        sink: Option<&mut dyn Write>,
    ) -> Result<ChatResult, ProviderError> {
        let body = self.read_limited_body(resp, "responses message").await?;
        let parsed: ResponsesResponse = serde_json::from_slice(&body).map_err(|e| {
            ProviderError::new(
                ErrKind::Decode,
                format!("malformed responses response: {e}"),
            )
        })?;
        // Same schema-invalid-2xx contract as the other two branches.
        if !parsed.has_meaningful_text() {
            return Err(ProviderError::new(
                ErrKind::Decode,
                "malformed responses response: empty output text",
            ));
        }
        let content = parsed.joined_text();
        if content.len() > MAX_CONTENT_BYTES {
            return Err(ProviderError::new(
                ErrKind::Decode,
                format!("decoded content exceeded {MAX_CONTENT_BYTES} byte ceiling"),
            ));
        }
        if let Some(s) = sink {
            let _ = s.write_all(content.as_bytes());
            let _ = s.flush();
        }
        let (prompt_tokens, completion_tokens) = match parsed.usage {
            Some(ref u) => (u.input_tokens, u.output_tokens),
            None => (None, None),
        };
        let tokens_out = completion_tokens.unwrap_or(0);
        Ok(ChatResult {
            content,
            tokens_out,
            prompt_tokens,
            completion_tokens,
            // The Responses API doesn't expose an OpenAI-shaped
            // `cached_tokens` block on a non-streaming response; the
            // cached-prompt telemetry cap stays at `None` for this
            // branch (see the doc comment for rationale).
            cached_prompt_tokens: None,
            telemetry,
        })
    }

    /// Streaming path: parse newline-delimited SSE frames as they arrive.
    /// Dispatched on `kind`: Anthropic events use a different envelope
    /// (`event: …` lines and `data: {...}` with `type: "content_block_delta"`
    /// / `message_delta` etc.) than OpenAI's `data: {"choices":[…]}` chunks,
    /// so the two paths run entirely separate parsers rather than sharing a
    /// single loop. The OpenAI loop below is byte-for-byte unchanged from
    /// the pre-Anthropic-adapter version.
    async fn consume_stream(
        &self,
        req: &ChatRequest,
        resp: reqwest::Response,
        telemetry: CallTelemetry,
        mut sink: Option<&mut dyn Write>,
    ) -> Result<ChatResult, ProviderError> {
        if self.is_anthropic() {
            return self.consume_stream_anthropic(resp, telemetry, sink).await;
        }
        if self.is_responses() {
            return self.consume_stream_responses(resp, telemetry, sink).await;
        }
        let deadline = Instant::now() + self.request_timeout;
        let mut content = String::new();
        let mut chunk_count: u64 = 0;
        let mut prompt_tokens: Option<u64> = None;
        let mut completion_tokens: Option<u64> = None;
        let mut cached_prompt_tokens: Option<u64> = None;
        // True once answer bytes have been written to the sink: a retry of this
        // same model would duplicate visible output.
        let mut emitted = false;
        let mut stream = resp.bytes_stream();
        let mut response_bytes = 0usize;
        // Accumulate raw bytes; split on the 0x0A byte so multibyte UTF-8 chars
        // that straddle network chunks are never corrupted.
        let mut buf: Vec<u8> = Vec::new();
        let fail = |kind: ErrKind, msg: String, emitted: bool| ProviderError {
            message: msg,
            kind,
            status: None,
            retry_after: None,
            emitted,
            anthropic_error_body: None,
        };
        let mut done = false;
        let mut saw_choice = false;
        // Z-9: a stream that yields one or more `choices` frames but never
        // pushes a non-empty content delta must NOT be reconciled as a
        // successful completion — the billable reservation would otherwise
        // be paid out as a no-op. Mirrors the non-streaming
        // `has_meaningful_message()` guard at ~1205. Set when any non-empty
        // `pick_text` fragment is appended to `content`; a `choices: [{}]`
        // or `{"choices":[{"delta":{"content":""}}]}` stream leaves this
        // false and surfaces as `ErrKind::Decode` at the bottom of the
        // function, with `emitted` unchanged so the caller knows nothing
        // was written to the sink.
        let mut saw_content = false;
        loop {
            // Stall guard: bound each read by the smaller of idle budget and
            // remaining overall budget so a silently-hanging model fails fast
            // (-> recorded as a health failure -> breaker -> reroute).
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(fail(
                    ErrKind::Timeout,
                    format!("request timeout after {:?}", self.request_timeout),
                    emitted,
                ));
            }
            let step = self.idle_timeout.min(remaining);
            let next = match timeout(step, stream.next()).await {
                Ok(n) => n,
                Err(_) => {
                    if Instant::now() >= deadline {
                        return Err(fail(
                            ErrKind::Timeout,
                            format!("request timeout after {:?}", self.request_timeout),
                            emitted,
                        ));
                    }
                    return Err(fail(
                        ErrKind::Timeout,
                        format!("stream stalled: no data for {:?}", self.idle_timeout),
                        emitted,
                    ));
                }
            };
            let Some(chunk) = next else { break };
            let bytes = chunk.map_err(|e| {
                let mut pe = classify_reqwest(e);
                pe.emitted = emitted;
                pe
            })?;
            response_bytes = response_bytes.saturating_add(bytes.len());
            if response_bytes > MAX_RESPONSE_BYTES {
                return Err(fail(
                    ErrKind::Decode,
                    format!("stream exceeded {MAX_RESPONSE_BYTES} byte response ceiling"),
                    emitted,
                ));
            }
            buf.extend_from_slice(&bytes);
            if buf.len() > MAX_BUFFER_BYTES {
                return Err(fail(
                    ErrKind::Decode,
                    format!("stream buffer exceeded {MAX_BUFFER_BYTES} byte ceiling"),
                    emitted,
                ));
            }
            // SSE frames are newline-delimited "data: {...}" lines.
            while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                if nl > MAX_LINE_BYTES {
                    return Err(fail(
                        ErrKind::Decode,
                        format!("stream line exceeded {MAX_LINE_BYTES} byte ceiling"),
                        emitted,
                    ));
                }
                let line_bytes: Vec<u8> = buf.drain(..=nl).collect();
                let raw = std::str::from_utf8(&line_bytes[..nl]).map_err(|error| {
                    fail(
                        ErrKind::Decode,
                        format!("stream contained invalid UTF-8: {error}"),
                        emitted,
                    )
                })?;
                let line = raw.trim();
                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload == "[DONE]" {
                    done = true;
                    break;
                }
                let val: serde_json::Value = serde_json::from_str(payload).map_err(|error| {
                    fail(
                        ErrKind::Decode,
                        format!("malformed provider stream JSON: {error}"),
                        emitted,
                    )
                })?;
                // A streamed `{"error": ...}` frame is a real provider failure —
                // surfacing it (rather than skipping) prevents an error from
                // masquerading as an empty successful response.
                if let Some(err) = val.get("error") {
                    return Err(fail(
                        ErrKind::Http,
                        format!("provider stream error: {}", redact(&err.to_string())),
                        emitted,
                    ));
                }
                let parsed = serde_json::from_value::<StreamChunk>(val).map_err(|error| {
                    fail(
                        ErrKind::Decode,
                        format!("malformed provider stream frame: {error}"),
                        emitted,
                    )
                })?;
                // Every successful frame must advance one of the two pieces of
                // stream state we understand: completion choices, or concrete
                // usage telemetry. Requiring `choices` and each choice's
                // `delta` at deserialization rejects `{}` and
                // `{"choices":[{}]}`; this check also rejects inert frames such
                // as `{"choices":[],"usage":{}}` while preserving the standard
                // final usage-only frame (`choices: []` plus token counts).
                if parsed.choices.is_empty()
                    && !parsed.usage.as_ref().is_some_and(Usage::is_meaningful)
                {
                    return Err(fail(
                        ErrKind::Decode,
                        "malformed provider stream frame: expected a completion choice or meaningful usage telemetry"
                            .to_string(),
                        emitted,
                    ));
                }
                if let Some(u) = parsed.usage {
                    if u.prompt_tokens.is_some() {
                        prompt_tokens = u.prompt_tokens;
                    }
                    if u.completion_tokens.is_some() {
                        completion_tokens = u.completion_tokens;
                    }
                    if let Some(cached) = u.cached_prompt_tokens() {
                        cached_prompt_tokens = Some(cached);
                    }
                }
                if let Some(choice) = parsed.choices.into_iter().next() {
                    saw_choice = true;
                    let piece = pick_text(
                        choice.delta.content,
                        choice.delta.reasoning_content,
                        choice.delta.reasoning,
                        req.show_reasoning,
                    );
                    if !piece.is_empty() {
                        if content.len().saturating_add(piece.len()) > MAX_CONTENT_BYTES {
                            return Err(fail(
                                ErrKind::Decode,
                                format!(
                                    "decoded content exceeded {MAX_CONTENT_BYTES} byte ceiling"
                                ),
                                emitted,
                            ));
                        }
                        // Mirror the non-streaming `has_meaningful_message`
                        // contract: whitespace-only deltas do not count as
                        // a real answer, so a stream that pushes only
                        // whitespace (or a single non-content frame) is
                        // still schema-invalid and must surface as Decode
                        // at the bottom of the function. This stops a
                        // billable reservation from being reconciled as a
                        // paid no-op when the model "answered" with just
                        // a newline.
                        if piece.chars().any(|c| !c.is_whitespace()) {
                            saw_content = true;
                        }
                        chunk_count += 1;
                        content.push_str(&piece);
                        if let Some(s) = sink.as_deref_mut() {
                            let _ = s.write_all(piece.as_bytes());
                            let _ = s.flush();
                            emitted = true;
                        }
                    }
                }
            }
            // `[DONE]` (or a terminal error) ends the whole stream — break the
            // outer read loop so a keep-alive connection doesn't stall to EOF.
            if done {
                break;
            }
        }
        if !buf.iter().all(|byte| byte.is_ascii_whitespace()) {
            return Err(fail(
                ErrKind::Decode,
                "stream ended with an incomplete SSE frame".to_string(),
                emitted,
            ));
        }
        if !saw_choice {
            return Err(fail(
                ErrKind::Decode,
                "malformed completion stream: no valid completion choice".to_string(),
                emitted,
            ));
        }
        // Z-9 guard: a stream that carries a choice but no non-empty
        // content delta is schema-invalid in the same sense the
        // non-streaming `has_meaningful_message()` guard catches. We
        // must NOT credit the call as a 0-token success — a billable
        // reservation would be paid out as a no-op, and a
        // `--no-stream` / reviewer / health-probe run on the same
        // model would have surfaced as a Decode error on the
        // non-streaming path. The `emitted` flag stays at its
        // accumulated value so the caller (and the retry/fallback
        // classifier) can tell whether any bytes reached the sink.
        if !saw_content {
            return Err(fail(
                ErrKind::Decode,
                "malformed completion stream: empty completion choice (no content delta)"
                    .to_string(),
                emitted,
            ));
        }
        // Prefer authoritative usage; fall back to the streamed-chunk count.
        let tokens_out = completion_tokens.unwrap_or(chunk_count);
        Ok(ChatResult {
            content,
            tokens_out,
            prompt_tokens,
            completion_tokens,
            cached_prompt_tokens,
            telemetry,
        })
    }

    /// Anthropic SSE streaming parser. Mirrors the shape of the OpenAI
    /// streaming loop but consumes Anthropic's distinct event envelope:
    ///
    /// ```text
    /// event: message_start
    /// data: {"type":"message_start","message":{"id":"…","usage":{"input_tokens":N,"output_tokens":1}}}
    ///
    /// event: content_block_start
    /// data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}
    ///
    /// event: content_block_delta
    /// data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}
    ///
    /// event: content_block_stop
    /// data: {"type":"content_block_stop","index":0}
    ///
    /// event: message_delta
    /// data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":K}}
    ///
    /// event: message_stop
    /// data: {"type":"message_stop"}
    /// ```
    ///
    /// The parser accumulates `delta.text` fragments into the same
    /// `content` string the OpenAI path produces, captures the
    /// `input_tokens` / `output_tokens` usage off the appropriate events
    /// (`message_start` carries `input_tokens`; `message_delta` carries
    /// the final `output_tokens` so we don't need to guess), and treats
    /// `message_stop` as the terminal event. A mid-stream `event: error`
    /// (the only Anthropic-side way to surface an auth / rate-limit /
    /// overload rejection on a 2xx connection) is mapped onto the same
    /// `ErrKind::Http` shape the OpenAI streaming-error path uses, so
    /// downstream classification does not silently swallow it.
    async fn consume_stream_anthropic(
        &self,
        resp: reqwest::Response,
        telemetry: CallTelemetry,
        mut sink: Option<&mut dyn Write>,
    ) -> Result<ChatResult, ProviderError> {
        let deadline = Instant::now() + self.request_timeout;
        let mut content = String::new();
        let mut chunk_count: u64 = 0;
        let mut prompt_tokens: Option<u64> = None;
        let mut completion_tokens: Option<u64> = None;
        let mut cached_prompt_tokens: Option<u64> = None;
        let mut emitted = false;
        let mut stream = resp.bytes_stream();
        let mut response_bytes = 0usize;
        let mut buf: Vec<u8> = Vec::new();
        // Y-14: the Anthropic SSE parser attaches the typed error frame's
        // raw payload (`anthropic_error_body`) so `classify_err` can route an
        // `overloaded_error` / `rate_limit_error` to `Capacity` and an
        // `authentication_error` to `Unauthorized` -- identical to how a
        // 529 / 401 in the HTTP headers would classify. The default `None`
        // for non-Anthropic / non-error code paths keeps every other
        // caller's classification pipeline unchanged. Using a `Cell` lets
        // the existing 3-arg `fail` closure shape pick up the typed body
        // without having to retrofit the signature at every call site.
        let anthropic_error_body: std::cell::Cell<Option<String>> = std::cell::Cell::new(None);
        let fail = |kind: ErrKind, msg: String, emitted: bool| ProviderError {
            anthropic_error_body: anthropic_error_body.take(),
            message: msg,
            kind,
            status: None,
            retry_after: None,
            emitted,
        };
        let mut done = false;
        let mut saw_text = false;
        // Track the most recent `event:` line so the next `data:` line is
        // paired with the right envelope. SSE resets per-event, so we keep
        // the LAST seen event name until a new one arrives. A missing
        // `event:` line defaults to `"message"` (the spec's default event
        // type), which Anthropic never uses in practice for the messages
        // API — every real frame carries an explicit `event:` line.
        let mut current_event: Option<String> = None;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(fail(
                    ErrKind::Timeout,
                    format!("request timeout after {:?}", self.request_timeout),
                    emitted,
                ));
            }
            let step = self.idle_timeout.min(remaining);
            let next = match timeout(step, stream.next()).await {
                Ok(n) => n,
                Err(_) => {
                    if Instant::now() >= deadline {
                        return Err(fail(
                            ErrKind::Timeout,
                            format!("request timeout after {:?}", self.request_timeout),
                            emitted,
                        ));
                    }
                    return Err(fail(
                        ErrKind::Timeout,
                        format!("stream stalled: no data for {:?}", self.idle_timeout),
                        emitted,
                    ));
                }
            };
            let Some(chunk) = next else { break };
            let bytes = chunk.map_err(|e| {
                let mut pe = classify_reqwest(e);
                pe.emitted = emitted;
                pe
            })?;
            response_bytes = response_bytes.saturating_add(bytes.len());
            if response_bytes > MAX_RESPONSE_BYTES {
                return Err(fail(
                    ErrKind::Decode,
                    format!("stream exceeded {MAX_RESPONSE_BYTES} byte response ceiling"),
                    emitted,
                ));
            }
            buf.extend_from_slice(&bytes);
            if buf.len() > MAX_BUFFER_BYTES {
                return Err(fail(
                    ErrKind::Decode,
                    format!("stream buffer exceeded {MAX_BUFFER_BYTES} byte ceiling"),
                    emitted,
                ));
            }
            // Anthropic SSE frames are CRLF- or LF-delimited, terminated by
            // a blank line. Process one frame at a time: a frame is a
            // sequence of `field: value` lines (we only care about
            // `event:` and `data:`) followed by an empty line.
            while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                if nl > MAX_LINE_BYTES {
                    return Err(fail(
                        ErrKind::Decode,
                        format!("stream line exceeded {MAX_LINE_BYTES} byte ceiling"),
                        emitted,
                    ));
                }
                let line_bytes: Vec<u8> = buf.drain(..=nl).collect();
                let raw = std::str::from_utf8(&line_bytes[..nl]).map_err(|error| {
                    fail(
                        ErrKind::Decode,
                        format!("stream contained invalid UTF-8: {error}"),
                        emitted,
                    )
                })?;
                let line = raw.trim_end_matches(['\r', '\n']);
                if line.is_empty() {
                    // Blank line = end of frame. Nothing to dispatch —
                    // the per-line `event`/`data` updates above already
                    // happened. Reset `current_event` to its default
                    // (None) for the next frame, per the SSE spec
                    // (each event stands alone; the event type from a
                    // previous frame MUST NOT leak into a subsequent
                    // frame that omits its own `event:` line). The next
                    // `event:` line will re-populate `current_event`
                    // before the next `data:` line is dispatched.
                    current_event = None;
                    continue;
                }
                if let Some(ev) = line.strip_prefix("event:") {
                    current_event = Some(ev.trim().to_string());
                    continue;
                }
                if let Some(payload) = line.strip_prefix("data:") {
                    let payload = payload.trim();
                    if payload.is_empty() {
                        continue;
                    }
                    if payload == "[DONE]" {
                        // Anthropic never sends `[DONE]`; ignore (don't
                        // mark `done`) so we keep waiting for the
                        // terminal `message_stop` event.
                        continue;
                    }
                    let val: serde_json::Value =
                        serde_json::from_str(payload).map_err(|error| {
                            fail(
                                ErrKind::Decode,
                                format!("malformed anthropic stream JSON: {error}"),
                                emitted,
                            )
                        })?;
                    // Top-level `error` envelope: Anthropic emits
                    // `event: error` with a body shaped like
                    // `{"type":"error","error":{"type":"authentication_error",...}}`.
                    //
                    // Y-14: the typed envelope is the ONLY signal we get for
                    // an Anthropic 401/429/529 mid-stream rejection (the HTTP
                    // 200 that carried the body has no status to anchor on).
                    // Carry the raw payload through `anthropic_error_body`
                    // so `classify_err` routes an `overloaded_error` /
                    // `rate_limit_error` to `Capacity` (consult skips, no
                    // breaker trip) and an `authentication_error` /
                    // `permission_error` to `Unauthorized` (key rejected,
                    // no breaker trip), instead of the historical
                    // hard-coded `Error` that benches a healthy model
                    // behind a bad key or temporary overload.
                    if current_event.as_deref() == Some("error")
                        || val.get("type").and_then(|t| t.as_str()) == Some("error")
                    {
                        anthropic_error_body.set(Some(payload.to_string()));
                        return Err(fail(
                            ErrKind::Http,
                            format!("anthropic stream error: {}", redact(payload)),
                            emitted,
                        ));
                    }
                    match current_event.as_deref() {
                        Some("message_start") => {
                            // Carries `message.usage.input_tokens` (and
                            // `output_tokens` = 1 placeholder; we ignore
                            // the placeholder and wait for `message_delta`
                            // to set the authoritative output_tokens).
                            if let Some(input) = val
                                .get("message")
                                .and_then(|m| m.get("usage"))
                                .and_then(|u| u.get("input_tokens"))
                                .and_then(|n| n.as_u64())
                            {
                                prompt_tokens = Some(input);
                            }
                            if let Some(usage) = val
                                .get("message")
                                .and_then(|m| m.get("usage"))
                                .and_then(|u| u.get("cache_read_input_tokens"))
                                .and_then(|n| n.as_u64())
                            {
                                cached_prompt_tokens = Some(usage);
                            }
                        }
                        Some("content_block_delta") => {
                            // The deltas carry `delta.type` ∈
                            // {"text_delta","input_json_delta","thinking_delta"}.
                            // We only surface `text_delta` for parity
                            // with the OpenAI path's `content` field.
                            let piece = val
                                .get("delta")
                                .and_then(|d| d.get("text"))
                                .and_then(|t| t.as_str())
                                .unwrap_or_default();
                            if !piece.is_empty() {
                                saw_text = true;
                                if content.len().saturating_add(piece.len()) > MAX_CONTENT_BYTES {
                                    return Err(fail(
                                        ErrKind::Decode,
                                        format!(
                                            "decoded content exceeded {MAX_CONTENT_BYTES} byte ceiling"
                                        ),
                                        emitted,
                                    ));
                                }
                                chunk_count += 1;
                                content.push_str(piece);
                                if let Some(s) = sink.as_deref_mut() {
                                    let _ = s.write_all(piece.as_bytes());
                                    let _ = s.flush();
                                    emitted = true;
                                }
                            }
                        }
                        Some("message_delta") => {
                            // Carries the authoritative final
                            // `usage.output_tokens` (and optionally a
                            // stop_reason / stop_sequence, which we
                            // ignore for parity with the OpenAI path).
                            if let Some(out) = val
                                .get("usage")
                                .and_then(|u| u.get("output_tokens"))
                                .and_then(|n| n.as_u64())
                            {
                                completion_tokens = Some(out);
                            }
                            // Cache-read updates can also arrive here on
                            // long streams; sum with the value from
                            // `message_start` so a multi-block run keeps
                            // the running tally.
                            if let Some(extra) = val
                                .get("usage")
                                .and_then(|u| u.get("cache_read_input_tokens"))
                                .and_then(|n| n.as_u64())
                            {
                                cached_prompt_tokens =
                                    Some(cached_prompt_tokens.unwrap_or(0).saturating_add(extra));
                            }
                        }
                        Some("message_stop") => {
                            done = true;
                        }
                        // content_block_start / content_block_stop /
                        // ping / unknown: informational only, ignore.
                        _ => {}
                    }
                }
            }
            if done {
                break;
            }
        }
        if !buf.iter().all(|byte| byte.is_ascii_whitespace()) {
            return Err(fail(
                ErrKind::Decode,
                "stream ended with an incomplete SSE frame".to_string(),
                emitted,
            ));
        }
        // Schema-invalid-2xx guard for the streaming path: a successful
        // Anthropic stream MUST carry at least one text delta; a body
        // that connects, handshakes, and then closes with `message_stop`
        // having produced no `content_block_delta` with a text payload
        // is just as empty as the `{"choices":[{}]}` shape the OpenAI
        // path rejects.
        if !saw_text {
            return Err(fail(
                ErrKind::Decode,
                "malformed anthropic stream: no text content".to_string(),
                emitted,
            ));
        }
        let tokens_out = completion_tokens.unwrap_or(chunk_count);
        Ok(ChatResult {
            content,
            tokens_out,
            prompt_tokens,
            completion_tokens,
            cached_prompt_tokens,
            telemetry,
        })
    }

    /// OpenAI Responses API SSE streaming parser. Mirrors the shape of
    /// [`Self::consume_stream_anthropic`] but consumes the Responses
    /// API's distinct event envelope:
    ///
    /// ```text
    /// event: response.created
    /// data: {"type":"response.created","response":{"id":"…","model":"…"}}
    ///
    /// event: response.output_text.delta
    /// data: {"type":"response.output_text.delta","delta":"Hel"}
    ///
    /// event: response.output_text.delta
    /// data: {"type":"response.output_text.delta","delta":"lo"}
    ///
    /// event: response.completed
    /// data: {"type":"response.completed","response":{"usage":{"input_tokens":N,"output_tokens":K,"total_tokens":N+K}}}
    /// ```
    ///
    /// Text pieces arrive on `response.output_text.delta` (carrying a
    /// single `delta` string per event). Usage arrives on the terminal
    /// `response.completed` event under `response.usage.{input_tokens,
    /// output_tokens, total_tokens}`. Errors arrive as either an
    /// `event: error` or a typed `{"type":"response.failed", …}`
    /// envelope; both surface as `ErrKind::Http` (mirrors the way the
    /// Anthropic branch surfaces an inline stream error and the way
    /// the OpenAI-branch stream parser surfaces a `{"error":…}` data
    /// frame).
    ///
    /// `response.created` / `response.in_progress` /
    /// `response.output_item.added` / `response.content_part.added` /
    /// `response.output_text.done` / `response.content_part.done` /
    /// `response.output_item.done` / ping are recognized but ignored
    /// (the wire adapter does not surface any state from them, they
    /// exist to give a desktop-style consumer hooks for cursor +
    /// completion rendering). The Responses API uses typed
    /// `sequence_number` ordering rather than data-fragment ordering,
    /// which is fine for the byte-level text assembly this parser
    /// does.
    async fn consume_stream_responses(
        &self,
        resp: reqwest::Response,
        telemetry: CallTelemetry,
        mut sink: Option<&mut dyn Write>,
    ) -> Result<ChatResult, ProviderError> {
        // The endpoint URL is read in `request()`; we accept the same
        // stall / idle / buffer / line ceilings the chat and
        // Anthropic branches accept so a hostile / misconfigured
        // backend cannot pin us in a read loop. (The dispatch site in
        // `consume_stream` forwards `&ChatRequest` for symmetry with
        // the chat branch; the Responses parser does not need it
        // because text deltas arrive as plain strings, not under
        // `pick_text`'s reasoning-content fan-out.)
        let deadline = Instant::now() + self.request_timeout;
        let mut content = String::new();
        let mut chunk_count: u64 = 0;
        let mut prompt_tokens: Option<u64> = None;
        let mut completion_tokens: Option<u64> = None;
        // Responses API emits no cache-token surface on its SSE
        // terminal event (the chat branch's `cached_prompt_tokens`
        // comes from `prompt_tokens_details.cached_tokens`, which
        // the Responses stream does not publish). Leave it
        // unconditionally `None` so the wire adapter doesn't carry
        // a phantom value an operator inspecting the run log
        // would otherwise assume came from a typed envelope.
        let mut emitted = false;
        let mut stream = resp.bytes_stream();
        let mut response_bytes = 0usize;
        let mut buf: Vec<u8> = Vec::new();
        let fail = |kind: ErrKind, msg: String, emitted: bool| ProviderError {
            message: msg,
            kind,
            status: None,
            retry_after: None,
            emitted,
            anthropic_error_body: None,
        };
        let mut done = false;
        let mut saw_text = false;
        let mut current_event: Option<String> = None;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(fail(
                    ErrKind::Timeout,
                    format!("request timeout after {:?}", self.request_timeout),
                    emitted,
                ));
            }
            let step = self.idle_timeout.min(remaining);
            let next = match timeout(step, stream.next()).await {
                Ok(n) => n,
                Err(_) => {
                    if Instant::now() >= deadline {
                        return Err(fail(
                            ErrKind::Timeout,
                            format!("request timeout after {:?}", self.request_timeout),
                            emitted,
                        ));
                    }
                    return Err(fail(
                        ErrKind::Timeout,
                        format!("stream stalled: no data for {:?}", self.idle_timeout),
                        emitted,
                    ));
                }
            };
            let Some(chunk) = next else { break };
            let bytes = chunk.map_err(|e| {
                let mut pe = classify_reqwest(e);
                pe.emitted = emitted;
                pe
            })?;
            response_bytes = response_bytes.saturating_add(bytes.len());
            if response_bytes > MAX_RESPONSE_BYTES {
                return Err(fail(
                    ErrKind::Decode,
                    format!("stream exceeded {MAX_RESPONSE_BYTES} byte response ceiling"),
                    emitted,
                ));
            }
            buf.extend_from_slice(&bytes);
            if buf.len() > MAX_BUFFER_BYTES {
                return Err(fail(
                    ErrKind::Decode,
                    format!("stream buffer exceeded {MAX_BUFFER_BYTES} byte ceiling"),
                    emitted,
                ));
            }
            // Responses API SSE frames are CRLF- or LF-delimited
            // `field: value` lines separated by a blank line. The
            // frame loop tracks `event:` lines alongside `data:` ones
            // (same contract the Anthropic branch uses) so the next
            // `data:` line is paired with the correct envelope.
            while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                if nl > MAX_LINE_BYTES {
                    return Err(fail(
                        ErrKind::Decode,
                        format!("stream line exceeded {MAX_LINE_BYTES} byte ceiling"),
                        emitted,
                    ));
                }
                let line_bytes: Vec<u8> = buf.drain(..=nl).collect();
                let raw = std::str::from_utf8(&line_bytes[..nl]).map_err(|error| {
                    fail(
                        ErrKind::Decode,
                        format!("stream contained invalid UTF-8: {error}"),
                        emitted,
                    )
                })?;
                let line = raw.trim_end_matches(['\r', '\n']);
                if line.is_empty() {
                    // Blank line = end of frame. Reset `current_event`
                    // to its default (None) so the next frame's `data:`
                    // line is paired with the correct envelope — the
                    // SSE spec says each event stands alone, and the
                    // Responses API parser uses `current_event` (with a
                    // `val.type` fallback) to choose which dispatch arm
                    // to take, so a stale value from the previous frame
                    // would silently mis-classify a subsequent frame
                    // that omits `event:`.
                    current_event = None;
                    continue;
                }
                if let Some(ev) = line.strip_prefix("event:") {
                    current_event = Some(ev.trim().to_string());
                    continue;
                }
                if let Some(payload) = line.strip_prefix("data:") {
                    let payload = payload.trim();
                    if payload.is_empty() {
                        continue;
                    }
                    // The Responses API doesn't terminate with a
                    // `[DONE]` sentinel the way the chat-completions
                    // stream does; the equivalent is the
                    // `response.completed` typed event handled below.
                    // Ignore any `[DONE]` sentinel defensively in case
                    // a gateway proxy interposes one.
                    if payload == "[DONE]" {
                        continue;
                    }
                    let val: serde_json::Value =
                        serde_json::from_str(payload).map_err(|error| {
                            fail(
                                ErrKind::Decode,
                                format!("malformed responses stream JSON: {error}"),
                                emitted,
                            )
                        })?;
                    let event_type = current_event
                        .as_deref()
                        .or_else(|| val.get("type").and_then(|t| t.as_str()));
                    // Top-level / `event: error` envelope. Mirrors
                    // the same heuristic the Anthropic branch uses
                    // to surface an inline stream error so an
                    // HTTP-200 + error-frame cannot masquerade as a
                    // successful empty response.
                    if event_type == Some("error")
                        || val.get("type").and_then(|t| t.as_str()) == Some("error")
                    {
                        return Err(fail(
                            ErrKind::Http,
                            format!("responses stream error: {}", redact(payload)),
                            emitted,
                        ));
                    }
                    // `response.failed` carries the same operational
                    // outcome as an inline `error` event (the
                    // request was rejected by the backend mid-flight
                    // after a 200 had been written) — surface as
                    // `ErrKind::Http`, identical envelope shape.
                    if event_type == Some("response.failed") {
                        return Err(fail(
                            ErrKind::Http,
                            format!("responses stream failed: {}", redact(payload)),
                            emitted,
                        ));
                    }
                    match event_type {
                        // Text deltas: the Responses API carries the
                        // raw fragment in a top-level `delta` string
                        // (NOT inside a `delta: {text: "..."}`
                        // wrapper). Accumulate onto `content` and
                        // write through to the sink byte-for-byte
                        // so `sink` and `content` agree.
                        Some("response.output_text.delta") => {
                            let piece = val
                                .get("delta")
                                .and_then(|d| d.as_str())
                                .unwrap_or_default();
                            if !piece.is_empty() {
                                saw_text = true;
                                if content.len().saturating_add(piece.len()) > MAX_CONTENT_BYTES {
                                    return Err(fail(
                                        ErrKind::Decode,
                                        format!(
                                            "decoded content exceeded {MAX_CONTENT_BYTES} byte ceiling"
                                        ),
                                        emitted,
                                    ));
                                }
                                chunk_count += 1;
                                content.push_str(piece);
                                if let Some(s) = sink.as_deref_mut() {
                                    let _ = s.write_all(piece.as_bytes());
                                    let _ = s.flush();
                                    emitted = true;
                                }
                            }
                        }
                        // Terminal event: `response.completed` carries
                        // the authoritative usage under
                        // `response.usage.{input_tokens,
                        // output_tokens, total_tokens}`. The
                        // `total_tokens` field is accepted for
                        // future-compat but we only ever surface
                        // prompt + completion upstream to match the
                        // chat + Anthropic branch contract.
                        Some("response.completed") => {
                            if let Some(usage) = val.get("response").and_then(|r| r.get("usage")) {
                                if let Some(p) = usage.get("input_tokens").and_then(|n| n.as_u64())
                                {
                                    prompt_tokens = Some(p);
                                }
                                if let Some(c) = usage.get("output_tokens").and_then(|n| n.as_u64())
                                {
                                    completion_tokens = Some(c);
                                }
                            }
                            done = true;
                        }
                        // response.created /
                        // response.in_progress /
                        // response.output_item.added /
                        // response.content_part.added /
                        // response.output_text.done /
                        // response.content_part.done /
                        // response.output_item.done / unknown:
                        // informational only, ignore (the OpenAI
                        // Responses SSE spec calls these lifecycle
                        // events and they don't carry text the
                        // byte-level assembler would otherwise
                        // need).
                        _ => {}
                    }
                }
            }
            if done {
                break;
            }
        }
        if !buf.iter().all(|byte| byte.is_ascii_whitespace()) {
            return Err(fail(
                ErrKind::Decode,
                "stream ended with an incomplete SSE frame".to_string(),
                emitted,
            ));
        }
        if !saw_text {
            return Err(fail(
                ErrKind::Decode,
                "malformed responses stream: no output text".to_string(),
                emitted,
            ));
        }
        let tokens_out = completion_tokens.unwrap_or(chunk_count);
        Ok(ChatResult {
            content,
            tokens_out,
            prompt_tokens,
            completion_tokens,
            cached_prompt_tokens: None,
            telemetry,
        })
    }
}

/// Heuristic: turn a `reqwest::header::HeaderMap` + configured `base_url`
/// into a [`crate::utilization::Provider`] so we know which key the live
/// snapshot belongs under. The header set is the source of truth — a
/// Codex response always carries `x-codex-*`, an Anthropic response
/// always carries `anthropic-ratelimit-unified-*` / `anthropic-ratelimit-*`.
/// `base_url` is only consulted when no known headroom header is present,
/// in which case we fall back to a host-name lookup so a MiniMax response
/// (no headroom headers at all) classifies correctly as
/// `MiniMax` rather than collapsing into `Other` — the counter-fed wire-up
/// keys off the typed variant.
fn detect_utilization_provider(
    headers: &reqwest::header::HeaderMap,
    base_url: &str,
) -> Option<crate::utilization::Provider> {
    // Fast path: headroom headers beat host heuristics. Use the public
    // detector so we agree with the parser on what counts.
    if let Some(p) = crate::utilization::Provider::detect(&ReqwestHeaderProbe(headers)) {
        return Some(p);
    }
    let lowered = base_url.to_ascii_lowercase();
    if lowered.contains("minimax") {
        // MiniMax does not emit headroom headers; classify as the typed
        // `MiniMax` variant so the counter-fed `record_counter` path
        // can key utilization under its own `(provider, account, plan)`
        // triple and the header-fed path is a clean no-op.
        return Some(crate::utilization::Provider::MiniMax);
    }
    None
}

/// Adapter for the one-shot capture: same case-insensitive lookup the
/// parsers already expect.
struct ReqwestHeaderProbe<'a>(&'a reqwest::header::HeaderMap);

impl<'a> crate::utilization::HeaderLookup for ReqwestHeaderProbe<'a> {
    fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).and_then(|v| v.to_str().ok())
    }
}

/// Best-effort capture of live subscription telemetry off a chat-call
/// response. Maps the response's headers to a [`crate::utilization::Provider`],
/// parses any headroom window, and persists it via
/// [`crate::utilization::UtilizationStore::record`]. NEVER returns an
/// error — parse or IO failures are logged at debug and swallowed, so a
/// stale disk or a provider that started publishing a brand-new header
/// shape can never fail the in-flight request.
///
/// `account_id` is the configured
/// [`crate::config::SubscriptionPlan::effective_account_id`] of the
/// provider that served this response — KNEMON's per-account identity.
/// The HTTP wire doesn't publish the account, so we read it from the
/// provider config. A legacy provider that omits `account_id` resolves
/// to [`DEFAULT_ACCOUNT_ID`], preserving byte-for-byte the pre-fix
/// behavior on a single-account config. Two providers on the same
/// `(vendor, tier)` with different `account_id`s now keep separate
/// `(provider, account_id, plan)` rows. The configured plan label is
/// passed by `OpenAiProvider` and normalized after parsing so capture
/// and scenario/report lookup use the same tuple even when Codex
/// publishes a different display label.
fn capture_rate_limit_snapshot(
    headers: &reqwest::header::HeaderMap,
    base_url: &str,
    _kind: &str,
    plan: &str,
    account_id: &str,
) {
    let Some(provider) = detect_utilization_provider(headers, base_url) else {
        return;
    };
    let Some(mut snap) =
        crate::utilization::RateLimitSnapshot::from_headers(headers, provider, account_id, plan)
    else {
        return;
    };
    // Codex may publish its own plan label. The utilization-store key must
    // nevertheless use the configured subscription tier, which is what the
    // scenario/report readers resolve. Keep the provider value in the window
    // data, but normalize the storage key to the configured plan.
    snap.plan = plan.to_string();
    let Some(path) = crate::utilization::default_store_path() else {
        return;
    };
    // Open the store (cheap — empty file when missing), upsert, save.
    // We tolerate open failures (corrupt JSON, permission denied) so a
    // stale store on disk can't break the live chat path.
    let Ok(mut store) = crate::utilization::UtilizationStore::open(&path) else {
        tracing::debug!(?path, "utilization store open failed; skipping capture");
        return;
    };
    if !store.record(&snap, chrono::Utc::now()) {
        tracing::debug!(provider = ?snap.provider, "utilization snapshot had no windows");
    }
}

/// KNEMON Layer 3B — counter-fed capture for providers (MiniMax) that
/// publish no rate-limit headers. Increment the running token total for
/// the provider's monthly counter window and recompute `used_percent`
/// from the persisted cap. The cap is looked up lazily from the bundled
/// tier catalog on first sight and cached on the store; subsequent calls
/// just increment.
///
/// BEST-EFFORT — never returns an error, never poisons the request. The
/// header-fed [`capture_rate_limit_snapshot`] above runs first and is
/// the path for codex/anthropic; this function is the counterpart that
/// catches the no-header providers so they don't silently leak.
///
/// `account_id` is the configured
/// [`crate::config::SubscriptionPlan::effective_account_id`] of the
/// provider that served the response (see
/// [`capture_rate_limit_snapshot`] for the rationale). Counter rows
/// are keyed on `(provider, account_id, plan, window_name)`, so two
/// MiniMax accounts on the same tier now keep separate buckets instead
/// of colliding on the legacy `"default"` key.
///
/// Only windows with `observability = Counter` are incremented;
/// `PercentOnly` windows are intentionally untouched (the spec is
/// explicit: "Only windows with observability=Counter accumulate token
/// counts; PercentOnly windows are never locally computed").
fn capture_counter_usage(
    provider_id: &str,
    base_url: &str,
    plan: &str,
    account_id: &str,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    now: chrono::DateTime<chrono::Utc>,
) {
    // Detection: "minimax" appears in the provider id OR the base_url.
    // Either signal alone is enough; we OR them so an operator can
    // configure the provider with any id and route via a base_url that
    // names minimax, or vice versa.
    let id_lc = provider_id.to_ascii_lowercase();
    let url_lc = base_url.to_ascii_lowercase();
    if !id_lc.contains("minimax") && !url_lc.contains("minimax") {
        return;
    }
    // Total token count for this response. `usage.total_tokens` would
    // win if it were available, but the wire-shape we currently parse
    // only exposes prompt+completion — the spec explicitly handles this:
    // "carries token usage (usage.total_tokens or prompt+completion)".
    let total_tokens: f64 = match (prompt_tokens, completion_tokens) {
        (Some(p), Some(c)) => (p as f64) + (c as f64),
        (Some(p), None) => p as f64,
        (None, Some(c)) => c as f64,
        (None, None) => return, // No usage to count — the response just
                                // didn't carry it (e.g. streaming without `stream_options.include_usage`).
    };
    if total_tokens <= 0.0 {
        return;
    }
    let Some(path) = crate::utilization::default_store_path() else {
        return;
    };
    let Ok(mut store) = crate::utilization::UtilizationStore::open(&path) else {
        tracing::debug!(
            ?path,
            "utilization store open failed; skipping counter capture"
        );
        return;
    };
    // Cap resolution: look up the catalog once, find the Counter window
    // for the configured plan, and seed the store with the cap. The set is
    // idempotent (subsequent calls overwrite the same value, then
    // `record_counter` recomputes percent from the latest used_tokens).
    //
    // Calendar reset wiring (Finding #4): for every Counter window
    // declared by the catalog, also bind a `period_id` derived from
    // `now` so a later `record_counter` call that crosses a calendar
    // boundary atomically zeros `used_tokens` before applying the
    // increment. We compute the period for the SAME `now` the increment
    // uses (no clock skew between seed and increment), and we honor
    // the catalog's `ResetKind` — only calendar windows are period-
    // bound; rolling windows stay un-bound and never auto-reset.
    let catalog = crate::subscription_tiers::TierCatalog::bundled();
    if let Some(entry) = catalog.tier("minimax", plan) {
        for w in &entry.windows {
            if matches!(w.observability, crate::config::Observability::Counter) {
                store.set_counter_rolling_hours(
                    crate::utilization::Provider::MiniMax,
                    account_id,
                    plan,
                    &w.name,
                    (w.reset == crate::config::ResetKind::Rolling).then_some(w.hours),
                    now,
                );
                store.set_counter_cap(
                    crate::utilization::Provider::MiniMax,
                    account_id,
                    plan,
                    &w.name,
                    w.cap,
                    now,
                );
                let period_id = match w.reset {
                    crate::config::ResetKind::CalendarMonthly => {
                        crate::utilization::period_id_for(now)
                    }
                    crate::config::ResetKind::CalendarDaily => {
                        Some(now.format("%Y-%m-%d").to_string())
                    }
                    crate::config::ResetKind::Rolling => None,
                };
                store.set_counter_period_id(
                    crate::utilization::Provider::MiniMax,
                    account_id,
                    plan,
                    &w.name,
                    period_id,
                    now,
                );
            }
        }
    } else {
        tracing::debug!(
            plan,
            "minimax plan not in catalog; counter capture will run cap-less"
        );
    }
    // Increment the monthly window (the only Counter window the catalog
    // declares for `minimax-max` per the Layer 3B spec). If a future
    // catalog adds more Counter windows (e.g. a weekly counter), this
    // call would need to iterate the same `windows` list the cap-loop
    // just walked; today a single monthly window is enough.
    store.record_counter(
        crate::utilization::Provider::MiniMax,
        account_id,
        plan,
        "monthly",
        total_tokens,
        now,
    );
    // Best-effort persist. Mirror the header path: tolerate IO failure so
    // a transient disk hiccup never breaks a chat call.
    let _ = store.save();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        Auth, BillingMode, Observability, QuotaUnit, QuotaWindow, ResetKind, SubscriptionPlan,
    };

    #[test]
    fn endpoint_url_never_doubles_v1() {
        assert_eq!(
            endpoint_url(
                "https://api.example.com/v1",
                "openai-chat",
                "chat/completions",
                None
            ),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            endpoint_url("https://api.example.com/v1/", "openai-chat", "models", None),
            "https://api.example.com/v1/models"
        );
        assert_eq!(
            endpoint_url(
                "https://gw.example.com",
                "openai-chat",
                "chat/completions",
                None
            ),
            "https://gw.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn endpoint_url_azure_uses_deployment_route_no_v1() {
        let _guard = AZURE_API_VERSION_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("AZURE_OPENAI_API_VERSION", "2024-10-21");
        let u = endpoint_url(
            "https://res.openai.azure.com/openai/deployments/gpt4o",
            "azure-openai",
            "chat/completions",
            None,
        );
        assert_eq!(
            u,
            "https://res.openai.azure.com/openai/deployments/gpt4o/chat/completions?api-version=2024-10-21"
        );
        assert!(!u.contains("/v1/"), "azure route must not inject /v1");
        std::env::remove_var("AZURE_OPENAI_API_VERSION");
    }

    /// `Provider::azure_api_version` field (when `Some`) wins over both
    /// the env var and the built-in default — that's the per-provider
    /// override path an operator uses when they host multiple Azure
    /// deployments with different pinned Data Plane versions.
    #[test]
    fn endpoint_url_azure_explicit_api_version_overrides_env_var() {
        let _guard = AZURE_API_VERSION_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("AZURE_OPENAI_API_VERSION", "2024-08-01");
        let u = endpoint_url(
            "https://res.openai.azure.com/openai/deployments/gpt4o",
            "azure-openai",
            "chat/completions",
            Some("2024-10-21"),
        );
        assert_eq!(
            u,
            "https://res.openai.azure.com/openai/deployments/gpt4o/chat/completions?api-version=2024-10-21",
            "explicit api_version arg must beat the env var override"
        );
        std::env::remove_var("AZURE_OPENAI_API_VERSION");
    }

    /// With neither an explicit arg nor an env var, the helper falls
    /// through to the built-in default `"2024-10-21"`. The default is
    /// pinned here so a future bump is a single, intentional change
    /// rather than a silent drift across every Azure host.
    #[test]
    fn endpoint_url_azure_falls_back_to_builtin_default() {
        let _guard = AZURE_API_VERSION_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("AZURE_OPENAI_API_VERSION");
        let u = endpoint_url(
            "https://res.openai.azure.com/openai/deployments/gpt4o",
            "azure-openai",
            "chat/completions",
            None,
        );
        assert!(
            u.ends_with("?api-version=2024-10-21"),
            "absent arg + absent env var must yield the built-in default; got {u}"
        );
    }

    /// The api_version argument is azure-only — non-Azure kinds ignore
    /// it entirely (no query string, no behavior change for the
    /// `/v1/...` chat / responses routes).
    #[test]
    fn endpoint_url_ignores_api_version_for_non_azure_kinds() {
        // Even if a future caller passes Some, the non-Azure branches
        // must NOT add `?api-version=…` (only Azure accepts that param
        // — OpenAI / Anthropic would 400).
        let u = endpoint_url(
            "https://api.openai.com/v1",
            "openai-chat",
            "chat/completions",
            Some("2024-10-21"),
        );
        assert_eq!(u, "https://api.openai.com/v1/chat/completions");
        assert!(
            !u.contains("api-version"),
            "non-azure must not leak api-version: {u}"
        );
    }

    // -----------------------------------------------------------------
    // Azure OpenAI native wire adapter — unit tests.
    //
    // These tests pin the wire-shaping helpers
    // (`is_azure` / `azure_api_key` / the resolved `azure_api_version`)
    // in isolation. The end-to-end integration coverage (mocked HTTP
    // path, response decoder, error envelope classification) lives in
    // `tests/provider.rs::azure_*` and reuses the existing OpenAI
    // chat-completions response parser (Azure returns
    // OpenAI-standard `chat.completion` / `chat.completion.chunk`
    // shapes — no Azure-specific response code, by design).
    //
    // The pinned properties mirror the Anthropic / Responses unit-test
    // blocks above:
    //   * `kind == "azure-openai"` resolves `azure_api_version` in the
    //     documented precedence (config field -> env var -> default).
    //   * `is_azure()` discriminates correctly against every other kind.
    //   * `azure_api_key()` resolves the credential verbatim for the
    //     three supported `Auth` variants (None / Env / ApiKeyHeader),
    //     matching the docs in `config.microsoft.toml`.
    //   * The OpenAI-chat body builder is reused for Azure — the
    //     `kind == "azure-openai"` branch falls through to the chat
    //     body without any parallel `azure_body()` helper.
    // -----------------------------------------------------------------

    fn azure_provider_fixture(
        kind: &str,
        auth: Auth,
        azure_api_version: Option<&str>,
    ) -> OpenAiProvider {
        let cfg = Provider {
            id: "azure-test".into(),
            base_url: "https://res.openai.azure.com/openai/deployments/gpt4o".into(),
            kind: kind.into(),
            auth,
            paid: false,
            billing: BillingMode::Metered,
            subscription: None,
            serves: Vec::new(),
            azure_api_version: azure_api_version.map(|s| s.to_string()),
        };
        OpenAiProvider::new(&cfg).expect("fixture provider must build")
    }

    /// Mutex that serializes the small number of unit tests in this
    /// module that flip the shared `AZURE_OPENAI_API_VERSION` env
    /// var. Cargo runs tests in parallel by default, and the env-var
    /// read inside `OpenAiProvider::new` is non-atomic against
    /// concurrent `std::env::set_var` calls — without this guard,
    /// the precedence test races with `endpoint_url_azure_*` and
    /// `azure_endpoint_url_*` (each of which mutates the same
    /// variable) and reports spurious failures. Holding the mutex
    /// for the entire `set_var → fixture → assert → remove_var`
    /// window is the lightest acceptable isolation without pulling
    /// in a test-only `serial_test` dependency.
    static AZURE_API_VERSION_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn is_azure_is_true_only_for_azure_openai_kind() {
        let azure = azure_provider_fixture("azure-openai", Auth::None, None);
        assert!(
            azure.is_azure(),
            "azure-openai must enable the Azure branch"
        );

        let chat = azure_provider_fixture("openai-chat", Auth::None, None);
        assert!(
            !chat.is_azure(),
            "openai-chat must NOT take the Azure branch"
        );

        let responses = azure_provider_fixture("openai-responses", Auth::None, None);
        assert!(
            !responses.is_azure(),
            "openai-responses must NOT take the Azure branch (Responses uses Bearer)"
        );

        let anthropic = azure_provider_fixture("anthropic", Auth::None, None);
        assert!(
            !anthropic.is_azure(),
            "anthropic must NOT take the Azure branch"
        );

        // `custom` falls through to the openai-chat byte-for-byte
        // contract — the Azure branch is gated on the exact literal
        // string so a custom gateway operator isn't accidentally
        // routed to the Azure wire shape.
        let custom = azure_provider_fixture("custom", Auth::None, None);
        assert!(!custom.is_azure(), "custom must NOT take the Azure branch");

        // Empty / unset kind defaults to "openai-chat" via
        // `#[serde(default = "default_kind")]` — same byte-identical
        // guarantee, must NOT take the Azure branch.
        let empty = azure_provider_fixture("", Auth::None, None);
        assert!(
            !empty.is_azure(),
            "empty kind must NOT take the Azure branch"
        );
    }

    #[test]
    fn azure_api_version_resolution_precedence() {
        // Pin the resolution precedence the runtime applies at
        // `OpenAiProvider::new` time:
        //
        //   1. `Provider::azure_api_version` (per-provider field)
        //   2. `AZURE_OPENAI_API_VERSION` env var (host-wide)
        //   3. built-in default `"2024-10-21"`
        //
        // The helper-level env-var fallback is also covered by
        // `endpoint_url_azure_*` (which read the var at helper-call
        // time, not at construction); here we focus on the
        // `OpenAiProvider::new` constructor. Holding the
        // `AZURE_API_VERSION_TEST_LOCK` mutex for the duration of
        // each sub-case serializes this test against the other
        // unit tests in the binary that also flip the same env
        // var — without the lock, cargo's parallel test runner
        // would race us.
        let _guard = AZURE_API_VERSION_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        // (a) Provider field wins — the most specific override. Set
        // the env var to a SENTINEL distinct from the field value
        // so a parallel test flipping the var would be observably
        // distinct.
        std::env::set_var("AZURE_OPENAI_API_VERSION", "precedence-a-env");
        let p = azure_provider_fixture("azure-openai", Auth::None, Some("precedence-a-field"));
        assert_eq!(
            p.azure_api_version.as_deref(),
            Some("precedence-a-field"),
            "explicit azure_api_version field must beat the env var"
        );
        std::env::remove_var("AZURE_OPENAI_API_VERSION");

        // (b) Field absent, env var set — env var wins. The
        // helper-level fallback (the actual API-version resolution
        // at wire time) is exercised by `endpoint_url_azure_*`
        // above; here we just confirm the constructor reads the
        // env var under the documented precedence.
        std::env::set_var("AZURE_OPENAI_API_VERSION", "precedence-b-env");
        let p = azure_provider_fixture("azure-openai", Auth::None, None);
        assert_eq!(
            p.azure_api_version.as_deref(),
            Some("precedence-b-env"),
            "absent field must fall through to the env var"
        );
        std::env::remove_var("AZURE_OPENAI_API_VERSION");

        // (c) Both absent — built-in default wins.
        std::env::remove_var("AZURE_OPENAI_API_VERSION");
        let p = azure_provider_fixture("azure-openai", Auth::None, None);
        assert_eq!(
            p.azure_api_version.as_deref(),
            Some("2024-10-21"),
            "absent field + absent env var must yield the built-in default"
        );

        // (d) Non-Azure providers always have `azure_api_version = None`
        // — the field is otherwise dead state, and storing the
        // resolved default on every provider would muddy diffs in
        // serialization snapshots. The env var is set so any
        // parallel test that resolves an Azure provider doesn't
        // accidentally leak into this assertion; the `kind`
        // branch in `OpenAiProvider::new` short-circuits before
        // reading it. (We're holding the test lock so the env
        // var here is the one we set.)
        std::env::set_var("AZURE_OPENAI_API_VERSION", "precedence-d-env");
        let p = azure_provider_fixture("openai-chat", Auth::None, None);
        assert!(
            p.azure_api_version.is_none(),
            "non-azure providers must leave azure_api_version unset"
        );
        let p = azure_provider_fixture("anthropic", Auth::None, None);
        assert!(
            p.azure_api_version.is_none(),
            "anthropic providers must leave azure_api_version unset"
        );
        std::env::remove_var("AZURE_OPENAI_API_VERSION");
    }

    #[test]
    fn azure_api_key_resolves_every_auth_variant() {
        // `None` auth: no credential, no api-key header (Azure will
        // 401 and the existing classification pipeline picks it up —
        // same surface every other kind produces for missing creds).
        let p = azure_provider_fixture("azure-openai", Auth::None, None);
        assert!(
            p.azure_api_key().is_none(),
            "Auth::None must yield no api-key header"
        );

        // `Env { var }`: the canonical Azure config (`auth = { type =
        // "env", var = "AZURE_OPENAI_API_KEY" }` works the same as
        // the explicit `api_key_header` shape, because Azure's
        // credential is the api-key value verbatim — no Bearer
        // prefix.
        let var = "ZODER_TEST_AZURE_API_KEY_ENV";
        std::env::set_var(var, "azure-key-from-env");
        let p = azure_provider_fixture("azure-openai", Auth::Env { var: var.into() }, None);
        assert_eq!(
            p.azure_api_key().as_deref(),
            Some("azure-key-from-env"),
            "Auth::Env must feed the raw credential to the api-key header"
        );
        std::env::remove_var(var);

        // `ApiKeyHeader { header, var }` with `header = "api-key"` —
        // the explicit shape `config.microsoft.toml` recommends. The
        // header name itself is irrelevant on the wire adapter side
        // (Azure requires `api-key` regardless), so the test only
        // verifies the value flows through.
        let var = "ZODER_TEST_AZURE_API_KEY_HEADER";
        std::env::set_var(var, "azure-key-from-header");
        let p = azure_provider_fixture(
            "azure-openai",
            Auth::ApiKeyHeader {
                header: "api-key".into(),
                var: var.into(),
            },
            None,
        );
        assert_eq!(
            p.azure_api_key().as_deref(),
            Some("azure-key-from-header"),
            "Auth::ApiKeyHeader must feed the raw credential to the api-key header"
        );
        std::env::remove_var(var);

        // `Bearer { token }`: not the documented Azure shape, but
        // safe-by-construction — the helper strips a defensive
        // `Bearer ` prefix (matches the Anthropic branch's safety
        // net for an operator who copy-pastes a JWT into the env
        // var) and forwards the bare token.
        let p = azure_provider_fixture(
            "azure-openai",
            Auth::Bearer {
                token: "raw-azure-key".into(),
            },
            None,
        );
        assert_eq!(
            p.azure_api_key().as_deref(),
            Some("raw-azure-key"),
            "Auth::Bearer must strip the leading `Bearer ` for Azure"
        );
        let p = azure_provider_fixture(
            "azure-openai",
            Auth::Bearer {
                token: "Bearer raw-azure-key".into(),
            },
            None,
        );
        assert_eq!(
            p.azure_api_key().as_deref(),
            Some("raw-azure-key"),
            "an explicit `Bearer ` prefix must be stripped before the api-key header"
        );
    }

    #[test]
    fn azure_body_falls_through_to_openai_chat_shape() {
        // Azure's chat-completions body is structurally identical to
        // OpenAI's — there is NO `azure_body()` helper, and the body
        // builder falls through to the chat-completions branch for
        // `kind == "azure-openai"`. Pin the byte-for-byte contract so
        // a future refactor that accidentally introduces a parallel
        // body builder fails this test.
        let p = azure_provider_fixture("azure-openai", Auth::None, None);
        let req = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![
                Message::new("system", "be terse"),
                Message::new("user", "hi"),
            ],
            max_tokens: 32,
            temperature: Some(0.5),
            stream: true,
            show_reasoning: false,
            reasoning_effort: Some("medium".into()),
        };
        let body = p.body(&req);
        // The chat-completions shape MUST carry `messages`,
        // `max_tokens`, `temperature`, `stream`, `stream_options`,
        // `reasoning_effort` — and MUST NOT carry the Responses-only
        // `input` / `max_output_tokens` / `reasoning` fields.
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["max_tokens"], 32);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["stream"], true);
        assert_eq!(
            body["stream_options"]["include_usage"], true,
            "streaming Azure calls must request include_usage (chat-shape)"
        );
        assert_eq!(body["reasoning_effort"], "medium");
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2, "all messages pass through verbatim");
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "be terse");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "hi");
        // No Responses-shaped fields leak.
        assert!(
            body.get("input").is_none(),
            "Azure body must not carry Responses `input`: {body}"
        );
        assert!(
            body.get("max_output_tokens").is_none(),
            "Azure body must not carry Responses `max_output_tokens`: {body}"
        );
        assert!(
            body.get("reasoning").is_none(),
            "Azure body must not carry Responses `reasoning` object: {body}"
        );
    }

    /// Azure's resolved endpoint URL must include BOTH the deployment
    /// route (the `base_url` is expected to encode it — Azure's URL
    /// contract treats the deployment as a path segment) AND the
    /// `?api-version=` query string. The `?api-version` is what makes
    /// the request Azure-shaped and byte-distinguishes it from the
    /// openai-chat branch (which carries neither).
    #[test]
    fn azure_endpoint_url_is_deployment_plus_api_version() {
        let _guard = AZURE_API_VERSION_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Reset the env var for determinism.
        std::env::remove_var("AZURE_OPENAI_API_VERSION");
        let p = azure_provider_fixture("azure-openai", Auth::None, Some("2024-10-21"));
        let url = p.endpoint("chat/completions");
        assert_eq!(
            url,
            "https://res.openai.azure.com/openai/deployments/gpt4o/chat/completions?api-version=2024-10-21",
            "endpoint must keep the deployment in the path and append api-version"
        );
        assert!(
            !url.contains("/v1/"),
            "azure endpoint must not inject /v1: {url}"
        );

        // And the openai-chat path on the SAME base_url must yield a
        // different URL — that's the regression guard against the
        // Azure branch silently falling through to the chat branch.
        // (The Azure-shaped base `…/openai/deployments/gpt4o` doesn't
        // end in `/v1` and doesn't contain `/v1/`, so the openai-chat
        // branch's normalizer DOES inject `/v1/` — a key part of the
        // byte-distinguishing wire shape between the two branches on
        // the same base. The Azure branch keeps the URL
        // deployment-scoped AND adds `?api-version=`. Together that's
        // two independent byte-level distinctions from the chat path.)
        let chat = azure_provider_fixture("openai-chat", Auth::None, None);
        let chat_url = chat.endpoint("chat/completions");
        assert_ne!(
            chat_url, url,
            "openai-chat and azure-openai must produce distinct URLs on the same base"
        );
        assert_eq!(
            chat_url, "https://res.openai.azure.com/openai/deployments/gpt4o/v1/chat/completions",
            "openai-chat on the same Azure-shaped base_url must inject /v1 (no api-version)"
        );
        assert!(
            !chat_url.contains("api-version"),
            "openai-chat branch must NOT carry api-version (Azure-only): {chat_url}"
        );
    }

    #[test]
    fn redact_scrubs_secrets_keeps_prose() {
        let r = redact("error: invalid key sk-abcdef0123456789abcdef please retry");
        assert!(!r.contains("sk-abcdef"), "sk- key must be redacted: {r}");
        assert!(
            r.contains("error:") && r.contains("retry"),
            "prose preserved: {r}"
        );
        assert!(r.contains("[REDACTED]"));
        let r2 = redact("prefix AKIAIOSFODNN7EXAMPLE0123456789 suffix");
        assert!(r2.contains("[REDACTED]") && r2.contains("prefix") && r2.contains("suffix"));
    }

    #[test]
    fn usage_reads_cache_tokens_from_supported_shapes() {
        let openai: Usage = serde_json::from_value(serde_json::json!({
            "prompt_tokens": 100,
            "prompt_tokens_details": { "cached_tokens": 75 }
        }))
        .unwrap();
        assert_eq!(openai.cached_prompt_tokens(), Some(75));

        let anthropic: Usage = serde_json::from_value(serde_json::json!({
            "prompt_tokens": 100,
            "cache_read_input_tokens": 60
        }))
        .unwrap();
        assert_eq!(anthropic.cached_prompt_tokens(), Some(60));
    }

    /// Companion unit tests to the wire-level integration tests in
    /// `tests/provider.rs`. They pin the `has_meaningful_message` helper
    /// in isolation so the schema-invalid response shape is rejected even
    /// if a future refactor moves the helper. `{}` and `{"message":{}}`
    /// must fail; `{"message":{"content":""}}` must also fail (the
    /// provider was given a turn and returned nothing); whitespace-only
    /// counts as nothing; a real answer counts.
    #[test]
    fn completion_choice_meaningful_message_rejects_schema_invalid_shapes() {
        // `{}` — no `message` at all -> deserialize with `message = None`.
        let placeholder: CompletionChoice = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(
            !placeholder.has_meaningful_message(),
            "an entirely-empty choice object is schema-invalid"
        );

        // `{"message": {}}` — `message` present, every field None.
        let empty_msg: CompletionChoice =
            serde_json::from_value(serde_json::json!({"message": {}})).unwrap();
        assert!(
            !empty_msg.has_meaningful_message(),
            "a present-but-empty message is schema-invalid"
        );

        // `{"message": {"content": null}}` — explicit null.
        let null_content: CompletionChoice =
            serde_json::from_value(serde_json::json!({"message": {"content": null}})).unwrap();
        assert!(
            !null_content.has_meaningful_message(),
            "explicit null content is schema-invalid"
        );

        // `{"message": {"content": "   "}}` — whitespace-only answer.
        let ws: CompletionChoice =
            serde_json::from_value(serde_json::json!({"message": {"content": "   "}})).unwrap();
        assert!(
            !ws.has_meaningful_message(),
            "whitespace-only content is not a meaningful answer"
        );

        // A real `content` succeeds.
        let ok_content: CompletionChoice =
            serde_json::from_value(serde_json::json!({"message": {"content": "hi"}})).unwrap();
        assert!(
            ok_content.has_meaningful_message(),
            "non-empty content is meaningful"
        );

        // A reasoning-only answer also counts (with show_reasoning=true).
        let reasoning_only: CompletionChoice = serde_json::from_value(serde_json::json!({
            "message": {"content": null, "reasoning_content": "thinking aloud"}
        }))
        .unwrap();
        assert!(
            reasoning_only.has_meaningful_message(),
            "a non-empty reasoning_content counts as meaningful"
        );
    }

    // -----------------------------------------------------------------
    // OpenAI Responses API — unit tests for the wire-shaping helpers.
    // Companion to the integration tests in `tests/provider.rs`, which
    // exercise the full HTTP path. These pin `responses_body()` and
    // the `ResponsesResponse` parser in isolation so a future refactor
    // that moves them stays covered:
    //   * system message inline as `role: "system"` (not promoted),
    //   * `max_output_tokens` instead of `max_tokens`,
    //   * `temperature` + `stream` passed through verbatim,
    //   * `reasoning_effort` translated to `reasoning: {effort: …}`,
    //   * `input` is an array of `{role, content}` items (never the
    //     chat-shaped `messages`),
    //   * `ResponsesResponse.joined_text` concatenates every
    //     `output[].content[].type == "output_text"` block and ignores
    //     `reasoning` / tool / function-call entries.
    // -----------------------------------------------------------------

    fn responses_unit_req() -> ChatRequest {
        ChatRequest {
            model: "gpt-5".into(),
            messages: vec![
                Message::new("system", "be terse"),
                Message::new("developer", "answer in one sentence"),
                Message::new("user", "hi"),
                Message::new("assistant", "hello"),
            ],
            max_tokens: 32,
            temperature: Some(0.25),
            stream: true,
            show_reasoning: false,
            reasoning_effort: Some("medium".into()),
        }
    }

    #[test]
    fn responses_body_emits_input_array_with_system_dev_user_assistant_items() {
        let body = responses_body(&responses_unit_req());
        // `messages` MUST NOT appear in the Responses body — the
        // chat-shaped field name would reject at the backend or, at
        // best, be silently ignored.
        assert!(
            body.get("messages").is_none(),
            "messages must not appear in a responses body: {body}"
        );
        let input = body
            .get("input")
            .and_then(|v| v.as_array())
            .expect("input array");
        assert_eq!(input.len(), 4, "all messages must pass through: {body}");
        // `system` and `developer` collapse onto the same `system`
        // role in `input` (the Responses API spec treats them as
        // siblings; collapsing them here keeps the wire body
        // deterministic regardless of how the caller labeled the
        // message).
        assert_eq!(input[0]["role"], "system");
        assert_eq!(input[0]["content"], "be terse");
        assert_eq!(input[1]["role"], "system");
        assert_eq!(input[1]["content"], "answer in one sentence");
        assert_eq!(input[2]["role"], "user");
        assert_eq!(input[2]["content"], "hi");
        assert_eq!(input[3]["role"], "assistant");
        assert_eq!(input[3]["content"], "hello");
    }

    #[test]
    fn responses_body_uses_max_output_tokens_and_keeps_temperature_stream() {
        let body = responses_body(&responses_unit_req());
        assert_eq!(body["model"], "gpt-5");
        assert_eq!(body["max_output_tokens"], 32);
        // `max_tokens` MUST NOT appear — the chat-shaped field name
        // would either be silently ignored or rejected by the
        // Responses API depending on the gateway.
        assert!(
            body.get("max_tokens").is_none(),
            "max_tokens must not appear in a responses body: {body}"
        );
        assert_eq!(body["temperature"], 0.25);
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn responses_body_translates_reasoning_effort_into_reasoning_object() {
        let body = responses_body(&responses_unit_req());
        assert_eq!(
            body.get("reasoning")
                .and_then(|r| r.get("effort"))
                .and_then(|e| e.as_str()),
            Some("medium"),
            "reasoning_effort must be translated to reasoning: {{effort: ...}}: {body}"
        );
        // No top-level `reasoning_effort` field — the Responses API
        // would reject it.
        assert!(
            body.get("reasoning_effort").is_none(),
            "raw reasoning_effort must not appear: {body}"
        );
    }

    #[test]
    fn responses_body_without_reasoning_effort_omits_reasoning_field() {
        let mut req = responses_unit_req();
        req.reasoning_effort = None;
        let body = responses_body(&req);
        assert!(
            body.get("reasoning").is_none(),
            "no reasoning object when reasoning_effort is unset: {body}"
        );
    }

    #[test]
    fn responses_response_joined_text_concatenates_output_text_blocks() {
        let parsed: ResponsesResponse = serde_json::from_value(serde_json::json!({
            "output": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {"type": "output_text", "text": "Hello "},
                        {"type": "output_text", "text": "world"}
                    ]
                }
            ]
        }))
        .unwrap();
        assert_eq!(parsed.joined_text(), "Hello \nworld");
        assert!(parsed.has_meaningful_text());
    }

    #[test]
    fn responses_response_joined_text_ignores_non_message_and_non_text_items() {
        let parsed: ResponsesResponse = serde_json::from_value(serde_json::json!({
            "output": [
                {
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": "thinking"}]
                },
                {
                    "type": "function_call",
                    "name": "lookup",
                    "arguments": "{}"
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "refusal", "refusal": "nope"}]
                }
            ]
        }))
        .unwrap();
        assert_eq!(parsed.joined_text(), "");
        assert!(!parsed.has_meaningful_text());
    }

    #[test]
    fn responses_response_joined_text_skips_whitespace_only_text() {
        let parsed: ResponsesResponse = serde_json::from_value(serde_json::json!({
            "output": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "   \n  "}]
                }
            ]
        }))
        .unwrap();
        assert!(!parsed.has_meaningful_text());
    }

    #[test]
    fn responses_response_extracts_input_and_output_token_counts() {
        let parsed: ResponsesResponse = serde_json::from_value(serde_json::json!({
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "x"}]
            }],
            "usage": {"input_tokens": 9, "output_tokens": 4, "total_tokens": 13}
        }))
        .unwrap();
        let (prompt, completion) = match parsed.usage {
            Some(ref u) => (u.input_tokens, u.output_tokens),
            None => (None, None),
        };
        assert_eq!(prompt, Some(9));
        assert_eq!(completion, Some(4));
        // Mirrors the `total_tokens` round-trip — proves the field is
        // accepted even though the wire adapter doesn't surface it
        // upstream.
        assert_eq!(parsed.usage.as_ref().and_then(|u| u.total_tokens), Some(13));
    }

    // -----------------------------------------------------------------
    // Z-10: Anthropic Messages API body — `temperature` forwarding.
    //
    // `anthropic_body()` silently dropped `req.temperature` from the
    // wire body, so a reviewer / health-probe call that requested
    // `temperature = 0` would get the model default and become
    // nondeterministic. The Responses path already forwards
    // `temperature`; mirror it on the Anthropic branch. The pin:
    //   * `temperature = Some(0.0)` MUST emit `"temperature": 0.0` on
    //     the wire (the deterministic-call contract a reviewer or
    //     health probe depends on),
    //   * `temperature = None` MUST NOT emit a `temperature` field at
    //     all (an explicit `None` means "let the model pick its own
    //     default", which is the only correct way to opt back in to
    //     the pre-fix behavior).
    //
    // Tests exercise the REAL `anthropic_body()` function (not a
    // reimplementation) so a future refactor that moves the helper
    // stays covered.
    // -----------------------------------------------------------------

    fn anthropic_unit_req(temperature: Option<f32>) -> ChatRequest {
        ChatRequest {
            model: "claude-3-5-sonnet".into(),
            messages: vec![
                Message::new("system", "be terse"),
                Message::new("user", "hi"),
            ],
            max_tokens: 16,
            temperature,
            stream: false,
            show_reasoning: false,
            reasoning_effort: None,
        }
    }

    /// `Some(0.0)` MUST round-trip to `"temperature": 0.0` on the wire
    /// so a deterministic reviewer / health-probe call actually gets
    /// the temperature it asked for. Pre-fix this assertion fails
    /// because `anthropic_body` silently drops the field.
    #[test]
    fn anthropic_body_forwards_some_temperature() {
        let body = anthropic_body(&anthropic_unit_req(Some(0.0)));
        assert_eq!(
            body.get("temperature"),
            Some(&serde_json::json!(0.0)),
            "Some(0.0) must round-trip as `\"temperature\": 0.0`: {body}"
        );
    }

    /// `None` MUST NOT emit a `temperature` key at all — the
    /// Anthropic wire protocol treats the absence of the field as
    /// "use the model default", and a stale `temperature: 0.0` from
    /// a previous request would silently downgrade the call. Pre-fix
    /// the field was never emitted regardless of the request, so this
    /// assertion also pins the unchanged None path.
    #[test]
    fn anthropic_body_omits_temperature_when_none() {
        let body = anthropic_body(&anthropic_unit_req(None));
        assert!(
            body.get("temperature").is_none(),
            "None must not emit a temperature field: {body}"
        );
    }

    // -----------------------------------------------------------------
    // KNEMON per-account wiring — regression tests for the follow-up
    // that threaded `SubscriptionPlan::effective_account_id()` through
    // every capture / counter / routing path. The schema-validation
    // half already landed (config.rs); these tests pin the runtime
    // half so a future refactor can't silently re-collapse two
    // configured accounts onto the literal `"default"` key.
    //
    // The three regression cases the task calls out:
    //   (a) two configured accounts on the same `(provider, tier)` keep
    //       separate counter / snapshot rows (no collision);
    //   (b) the report/route rendering for a multi-account provider
    //       shows the distinguishing account label;
    //   (c) a legacy single-default config still resolves to
    //       `DEFAULT_ACCOUNT_ID` end-to-end (byte-identical to before).
    // -----------------------------------------------------------------

    /// Build a `Provider` fixture whose `SubscriptionPlan` carries an
    /// `account_id` (or `None`, for the legacy cases). The provider id,
    /// base_url, kind, and tier are stable across the four regression
    /// tests so only the `account_id` field varies.
    fn fixture_subscription_provider(account_id: Option<&str>) -> Provider {
        Provider {
            id: "minimax".into(),
            base_url: "https://api.minimax.io/v1".into(),
            kind: "openai-chat".into(),
            auth: Auth::None,
            paid: false,
            billing: BillingMode::Subscription,
            subscription: Some(SubscriptionPlan {
                monthly_fee_usd: 200.0,
                tier: Some("token-plan-2".into()),
                windows: vec![QuotaWindow {
                    name: "monthly".into(),
                    hours: 720,
                    unit: QuotaUnit::Tokens,
                    cap: Some(100.0),
                    models: None,
                    observability: Observability::Counter,
                    reset: ResetKind::CalendarMonthly,
                }],
                account_id: account_id.map(|s| s.to_string()),
            }),
            serves: vec!["MiniMax-".into()],
            azure_api_version: None,
        }
    }

    /// (c) Legacy single-default config: a provider with no `account_id`
    /// must resolve to `DEFAULT_ACCOUNT_ID` for both the OpenAiProvider
    /// capture wire AND the store-side `effective_account_id` accessor.
    /// This is the byte-identical-to-today guarantee.
    #[test]
    fn knemon_legacy_provider_without_account_id_resolves_to_default() {
        let legacy = fixture_subscription_provider(None);
        let prov = OpenAiProvider::new(&legacy).expect("legacy provider must build");
        assert_eq!(
            prov.account_id(),
            DEFAULT_ACCOUNT_ID,
            "no-account provider must resolve to DEFAULT_ACCOUNT_ID for back-compat"
        );
        // And the same id flows through the `effective_account_id()` accessor
        // on the subscription plan itself — i.e. CLI and provider agree.
        assert_eq!(
            legacy.subscription.as_ref().unwrap().effective_account_id(),
            DEFAULT_ACCOUNT_ID
        );
    }

    /// (c) Provider with a configured non-default `account_id` resolves
    /// to the configured label, NOT the literal `"default"`. Pre-fix
    /// this returned `"default"` and two accounts on the same tier
    /// silently collided.
    #[test]
    fn knemon_provider_with_account_id_resolves_to_configured_label() {
        let personal = fixture_subscription_provider(Some("personal"));
        let team = fixture_subscription_provider(Some("team"));
        let p_prov = OpenAiProvider::new(&personal).expect("build");
        let t_prov = OpenAiProvider::new(&team).expect("build");
        assert_eq!(p_prov.account_id(), "personal");
        assert_eq!(t_prov.account_id(), "team");
        // And the two ids MUST be distinct — the whole point of the
        // fix. Without it, both would have collapsed to "default".
        assert_ne!(p_prov.account_id(), t_prov.account_id());
    }

    /// (a) Two providers configured for the same `(provider, tier)`
    /// but with distinct `account_id`s must NOT collide in the
    /// utilization store. Pre-fix, capture_rate_limit_snapshot and
    /// capture_counter_usage hard-coded the literal `"default"`, so
    /// every account on the same tier overwrote every other. We prove
    /// the fix by feeding each account its own snapshot + counter
    /// increment and reading them back independently through the same
    /// `UtilizationStore` API the CLI uses.
    #[test]
    fn knemon_per_account_counters_do_not_collide_on_same_tier() {
        use crate::utilization::{Provider as UtilProv, UtilizationStore};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utilization.json");
        let mut store = UtilizationStore::open(&path).expect("store open");
        let now = chrono::Utc::now();
        let plan = "token-plan-2";

        // Personal account: 30 tokens used.
        let personal = fixture_subscription_provider(Some("personal"));
        let personal_acct = personal
            .subscription
            .as_ref()
            .unwrap()
            .effective_account_id();
        store.set_counter_cap(
            UtilProv::MiniMax,
            &personal_acct,
            plan,
            "monthly",
            Some(100.0),
            now,
        );
        let p_used = store.record_counter(
            UtilProv::MiniMax,
            &personal_acct,
            plan,
            "monthly",
            30.0,
            now,
        );
        assert_eq!(
            p_used, 30.0,
            "personal counter must record exactly its own 30 tokens"
        );

        // Team account: 70 tokens used, same provider + same tier.
        let team = fixture_subscription_provider(Some("team"));
        let team_acct = team.subscription.as_ref().unwrap().effective_account_id();
        store.set_counter_cap(
            UtilProv::MiniMax,
            &team_acct,
            plan,
            "monthly",
            Some(100.0),
            now,
        );
        let t_used =
            store.record_counter(UtilProv::MiniMax, &team_acct, plan, "monthly", 70.0, now);
        assert_eq!(
            t_used, 70.0,
            "team counter must record exactly its own 70 tokens, untouched by personal"
        );

        // Read back independently: each counter row holds the value
        // for its own account; neither sees the other's increment.
        let p_row = store
            .get_counter(UtilProv::MiniMax, &personal_acct, plan, "monthly")
            .expect("personal counter row");
        let t_row = store
            .get_counter(UtilProv::MiniMax, &team_acct, plan, "monthly")
            .expect("team counter row");
        assert_eq!(p_row.used_tokens, 30.0);
        assert_eq!(t_row.used_tokens, 70.0);
        // CRITICAL: the pre-fix bug collapsed both onto the literal
        // `"default"` key, so the second `record_counter` would have
        // overwritten the first (`used_tokens == 70.0` for BOTH reads).
        // Assert distinctness directly so the test FAILS if either id
        // is mistakenly passed as `"default"`.
        assert_ne!(
            p_row.used_tokens, t_row.used_tokens,
            "two accounts on the same tier must keep separate used_tokens (got collision)"
        );
        assert_eq!(p_row.account_id, "personal");
        assert_eq!(t_row.account_id, "team");
    }

    /// (a) Two snapshots with the same `(provider, plan)` but distinct
    /// `account_id`s must produce independent `UtilizationRecord` rows
    /// — i.e. the header-fed capture path's
    /// `(provider, account_id, plan)` key must include the configured
    /// id, not the literal `"default"`. We drive the public store API
    /// (the same one `capture_rate_limit_snapshot` uses internally) so
    /// a regression in either the call site or the storage key is
    /// caught.
    #[test]
    fn knemon_per_account_snapshots_do_not_collide_on_same_tier() {
        use crate::utilization::{Provider as UtilProv, RateLimitSnapshot, UtilizationStore};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utilization.json");
        let mut store = UtilizationStore::open(&path).expect("store open");
        let now = chrono::Utc::now();
        let plan = "chatgpt-pro";

        // Personal snapshot — 40% primary used.
        let personal_snap = RateLimitSnapshot {
            provider: UtilProv::OpenaiCodex,
            account_id: "personal".into(),
            plan: plan.into(),
            primary: Some(crate::utilization::WindowSnapshot {
                used_percent: Some(40.0),
                reset_at_epoch: None,
                window_minutes: Some(300),
                label: Some("primary".into()),
            }),
            secondary: None,
            has_credits: Some(true),
            observed_at: Some(now),
        };
        assert!(store.record(&personal_snap, now));

        // Team snapshot — 80% primary used, same `(provider, plan)`.
        let team_snap = RateLimitSnapshot {
            provider: UtilProv::OpenaiCodex,
            account_id: "team".into(),
            plan: plan.into(),
            primary: Some(crate::utilization::WindowSnapshot {
                used_percent: Some(80.0),
                reset_at_epoch: None,
                window_minutes: Some(300),
                label: Some("primary".into()),
            }),
            secondary: None,
            has_credits: Some(true),
            observed_at: Some(now),
        };
        assert!(store.record(&team_snap, now));

        // Read back independently: personal sees 40%, team sees 80%.
        let p_rec = store
            .get(UtilProv::OpenaiCodex, "personal", plan)
            .expect("personal record");
        let t_rec = store
            .get(UtilProv::OpenaiCodex, "team", plan)
            .expect("team record");
        assert_eq!(p_rec.primary.as_ref().unwrap().used_percent, Some(40.0));
        assert_eq!(t_rec.primary.as_ref().unwrap().used_percent, Some(80.0));
        assert_eq!(p_rec.account_id, "personal");
        assert_eq!(t_rec.account_id, "team");
    }

    /// (c) End-to-end: a legacy `SubscriptionPlan` with no `account_id`
    /// produces an `OpenAiProvider` whose `account_id()` accessor
    /// matches `DEFAULT_ACCOUNT_ID`, AND whose `plan_label()` matches
    /// the catalog tier. Pre-fix the runtime wrote the literal
    /// `"default"`; this pins the post-fix behavior so a future
    /// regression that re-introduces a hard-coded literal would flip
    /// the assertion and fail the build.
    #[test]
    fn knemon_legacy_provider_pins_default_account_id_and_plan_label() {
        let legacy = fixture_subscription_provider(None);
        let prov = OpenAiProvider::new(&legacy).expect("build");
        assert_eq!(prov.account_id(), DEFAULT_ACCOUNT_ID);
        assert_eq!(prov.plan_label(), "token-plan-2");
    }

    /// (a) End-to-end through the private `capture_rate_limit_snapshot`
    /// helper: feed it real `x-codex-*` headers with one `account_id`,
    /// then again with a different `account_id`, and verify the rows
    /// land under their respective keys instead of colliding on
    /// `"default"`. This pins the integration point where the
    /// `OpenAiProvider` wire-up threads the configured id through the
    /// capture helper, independent of the public `OpenAiProvider` API
    /// surface.
    #[test]
    fn knemon_capture_rate_limit_snapshot_threads_account_id_through() {
        use crate::utilization::{Provider as UtilProv, UtilizationStore};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utilization.json");
        // The capture helper writes to `default_store_path()` —
        // override `ZODER_HOME` so the test doesn't touch
        // `~/.zoder/utilization.json`.
        std::env::set_var("ZODER_HOME", dir.path());

        // Real Codex `x-codex-*` headers — `from_headers` needs at
        // least one of the family to detect the vendor.
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-codex-plan-type", "pro".parse().unwrap());
        headers.insert("x-codex-primary-used-percent", "60".parse().unwrap());
        headers.insert(
            "x-codex-primary-reset-after-seconds",
            "600".parse().unwrap(),
        );

        // Personal account: capture under its own key.
        capture_rate_limit_snapshot(
            &headers,
            "https://chatgpt.com/backend-api/codex",
            "openai-responses",
            "chatgpt-pro",
            "personal",
        );
        // Team account: same provider+plan, distinct id.
        capture_rate_limit_snapshot(
            &headers,
            "https://chatgpt.com/backend-api/codex",
            "openai-responses",
            "chatgpt-pro",
            "team",
        );

        // Read back from the persisted store. Both rows must exist
        // and report the correct account_id.
        let store = UtilizationStore::open(&path).expect("store opens");
        let p = store
            .get(UtilProv::OpenaiCodex, "personal", "chatgpt-pro")
            .expect("personal snapshot persisted under its own account_id");
        let t = store
            .get(UtilProv::OpenaiCodex, "team", "chatgpt-pro")
            .expect("team snapshot persisted under its own account_id");
        assert_eq!(p.account_id, "personal");
        assert_eq!(t.account_id, "team");
        assert_eq!(p.primary.as_ref().unwrap().used_percent, Some(60.0));
        assert_eq!(t.primary.as_ref().unwrap().used_percent, Some(60.0));

        std::env::remove_var("ZODER_HOME");
    }
}
