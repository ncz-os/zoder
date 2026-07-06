//! Provider layer: OpenAI-compatible chat calls with streaming + LiteLLM
//! telemetry extraction (exact per-call cost, served backend, fallbacks).

use crate::config::Provider;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::time::Duration;
use tokio::time::{timeout, Instant};

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

/// A provider call failure with enough structure to drive retries + fallback.
#[derive(Debug, Clone, thiserror::Error)]
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
}

impl ProviderError {
    fn new(kind: ErrKind, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind,
            status: None,
            retry_after: None,
            emitted: false,
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
    pub temperature: f32,
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
    fn cached_prompt_tokens(&self) -> Option<u64> {
        self.prompt_tokens_details
            .as_ref()
            .and_then(|details| details.cached_tokens)
            .or(self.cache_read_input_tokens)
    }
}

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}
#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
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
    #[serde(default)]
    message: CompletionMessage,
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
fn endpoint_url(base_url: &str, kind: &str, suffix: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if kind == "azure-openai" {
        let ver =
            std::env::var("AZURE_OPENAI_API_VERSION").unwrap_or_else(|_| "2024-10-21".to_string());
        return format!("{base}/{suffix}?api-version={ver}");
    }
    if base.ends_with("/v1") || base.contains("/v1/") {
        format!("{base}/{suffix}")
    } else {
        format!("{base}/v1/{suffix}")
    }
}

pub struct OpenAiProvider {
    base_url: String,
    /// Provider wire kind: `openai-chat` | `openai-responses` | `azure-openai`
    /// | `custom`. Drives endpoint routing (Azure uses a deployment path +
    /// `?api-version`, others the OpenAI `/v1/...` convention).
    kind: String,
    /// Pre-resolved auth header `(name, value)` — `Authorization: Bearer …`
    /// for bearer styles, or a custom `api-key`-style header for enterprise
    /// gateways. `None` when the provider needs no credential.
    auth_header: Option<(String, String)>,
    /// Provider id from the config (e.g. `"minimax"`). Used by the
    /// counter-fed utilization wire-up to decide whether a response
    /// belongs to a MiniMax provider (which publishes no rate-limit
    /// headers, so its usage has to be counted locally).
    provider_id: String,
    /// Plan label used as the `plan` key in the utilization store. The
    /// catalog tier when the provider has a `subscription.tier`; the
    /// provider id otherwise. Always set so the store key is stable.
    plan_label: String,
    client: reqwest::Client,
    request_timeout: Duration,
    idle_timeout: Duration,
}

impl OpenAiProvider {
    pub fn new(p: &Provider) -> anyhow::Result<Self> {
        let plan_label = p
            .subscription
            .as_ref()
            .map(|s| s.tier.clone().unwrap_or_else(|| "explicit".to_string()))
            .unwrap_or_else(|| p.id.clone());
        Ok(Self {
            base_url: p.base_url.trim_end_matches('/').to_string(),
            kind: p.kind.clone(),
            auth_header: p.auth.header_pair(),
            provider_id: p.id.clone(),
            plan_label,
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .pool_idle_timeout(Duration::from_secs(90))
                .build()?,
            request_timeout: Duration::from_secs(env_secs(
                "ZODER_TIMEOUT_S",
                DEFAULT_REQUEST_TIMEOUT_S,
            )),
            idle_timeout: Duration::from_secs(env_secs("ZODER_IDLE_S", DEFAULT_IDLE_TIMEOUT_S)),
        })
    }

    /// Build an endpoint URL for `suffix` (e.g. `"chat/completions"`,
    /// `"models"`), normalizing so we never emit `/v1/v1/...`. The configured
    /// `base_url` may or may not already include the `/v1` version segment;
    /// `azure-openai` uses its own deployment route (the base already carries
    /// the deployment path) with a `?api-version` (override via
    /// `AZURE_OPENAI_API_VERSION`).
    fn endpoint(&self, suffix: &str) -> String {
        endpoint_url(&self.base_url, &self.kind, suffix)
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
    pub async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
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
        let mut body = serde_json::json!({
            "model": req.model,
            "messages": req.messages,
            "max_tokens": req.max_tokens,
            "temperature": req.temperature,
            "stream": req.stream,
        });
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
        let mut rb = self
            .client
            .post(self.endpoint("chat/completions"))
            .json(&self.body(req));
        if let Some((name, value)) = &self.auth_header {
            rb = rb.header(name.as_str(), value.as_str());
        }
        rb
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
        // poison the request.
        capture_rate_limit_snapshot(resp.headers(), &self.base_url, &self.kind, &self.plan_label);
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
            });
        }
        if req.stream {
            let r = self.consume_stream(req, resp, telemetry, sink).await?;
            // Counter-fed KNEMON capture (Layer 3B) runs after the body
            // is consumed so we have prompt+completion to count. The
            // header-fed capture above already ran on the response
            // headers; for MiniMax that path is a clean no-op, so this
            // is the only signal that provider contributes. Best-effort.
            capture_counter_usage(
                &self.provider_id,
                &self.base_url,
                &self.plan_label,
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
        let body = self.read_limited_body(resp, "chat completion").await?;
        let parsed: ChatCompletion = serde_json::from_slice(&body).map_err(|e| {
            ProviderError::new(
                ErrKind::Decode,
                format!("malformed chat-completion response: {e}"),
            )
        })?;
        let msg = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message)
            .ok_or_else(|| {
                ProviderError::new(
                    ErrKind::Decode,
                    "malformed chat-completion response: no completion choices",
                )
            })?;
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

    /// Streaming path: parse newline-delimited SSE frames as they arrive.
    async fn consume_stream(
        &self,
        req: &ChatRequest,
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
        };
        let mut done = false;
        let mut saw_choice = false;
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
                let val: serde_json::Value = match serde_json::from_str(payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
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
                let Ok(parsed) = serde_json::from_value::<StreamChunk>(val) else {
                    continue;
                };
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
/// The account is not knowable from a single response, so it uses the stable
/// `"default"` key. The configured plan label is passed by `OpenAiProvider`
/// and normalized after parsing so capture and scenario/report lookup use the
/// same tuple even when Codex publishes a different display label.
fn capture_rate_limit_snapshot(
    headers: &reqwest::header::HeaderMap,
    base_url: &str,
    _kind: &str,
    plan: &str,
) {
    let Some(provider) = detect_utilization_provider(headers, base_url) else {
        return;
    };
    let Some(mut snap) =
        crate::utilization::RateLimitSnapshot::from_headers(headers, provider, "default", plan)
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
/// Only windows with `observability = Counter` are incremented;
/// `PercentOnly` windows are intentionally untouched (the spec is
/// explicit: "Only windows with observability=Counter accumulate token
/// counts; PercentOnly windows are never locally computed").
fn capture_counter_usage(
    provider_id: &str,
    base_url: &str,
    plan: &str,
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
                    "default",
                    plan,
                    &w.name,
                    (w.reset == crate::config::ResetKind::Rolling).then_some(w.hours),
                    now,
                );
                store.set_counter_cap(
                    crate::utilization::Provider::MiniMax,
                    "default",
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
                    "default",
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
        "default",
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

    #[test]
    fn endpoint_url_never_doubles_v1() {
        assert_eq!(
            endpoint_url(
                "https://api.example.com/v1",
                "openai-chat",
                "chat/completions"
            ),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            endpoint_url("https://api.example.com/v1/", "openai-chat", "models"),
            "https://api.example.com/v1/models"
        );
        assert_eq!(
            endpoint_url("https://gw.example.com", "openai-chat", "chat/completions"),
            "https://gw.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn endpoint_url_azure_uses_deployment_route_no_v1() {
        std::env::set_var("AZURE_OPENAI_API_VERSION", "2024-10-21");
        let u = endpoint_url(
            "https://res.openai.azure.com/openai/deployments/gpt4o",
            "azure-openai",
            "chat/completions",
        );
        assert_eq!(
            u,
            "https://res.openai.azure.com/openai/deployments/gpt4o/chat/completions?api-version=2024-10-21"
        );
        assert!(!u.contains("/v1/"), "azure route must not inject /v1");
        std::env::remove_var("AZURE_OPENAI_API_VERSION");
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
}
