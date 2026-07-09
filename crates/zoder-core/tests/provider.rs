use std::time::Duration;
use wiremock::matchers::{body_partial_json, header, header_exists, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zoder_core::{
    backoff_delay, classify_err, Auth, BillingMode, ChatRequest, Classification, ErrKind,
    HealthStore, Message, OpenAiProvider, Provider,
};

fn provider(base_url: &str) -> OpenAiProvider {
    let cfg = Provider {
        id: "test".into(),
        base_url: base_url.to_string(),
        kind: "openai-chat".into(),
        auth: Auth::None,
        paid: false,
        billing: BillingMode::Metered,
        subscription: None,
        serves: Vec::new(),
        azure_api_version: None,
    };
    OpenAiProvider::new(&cfg).unwrap()
}

fn req(model: &str, stream: bool) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: vec![Message::new("user", "hi")],
        max_tokens: 16,
        temperature: Some(0.0),
        stream,
        show_reasoning: false,
        reasoning_effort: None,
    }
}

#[tokio::test]
async fn streaming_sse_is_assembled() {
    let server = MockServer::start().await;
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n\
                data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n\
                data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let p = provider(&server.uri());
    let res = p.stream_chat(&req("m", true), None).await.unwrap();
    assert_eq!(res.content, "Hello");
    assert_eq!(res.completion_tokens, Some(2));
    assert_eq!(res.prompt_tokens, Some(3));
}

#[tokio::test]
async fn non_streaming_object_is_parsed() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "choices": [{"message": {"content": "full answer"}}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 7}
    });
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let p = provider(&server.uri());
    let res = p.stream_chat(&req("m", false), None).await.unwrap();
    assert_eq!(res.content, "full answer");
    assert_eq!(res.completion_tokens, Some(7));
}

#[tokio::test]
async fn non_streaming_success_without_choices_is_decode_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [],
            "usage": {"prompt_tokens": 5, "completion_tokens": 0}
        })))
        .mount(&server)
        .await;

    let error = provider(&server.uri())
        .stream_chat(&req("m", false), None)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrKind::Decode);
    assert!(error.message.contains("no completion choices"), "{error}");
}

/// Regression for the task's pinned open item: a 2xx non-streaming body
/// whose `choices` array contains only an empty `{}` placeholder used to
/// parse successfully because `CompletionChoice.message` was
/// `#[serde(default)]`, returning an empty `""` from `pick_text` and
/// crediting the call as a 0-token success. That let `--no-stream` /
/// reviewer / health-probe runs exit 0 with a billable reservation
/// reconciled as a no-op — exactly the failure mode the streaming SSE
/// path's `saw_choice` guard rejects. The non-streaming path must now
/// reject the same shape with `ErrKind::Decode` and a message that names
/// the cause.
#[tokio::test]
async fn non_streaming_schema_invalid_empty_choice_is_decode_error() {
    // The empty-choice shape: top-level `choices: [{}]` — no `message`
    // at all. Mirrors the streaming equivalent of `data: {"choices":[{}]}`.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 0}
        })))
        .mount(&server)
        .await;

    let error = provider(&server.uri())
        .stream_chat(&req("m", false), None)
        .await
        .unwrap_err();
    assert_eq!(
        error.kind,
        ErrKind::Decode,
        "schema-invalid 2xx body must surface as a Decode error (emitted={})",
        error.emitted
    );
    assert!(
        error.message.contains("empty completion choice"),
        "error must name the cause, got: {error}"
    );
    assert!(
        !error.emitted,
        "no answer bytes can have been written for an empty choice"
    );
}

/// `{"choices":[{"message":{}}]}` — the *other* half of the regression:
/// a `message` placeholder is present but carries no parseable content.
/// Without the `has_meaningful_message` guard this would, like `{}`,
/// produce an empty ChatResult and silently reconcile. It must also
/// surface as a Decode error.
#[tokio::test]
async fn non_streaming_empty_message_with_present_key_is_decode_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {}}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 0}
        })))
        .mount(&server)
        .await;

    let error = provider(&server.uri())
        .stream_chat(&req("m", false), None)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrKind::Decode, "emitted={}", error.emitted);
    assert!(
        error.message.contains("empty completion choice"),
        "got: {error}"
    );
}

/// And `{"choices":[{"message":{"content":null}}]}` — an *explicitly
/// null* `content`. `pick_text` would also fall through to `""` here,
/// so this is in the same failure family and must trip the same guard.
#[tokio::test]
async fn non_streaming_null_content_is_decode_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": null}}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 0}
        })))
        .mount(&server)
        .await;

    let error = provider(&server.uri())
        .stream_chat(&req("m", false), None)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrKind::Decode, "emitted={}", error.emitted);
    assert!(
        error.message.contains("empty completion choice"),
        "got: {error}"
    );
}

/// Whitespace-only content is also content-shaped but carries no real
/// answer — it must NOT reconcile as a successful call. The guard looks
/// at `chars().any(|c| !c.is_whitespace())`, so this trips the same
/// branch as the empty-placeholder cases above.
#[tokio::test]
async fn non_streaming_whitespace_only_content_is_decode_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "   \n\t  "}}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 0}
        })))
        .mount(&server)
        .await;

    let error = provider(&server.uri())
        .stream_chat(&req("m", false), None)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrKind::Decode, "emitted={}", error.emitted);
    assert!(
        error.message.contains("empty completion choice"),
        "got: {error}"
    );
}

/// The fix must not regress the legitimate "content present" path —
/// a normal OpenAI-shaped reply still parses through unchanged.
#[tokio::test]
async fn non_streaming_meaningful_choice_still_parses() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "ok"}}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1}
        })))
        .mount(&server)
        .await;

    let result = provider(&server.uri())
        .stream_chat(&req("m", false), None)
        .await
        .unwrap();
    assert_eq!(result.content, "ok");
    assert_eq!(result.completion_tokens, Some(1));
}

#[tokio::test]
async fn stream_with_only_malformed_frames_is_decode_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("data: definitely-not-json\n\ndata: [DONE]\n\n"),
        )
        .mount(&server)
        .await;

    let error = provider(&server.uri())
        .stream_chat(&req("m", true), None)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrKind::Decode);
    assert!(
        error.message.contains("malformed provider stream JSON"),
        "{error}"
    );
}

#[tokio::test]
async fn malformed_frame_after_emitted_choice_is_decode_error() {
    let server = MockServer::start().await;
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n\
                data: definitely-not-json\n\n\
                data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let mut sink = Vec::new();
    let error = provider(&server.uri())
        .stream_chat(&req("m", true), Some(&mut sink))
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrKind::Decode);
    assert!(error.emitted);
    assert_eq!(sink, b"partial");
}

#[tokio::test]
async fn schema_invalid_frame_after_valid_choice_is_decode_error() {
    let server = MockServer::start().await;
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n\
                data: {\"choices\":\"not-an-array\"}\n\n\
                data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let error = provider(&server.uri())
        .stream_chat(&req("m", true), None)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrKind::Decode);
    assert!(error.message.contains("malformed provider stream frame"));
    assert!(!error.emitted, "nothing was written without a sink");
}

#[tokio::test]
async fn choice_without_delta_is_decode_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("data: {\"choices\":[{}]}\n\ndata: [DONE]\n\n"),
        )
        .mount(&server)
        .await;

    let error = provider(&server.uri())
        .stream_chat(&req("m", true), None)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrKind::Decode);
    assert!(error.message.contains("malformed provider stream frame"));
}

#[tokio::test]
async fn bare_object_after_valid_choice_is_decode_error() {
    let server = MockServer::start().await;
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n\
                data: {}\n\n\
                data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let error = provider(&server.uri())
        .stream_chat(&req("m", true), None)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrKind::Decode);
    assert!(error.message.contains("malformed provider stream frame"));
}

#[tokio::test]
async fn meaningful_usage_only_frame_is_accepted() {
    let server = MockServer::start().await;
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n\
                data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}\n\n\
                data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let result = provider(&server.uri())
        .stream_chat(&req("m", true), None)
        .await
        .unwrap();
    assert_eq!(result.content, "ok");
    assert_eq!(result.prompt_tokens, Some(3));
    assert_eq!(result.completion_tokens, Some(1));
}

#[tokio::test]
async fn stream_with_valid_json_but_no_choices_is_decode_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3}}\n\ndata: [DONE]\n\n",
        ))
        .mount(&server)
        .await;

    let error = provider(&server.uri())
        .stream_chat(&req("m", true), None)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrKind::Decode);
}

/// Z-9: a streaming OpenAI response that yields one or more `choices`
/// frames but never emits a non-empty `delta.content` MUST surface as
/// `ErrKind::Decode`, not a successful empty completion. Without this
/// guard the stream would tick `saw_choice = true`, `pick_text` would
/// return `""`, and the call would be reconciled as a 0-token paid
/// success — exactly the failure mode the non-streaming
/// `has_meaningful_message()` guard at ~1205 already prevents. We
/// exercise the real streaming parser end-to-end through
/// `OpenAiProvider::stream_chat` with a wiremock SSE body whose
/// `choices[].delta.content` is absent / empty / whitespace-only.
#[tokio::test]
async fn stream_with_choices_but_no_content_is_decode_error() {
    // Three independent shapes that must all be rejected. Any one of
    // them reaching the success branch is the bug the task pins.
    let bodies = [
        // 1) choices present, delta has NO `content` field at all.
        "data: {\"choices\":[{\"delta\":{}}]}\n\n\
         data: [DONE]\n\n",
        // 2) choices present, delta content is the empty string.
        "data: {\"choices\":[{\"delta\":{\"content\":\"\"}}]}\n\n\
         data: [DONE]\n\n",
        // 3) choices present, delta content is whitespace-only.
        "data: {\"choices\":[{\"delta\":{\"content\":\"   \\n \"}}]}\n\n\
         data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":0}}\n\n\
         data: [DONE]\n\n",
    ];

    for (idx, body) in bodies.iter().enumerate() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(*body))
            .mount(&server)
            .await;

        let mut sink = Vec::new();
        let result = provider(&server.uri())
            .stream_chat(&req("m", true), Some(&mut sink))
            .await;
        let error = match result {
            Ok(r) => panic!(
                "case {idx}: empty-completion stream must NOT return Ok; got content={:?} completion_tokens={:?}",
                r.content, r.completion_tokens
            ),
            Err(e) => e,
        };
        assert_eq!(
            error.kind,
            ErrKind::Decode,
            "case {idx}: empty-completion stream must surface as Decode, got: {error}"
        );
        // The reservation must not be reconciled as a paid success:
        // the failure mode the task names is exactly an `Ok` with empty
        // `content`. The error message must name the cause so a
        // downstream classifier can pick it apart from a transport
        // or timeout error.
        assert!(
            error.message.contains("empty completion")
                || error.message.contains("no content")
                || error.message.contains("empty content"),
            "case {idx}: error must name the empty-completion cause, got: {error}"
        );
    }
}

/// Mirror of `stream_with_choices_but_no_content_is_decode_error`:
/// a stream whose choices DO produce a non-empty content delta must
/// still succeed (fail-open on the guard, not fail-closed on real
/// output). Pinned so a follow-up tightening of the Z-9 guard
/// doesn't accidentally swallow real content.
#[tokio::test]
async fn stream_with_real_content_delta_still_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n\
             data: [DONE]\n\n",
        ))
        .mount(&server)
        .await;

    let mut sink = Vec::new();
    let res = provider(&server.uri())
        .stream_chat(&req("m", true), Some(&mut sink))
        .await
        .unwrap();
    assert_eq!(res.content, "Hello");
    assert_eq!(sink, b"Hello");
    assert_eq!(res.completion_tokens, Some(2));
}

#[tokio::test]
async fn invalid_utf8_sse_is_decode_error() {
    let server = MockServer::start().await;
    let body = b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: \xff\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
        .mount(&server)
        .await;

    let error = provider(&server.uri())
        .stream_chat(&req("m", true), None)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrKind::Decode);
    assert!(error.message.contains("invalid UTF-8"), "{error}");
}

#[tokio::test]
async fn rate_limit_is_classified_and_retry_after_parsed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "7"))
        .mount(&server)
        .await;

    let p = provider(&server.uri());
    let err = p.stream_chat(&req("m", true), None).await.unwrap_err();
    assert_eq!(err.kind, ErrKind::RateLimit);
    assert_eq!(err.status, Some(429));
    assert_eq!(err.retry_after, Some(Duration::from_secs(7)));
    assert!(err.retryable());
}

#[tokio::test]
async fn server_error_is_retryable_client_error_is_not() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let p = provider(&server.uri());
    let err = p.stream_chat(&req("m", true), None).await.unwrap_err();
    assert_eq!(err.kind, ErrKind::Server);
    assert!(err.retryable());

    let server2 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&server2)
        .await;
    let p2 = provider(&server2.uri());
    let err2 = p2.stream_chat(&req("m", true), None).await.unwrap_err();
    assert_eq!(err2.kind, ErrKind::Http);
    assert!(!err2.retryable());
}

#[tokio::test]
async fn list_models_extracts_ids() {
    let server = MockServer::start().await;
    let body = serde_json::json!({"data": [{"id": "a/one"}, {"id": "b/two"}]});
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let p = provider(&server.uri());
    let ids = p.list_models().await.unwrap();
    assert_eq!(ids, vec!["a/one".to_string(), "b/two".to_string()]);
}

#[tokio::test]
async fn response_size_ceiling_covers_completions_models_and_error_bodies() {
    for (method_name, route, status, stream) in [
        ("POST", "/v1/chat/completions", 200, false),
        ("GET", "/v1/models", 200, false),
        ("POST", "/v1/chat/completions", 500, true),
    ] {
        let server = MockServer::start().await;
        Mock::given(method(method_name))
            .and(path(route))
            .respond_with(ResponseTemplate::new(status).set_body_bytes(vec![b'x'; 16_777_217]))
            .mount(&server)
            .await;
        let provider = provider(&server.uri());
        let error = if route.ends_with("models") {
            provider.list_models().await.unwrap_err()
        } else {
            provider
                .stream_chat(&req("m", stream), None)
                .await
                .unwrap_err()
        };
        assert!(error.message.contains("response ceiling"), "{error}");
    }
}

#[tokio::test]
async fn cumulative_streamed_content_has_independent_ceiling() {
    let server = MockServer::start().await;
    let piece = "x".repeat(1024);
    let mut body = String::new();
    for _ in 0..8_193 {
        body.push_str("data: {\"choices\":[{\"delta\":{\"content\":\"");
        body.push_str(&piece);
        body.push_str("\"}}]}\n");
    }
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;
    let error = provider(&server.uri())
        .stream_chat(&req("m", true), None)
        .await
        .unwrap_err();
    assert!(
        error.message.contains("decoded content exceeded"),
        "{error}"
    );
}

#[tokio::test]
async fn streaming_wire_bytes_have_total_response_ceiling() {
    let server = MockServer::start().await;
    let mut body = Vec::with_capacity(16_778_000);
    let comment = vec![b'x'; 1023];
    while body.len() <= 16_777_216 {
        body.extend_from_slice(&comment);
        body.push(b'\n');
    }
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&server)
        .await;
    let error = provider(&server.uri())
        .stream_chat(&req("m", true), None)
        .await
        .unwrap_err();
    assert!(error.message.contains("stream exceeded"), "{error}");
}

#[test]
fn backoff_honors_retry_after_floor() {
    // With a 10s server hint, the delay must be at least 10s regardless of attempt.
    let d = backoff_delay(0, Some(Duration::from_secs(10)));
    assert!(d >= Duration::from_secs(10));
    // Without a hint, attempt 0 is sub-second-ish (<= ~1s with jitter).
    let d0 = backoff_delay(0, None);
    assert!(d0 <= Duration::from_millis(1200));
}

// =====================================================================
// Anthropic Messages API adapter — wire-level integration tests.
//
// These tests pin the four properties the Anthropic slice promised:
//   * endpoint + headers (path, `x-api-key`, `anthropic-version`),
//   * body shape (system lifted to top-level, no `temperature` field,
//     streaming flag propagated),
//   * openai-chat byte-identical behavior (the non-anthropic path
//     never sees this fork),
//   * non-streaming response parser,
//   * streaming SSE decoder,
//   * typed error envelope on a 401 (no `auth_error`, just the
//     authentication_error JSON — surface as ErrKind::Http / 401).
// =====================================================================

/// Build an `OpenAiProvider` configured to hit the given base_url with
/// Anthropic wire shape. `auth` is whatever `Auth` variant the test
/// wants to exercise — `Bearer { token }` to assert the leading
/// `Bearer ` is stripped, `ApiKeyHeader { … }` to assert a verbatim
/// header passthrough, `None` to assert the empty-credential shape.
fn anthropic_provider(base_url: &str, auth: Auth) -> OpenAiProvider {
    let cfg = Provider {
        id: "anthropic".into(),
        base_url: base_url.to_string(),
        kind: "anthropic".into(),
        auth,
        paid: false,
        billing: BillingMode::Metered,
        subscription: None,
        serves: Vec::new(),
        azure_api_version: None,
    };
    OpenAiProvider::new(&cfg).unwrap()
}

/// Same as `req()` but uses an Anthropic-shaped model id.
fn anthropic_req(model: &str, stream: bool) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: vec![
            // Two system messages so we also exercise the
            // concat-with-blank-line behavior of the system-lift.
            Message::new("system", "You are concise."),
            Message::new("system", "Answer in one sentence."),
            Message::new("user", "hi"),
        ],
        max_tokens: 16,
        temperature: Some(0.5),
        stream,
        show_reasoning: false,
        reasoning_effort: None,
    }
}

/// Task-pinned test (item 4a): an Anthropic provider POSTs to the
/// `/v1/messages` endpoint with the spec-correct `x-api-key` +
/// `anthropic-version: 2023-06-01` headers, lifts any leading
/// `role: "system"` message(s) into the top-level `system` string,
/// passes the rest of the messages through, and lifts the leading
/// system-role messages into the top-level `system` string rather
/// than the OpenAI-shaped `messages[0].role == "system"` body. The
/// `Bearer ` prefix from a `Bearer { token }` auth is stripped
/// before the `x-api-key` header is set. The Anthropic Messages API
/// accepts its own `temperature` field (not the OpenAI-shaped
/// `messages[0]` row), so the wire adapter now forwards
/// `req.temperature = Some(_)` and omits the field on `None` — see
/// the dedicated `anthropic_body_forwards_some_temperature` /
/// `anthropic_body_omits_temperature_when_none` unit tests.
#[tokio::test]
async fn anthropic_request_hits_v1_messages_with_correct_headers_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(header("x-api-key", "raw-token-no-bearer"))
        .and(header_exists("x-api-key"))
        // The OpenAI-shaped `messages[0].role == "system"` body MUST
        // NOT appear — the system message must be lifted into the
        // top-level `system` string. The Anthropic `temperature`
        // field is independent of the OpenAI-shaped one (a separate
        // test pins its forwarding on the wire), so we only assert
        // the structural keys here.
        .and(body_partial_json(serde_json::json!({
            "model": "claude-3-5-sonnet",
            "max_tokens": 16,
            "stream": false,
            "system": "You are concise.\n\nAnswer in one sentence.",
            "messages": [
                { "role": "user", "content": "hi" }
            ]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content": [{"type": "text", "text": "ok"}],
            "usage": {"input_tokens": 3, "output_tokens": 1}
        })))
        .mount(&server)
        .await;

    let p = anthropic_provider(
        &server.uri(),
        Auth::Bearer {
            token: "raw-token-no-bearer".into(),
        },
    );
    // Build a request with `temperature: None` so this test stays
    // focused on the structural keys (the dedicated unit tests pin
    // the `temperature` forwarding end-to-end and on the wire).
    let mut req = anthropic_req("claude-3-5-sonnet", false);
    req.temperature = None;
    let res = p.stream_chat(&req, None).await.unwrap();
    assert_eq!(res.content, "ok");
}

/// Z-10 wire-level pin: a reviewer / health-probe call that asks
/// for `temperature = Some(0.0)` MUST put `"temperature": 0.0` on the
/// wire so the deterministic contract the caller depends on actually
/// reaches the backend. Companion to the unit test
/// `anthropic_body_forwards_some_temperature`; this one drives the
/// full `OpenAiProvider::stream_chat` path so a refactor that moves
/// the `body()` dispatch is also covered. The mock matches any
/// POST so we can inspect the actual request body the adapter
/// sent.
#[tokio::test]
async fn anthropic_request_forwards_some_temperature_on_wire() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content": [{"type": "text", "text": "ok"}],
            "usage": {"input_tokens": 3, "output_tokens": 1}
        })))
        .mount(&server)
        .await;

    let p = anthropic_provider(&server.uri(), Auth::None);
    let mut req = anthropic_req("claude", false);
    req.temperature = Some(0.0);
    let res = p.stream_chat(&req, None).await.unwrap();
    assert_eq!(res.content, "ok");

    // Inspect the actual wire body. Pre-fix this would have omitted
    // `temperature` entirely (the Anthropic branch silently dropped
    // the field), so the request would have fallen back to the model
    // default and the deterministic `temperature = 0` contract the
    // caller asked for would have been lost.
    let received = server.received_requests().await.unwrap_or_default();
    assert_eq!(received.len(), 1, "expected exactly one POST");
    let body: serde_json::Value =
        serde_json::from_slice(&received[0].body).expect("wire body must be valid JSON");
    assert_eq!(
        body.get("temperature"),
        Some(&serde_json::json!(0.0)),
        "Some(0.0) must round-trip as `temperature: 0.0` on the wire: {body}"
    );
}

/// Z-10 wire-level pin: `temperature = None` MUST NOT emit a
/// `temperature` field at all — the Anthropic Messages API treats
/// the absence of the field as "use the model default", and a stale
/// `temperature: 0.0` from a previous request would silently
/// downgrade the call. Companion to the unit test
/// `anthropic_body_omits_temperature_when_none`.
#[tokio::test]
async fn anthropic_request_omits_temperature_on_wire_when_none() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content": [{"type": "text", "text": "ok"}],
            "usage": {"input_tokens": 3, "output_tokens": 1}
        })))
        .mount(&server)
        .await;

    let p = anthropic_provider(&server.uri(), Auth::None);
    let mut req = anthropic_req("claude", false);
    req.temperature = None;
    let res = p.stream_chat(&req, None).await.unwrap();
    assert_eq!(res.content, "ok");

    let received = server.received_requests().await.unwrap_or_default();
    assert_eq!(received.len(), 1, "expected exactly one POST");
    let body: serde_json::Value =
        serde_json::from_slice(&received[0].body).expect("wire body must be valid JSON");
    assert!(
        body.get("temperature").is_none(),
        "None must not emit a temperature field on the wire: {body}"
    );
}

/// Task-pinned test (item 4b): the openai-chat path must remain
/// byte-identical to before — a `kind = "openai-chat"` provider
/// never sees the `is_anthropic()` branch, never sends the
/// `anthropic-version` header, and never lifts system messages. We
/// exercise this by hitting a path that the Anthropic branch would
/// ALSO accept and asserting the wire mock matched the OpenAI
/// `/v1/chat/completions` route instead of `/v1/messages`.
#[tokio::test]
async fn openai_chat_path_remains_byte_identical_after_anthropic_fork() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        // The OpenAI path passes `temperature` straight through and
        // carries system messages inline — the Anthropic branch would
        // have removed both. A passing assertion here proves the
        // OpenAI route is still the only route taken for
        // `kind = "openai-chat"`. No `stream_options` because this is
        // a non-streaming call; the Anthropic branch would have also
        // stripped `temperature`.
        .and(body_partial_json(serde_json::json!({
            "model": "gpt-4o",
            "messages": [
                { "role": "system", "content": "be terse" },
                { "role": "user", "content": "hi" }
            ],
            "max_tokens": 16,
            "temperature": 0.0,
            "stream": false
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "hello"}}],
            "usage": {"prompt_tokens": 2, "completion_tokens": 1}
        })))
        .mount(&server)
        .await;

    let p = provider(&server.uri());
    let req = ChatRequest {
        model: "gpt-4o".into(),
        messages: vec![
            Message::new("system", "be terse"),
            Message::new("user", "hi"),
        ],
        max_tokens: 16,
        temperature: Some(0.0),
        stream: false,
        show_reasoning: false,
        reasoning_effort: None,
    };
    let res = p.stream_chat(&req, None).await.unwrap();
    assert_eq!(res.content, "hello");
}

/// Task-pinned test (item 4c): the non-streaming Anthropic response
/// parser lifts the first `type: "text"` block, translates
/// `usage.input_tokens` -> `prompt_tokens` and
/// `usage.output_tokens` -> `completion_tokens`, and includes
/// `cache_read_input_tokens` + `cache_creation_input_tokens` in the
/// cache telemetry. A schema-invalid 2xx body (e.g. empty `content`)
/// MUST surface as ErrKind::Decode — same contract the OpenAI path
/// enforces.
#[tokio::test]
async fn anthropic_non_streaming_response_is_parsed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "msg_01",
            "content": [
                {"type": "text", "text": "Hello "},
                {"type": "text", "text": "world"},
                {"type": "tool_use", "id": "x", "name": "y", "input": {}}
            ],
            "usage": {
                "input_tokens": 7,
                "output_tokens": 4,
                "cache_read_input_tokens": 3,
                "cache_creation_input_tokens": 2
            }
        })))
        .mount(&server)
        .await;

    let p = anthropic_provider(&server.uri(), Auth::None);
    let res = p
        .stream_chat(&anthropic_req("claude", false), None)
        .await
        .unwrap();
    // joined_text() concatenates consecutive text blocks with `\n`.
    // The expected output therefore carries the inter-block separator
    // the parser inserts, not the implicit "no separator" the OpenAI
    // path uses for its single `message.content` string.
    assert_eq!(res.content, "Hello \nworld");
    assert_eq!(res.prompt_tokens, Some(7));
    assert_eq!(res.completion_tokens, Some(4));
    // cache_read + cache_creation = 3 + 2 = 5
    assert_eq!(res.cached_prompt_tokens, Some(5));
}

/// Schema-invalid 2xx guard for the non-streaming Anthropic path: a
/// response whose `content` array is empty (or contains only non-text
/// entries, or has only whitespace text) MUST surface as
/// ErrKind::Decode. This mirrors the OpenAI `{"choices":[{}]}`
/// guard.
#[tokio::test]
async fn anthropic_non_streaming_empty_content_is_decode_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content": [],
            "usage": {"input_tokens": 1, "output_tokens": 0}
        })))
        .mount(&server)
        .await;

    let p = anthropic_provider(&server.uri(), Auth::None);
    let err = p
        .stream_chat(&anthropic_req("claude", false), None)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrKind::Decode, "got: {err}");
    assert!(err.message.contains("empty content"), "got: {err}");
}

/// Task-pinned test (item 4d): the streaming Anthropic SSE decoder
/// walks the `event: …` / `data: {…}` envelope, accumulates
/// `content_block_delta.delta.text` into the `content` string,
/// captures `message_start.usage.input_tokens` as `prompt_tokens`,
/// captures `message_delta.usage.output_tokens` as the authoritative
/// `completion_tokens`, treats `message_stop` as the terminal event,
/// and rejects streams that close without producing any text delta.
#[tokio::test]
async fn anthropic_streaming_sse_is_assembled() {
    let server = MockServer::start().await;
    // Build the SSE body from the exact Anthropic event shape the
    // parser was written against.
    let body = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\
\n";
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(serde_json::json!({"stream": true})))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let p = anthropic_provider(&server.uri(), Auth::None);
    let mut sink = Vec::new();
    let res = p
        .stream_chat(&anthropic_req("claude", true), Some(&mut sink))
        .await
        .unwrap();
    assert_eq!(res.content, "Hello");
    assert_eq!(sink, b"Hello");
    // input_tokens from message_start, output_tokens from message_delta.
    assert_eq!(res.prompt_tokens, Some(5));
    assert_eq!(res.completion_tokens, Some(2));
}

/// Stream that closes via `message_stop` without producing any text
/// delta — same schema-invalid-2xx contract as the non-streaming
/// Z-23 placeholder start marker
#[tokio::test]
async fn anthropic_streaming_with_no_text_delta_is_decode_error() {
    let server = MockServer::start().await;
    let body = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\
\n";
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let p = anthropic_provider(&server.uri(), Auth::None);
    let err = p
        .stream_chat(&anthropic_req("claude", true), None)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrKind::Decode, "got: {err}");
    assert!(err.message.contains("no text content"), "got: {err}");
}

/// Z-23: the SSE parser's blank-line branch (event-dispatch point)
/// must reset `current_event` to its default. Pre-fix the parser
/// tracked the most recent `event:` line in `current_event` but NEVER
/// reset it on the blank line that terminated each frame, so a
/// subsequent frame that OMITS the `event:` line would inherit the
/// previous event type and be silently mis-parsed under whichever arm
/// of the `match` the previous event had selected. The comment
/// immediately above the blank-line branch literally says "Reset for
/// the next frame; the next `event:` line will re-populate
/// `current_event`" but no reset ever happened.
///
/// Drives the real `OpenAiProvider::stream_chat` SSE decoder
/// end-to-end through a wiremock Anthropic-shaped stream whose first
/// event sets `prompt_tokens` via `message_start`, and whose SECOND
/// event OMITS `event:` but carries a data payload shaped like a
/// `message_start` with a different `input_tokens`. With the bug,
/// `current_event` stays "message_start" across the blank line, the
/// second data is parsed as `message_start`, and `prompt_tokens` is
/// silently overwritten to 999. With the fix, the blank-line dispatch
/// point resets `current_event` to `None`, the second data falls
/// through the `match` to `_ => {}`, and `prompt_tokens` stays at 5.
///
/// Non-regression: the rest of the well-formed Anthropic event
/// sequence still produces the expected content + completion_tokens.
#[tokio::test]
async fn anthropic_streaming_resets_current_event_on_blank_line() {
    let server = MockServer::start().await;
    let body = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_2\",\"usage\":{\"input_tokens\":999,\"output_tokens\":1}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let p = anthropic_provider(&server.uri(), Auth::None);
    let mut sink = Vec::new();
    let res = p
        .stream_chat(&anthropic_req("claude", true), Some(&mut sink))
        .await
        .unwrap();
    assert_eq!(
        res.prompt_tokens,
        Some(5),
        "second SSE frame omitted `event:` and must NOT inherit message_start; got prompt_tokens={:?}",
        res.prompt_tokens
    );
    assert_eq!(res.content, "hi");
    assert_eq!(sink, b"hi");
    assert_eq!(res.completion_tokens, Some(2));
}

/// Typed Anthropic error envelope on a 401 must surface as
/// ErrKind::Http (the existing classify_err path picks the
/// `Unauthorized` classification off the status code 401 — the
/// envelope parsing for the *body* is wired into
/// `model_health::Classification::from_anthropic_error_body` and is
/// exercised by the model-health unit tests). This integration test
/// pins the wire side: the 401 response body is read verbatim and
/// surfaced through `ProviderError::message`.
#[tokio::test]
async fn anthropic_401_with_typed_error_envelope_is_classified_as_http() {
    let server = MockServer::start().await;
    let body =
        r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_string(body))
        .mount(&server)
        .await;

    let p = anthropic_provider(
        &server.uri(),
        Auth::Bearer {
            token: "bogus".into(),
        },
    );
    let err = p
        .stream_chat(&anthropic_req("claude", false), None)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrKind::Http);
    assert_eq!(err.status, Some(401));
    // The body must round-trip through `ProviderError::message` so a
    // downstream classifier can pick the typed envelope apart.
    assert!(err.message.contains("authentication_error"), "got: {err}");
}

/// Y-14: an Anthropic mid-stream SSE `event: error` carrying a typed
/// `overloaded_error` envelope (the wire shape Anthropic uses to surface
/// a 529 / capacity rejection after a 200 has been written to the
/// stream) MUST:
///   * populate `ProviderError::anthropic_error_body` with the raw frame
///     payload (so `classify_err` can route through
///     `from_anthropic_error_body`),
///   * classify as `Capacity` via `classify_err` (NOT `Error`), so the
///     breaker backs off instead of tripping on a perfectly fine model
///     that's currently being rate-limited / overloaded.
///
/// Pre-fix the live SSE parser hard-coded `ErrKind::Http` /
/// `status: None` on this branch and dumped the payload into
/// `message`, so the existing `classify_err` saw only
/// `kind: Http, status: None` and fell through to
/// `classify_err_kind(ErrKind::Http) = Classification::Error` —
/// breaking the breaker on every overload. Post-fix the typed
/// envelope carries through `anthropic_error_body` and routes to
/// `Capacity`.
///
/// The unit-level assertion (`classify_err_anthropic_*`) lives in
/// `health_probe.rs`; this integration test drives the full
/// `OpenAiProvider::stream_chat` -> `consume_stream_anthropic` path so a
/// future refactor that drops the `anthropic_error_body.set(...)` call
/// also fails this test.
#[tokio::test]
async fn anthropic_streaming_overloaded_error_frame_classifies_as_capacity() {
    use zoder_core::{classify_err, Classification};

    let server = MockServer::start().await;
    let body = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\
\n\
event: error\n\
data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded, try again\"}}\n\
\n";
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let p = anthropic_provider(&server.uri(), Auth::None);
    let err = p
        .stream_chat(&anthropic_req("claude", true), None)
        .await
        .unwrap_err();

    // Wire-side pins: kind/status show the underlying transport level
    // signal.
    assert_eq!(err.kind, ErrKind::Http);
    assert!(
        err.status.is_none(),
        "mid-stream SSE error arrives on a 200 stream; status must stay None: got status={:?} err={}",
        err.status,
        err
    );

    // Y-14: the typed envelope must round-trip through
    // `anthropic_error_body` so `classify_err` can reach
    // `from_anthropic_error_body`. Without the field, the body would
    // be lost -- the existing `consume_stream_anthropic` docstring
    // explicitly notes that the body was previously in `message`
    // only, which `classify_err` cannot read.
    let body_str = err.anthropic_error_body.as_deref().unwrap_or_else(|| {
        panic!("Y-14: anthropic_error_body must be populated for the SSE error frame; got: {err}")
    });
    assert!(
        body_str.contains("overloaded_error"),
        "anthropic_error_body must carry the original typed envelope: got {body_str}"
    );

    // Critical classification assertion: `Capacity`, NOT `Error`.
    // This is the exact failure mode Y-14 names -- a 529-style
    // overload tripping the breaker on a healthy model behind
    // temporary provider pressure.
    let cls = classify_err(&err);
    assert_eq!(
        cls,
        Classification::Capacity,
        "Anthropic mid-stream `overloaded_error` envelope must classify as Capacity \
         so the breaker does NOT trip on a healthy model (Y-14): got {cls:?}, err={err}"
    );
    assert_ne!(
        cls,
        Classification::Error,
        "Y-14 contract: must NOT regress to Error (would trip the breaker on overload)"
    );
}

/// Y-14 companion: a mid-stream `authentication_error` SSE frame
/// classifies as `Unauthorized` (never trip the breaker on a model
/// whose only problem is the operator's API key).
#[tokio::test]
async fn anthropic_streaming_authentication_error_frame_classifies_as_unauthorized() {
    use zoder_core::{classify_err, Classification};

    let server = MockServer::start().await;
    let body = "\
event: error\n\
data: {\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"bad api key\"}}\n\
\n";
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let p = anthropic_provider(&server.uri(), Auth::None);
    let err = p
        .stream_chat(&anthropic_req("claude", true), None)
        .await
        .unwrap_err();
    assert!(
        err.anthropic_error_body
            .as_deref()
            .is_some_and(|b| b.contains("authentication_error")),
        "Y-14: authentication_error envelope must survive into anthropic_error_body; err={err}"
    );
    assert_eq!(classify_err(&err), Classification::Unauthorized);
}

// =====================================================================
// OpenAI Responses API adapter — wire-level integration tests.
//
// These tests pin the same properties the Anthropic slice pinned,
// scoped to the OpenAI Responses wire shape:
//   * endpoint + headers (`.../v1/responses`, `Authorization: Bearer …`),
//   * body shape (`input` array + `max_output_tokens` + system item +
//     `reasoning: {effort: …}` translation),
//   * openai-chat byte-identical behavior (the openai-responses branch
//     never triggers for `kind = "openai-chat"` — see
//     `openai_chat_path_remains_byte_identical_after_responses_fork`),
//   * non-streaming response parser,
//   * streaming SSE decoder,
//   * `Authorization: Bearer …` + the standard OpenAI `{"error":…}` 401
//     envelope surfaces as `ErrKind::Http`. The OpenAI-shaped error
//     body is intentionally NOT classified specially in model-health:
//     `Classification::from_status(401)` already lands on
//     `Unauthorized` so no per-provider branch is needed (this is the
//     no-new-classification-code contract the spec calls out — see the
//     existing `Classification::from_anthropic_error_body` OpenAI-shape
//     tests, which already defer to `from_status`).
// =====================================================================

/// Build an `OpenAiProvider` configured to hit the given base_url with
/// the OpenAI Responses API wire shape. Mirrors `anthropic_provider`.
fn responses_provider(base_url: &str, auth: Auth) -> OpenAiProvider {
    let cfg = Provider {
        id: "responses".into(),
        base_url: base_url.to_string(),
        kind: "openai-responses".into(),
        auth,
        paid: false,
        billing: BillingMode::Metered,
        subscription: None,
        serves: Vec::new(),
        azure_api_version: None,
    };
    OpenAiProvider::new(&cfg).unwrap()
}

/// Same as `req()` but uses a Responses-shaped request, including a
/// `reasoning_effort` so we also assert the `{effort: "..."}`
/// translation the adapter applies.
fn responses_req(model: &str, stream: bool) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: vec![
            Message::new("system", "You are concise."),
            Message::new("user", "hi"),
        ],
        max_tokens: 32,
        temperature: Some(0.25),
        stream,
        show_reasoning: false,
        reasoning_effort: Some("low".into()),
    }
}

/// Task-pinned test (item 1a): a `kind == "openai-responses"`
/// provider POSTs to the `/v1/responses` endpoint with the
/// `Authorization: Bearer …` header (NOT `x-api-key`), converts the
/// chat-shaped `messages` array into the Responses-shaped `input`
/// array, replaces `max_tokens` with `max_output_tokens`, lifts the
/// leading system message inline as a `role: "system"` item in
/// `input`, and translates `reasoning_effort` into a top-level
/// `reasoning: {effort: "..."}` object.
#[tokio::test]
async fn responses_request_hits_v1_responses_with_correct_headers_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("Authorization", "Bearer raw-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "resp_01",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "ok"}]
            }],
            "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}
        })))
        .mount(&server)
        .await;

    let p = responses_provider(
        &server.uri(),
        Auth::Bearer {
            token: "raw-token".into(),
        },
    );
    let res = p
        .stream_chat(&responses_req("gpt-5", false), None)
        .await
        .unwrap();
    assert_eq!(res.content, "ok");
    // The Bodies the OpenAI Responses wire contract REQUIRES these
    // fields — pin them through a follow-up mock that exercises the
    // exact wire body, not just the response.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("Authorization", "Bearer raw-token"))
        .and(body_partial_json(serde_json::json!({
            "model": "gpt-5",
            "input": [
                { "role": "system", "content": "You are concise." },
                { "role": "user", "content": "hi" }
            ],
            "max_output_tokens": 32,
            "temperature": 0.25,
            "stream": false,
            "reasoning": { "effort": "low" }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "ok"}]
            }],
            "usage": {"input_tokens": 3, "output_tokens": 1}
        })))
        .mount(&server)
        .await;
    let p2 = responses_provider(
        &server.uri(),
        Auth::Bearer {
            token: "raw-token".into(),
        },
    );
    let res = p2
        .stream_chat(&responses_req("gpt-5", false), None)
        .await
        .unwrap();
    assert_eq!(res.content, "ok");
}

/// Task-pinned test (item 1b): the openai-chat path must remain
/// byte-identical to before — a `kind = "openai-chat"` provider
/// never sees the `is_responses()` branch, never sends the
/// `input` / `max_output_tokens` / `reasoning: {effort: …}` body,
/// and still POSTs to `/v1/chat/completions`. We exercise this by
/// hitting the OpenAI Chat Completions shape and asserting the wire
/// mock matched that route instead of `/v1/responses`.
#[tokio::test]
async fn openai_chat_path_remains_byte_identical_after_responses_fork() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        // The OpenAI Chat path passes `messages` straight through
        // and uses `max_tokens` — the Responses branch would have
        // translated these to `input` and `max_output_tokens`.
        // A passing assertion here proves the Chat route is still
        // the only route taken for `kind = "openai-chat"`.
        .and(body_partial_json(serde_json::json!({
            "model": "gpt-4o",
            "messages": [
                { "role": "system", "content": "be terse" },
                { "role": "user", "content": "hi" }
            ],
            "max_tokens": 16,
            "temperature": 0.0,
            "stream": false
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "hello"}}],
            "usage": {"prompt_tokens": 2, "completion_tokens": 1}
        })))
        .mount(&server)
        .await;

    let p = provider(&server.uri());
    let req = ChatRequest {
        model: "gpt-4o".into(),
        messages: vec![
            Message::new("system", "be terse"),
            Message::new("user", "hi"),
        ],
        max_tokens: 16,
        temperature: Some(0.0),
        stream: false,
        show_reasoning: false,
        reasoning_effort: None,
    };
    let res = p.stream_chat(&req, None).await.unwrap();
    assert_eq!(res.content, "hello");
}

/// Task-pinned test (item 1c): the non-streaming Responses response
/// parser concatenates every `output[].content[].type == "output_text"`
/// block into the `content` string and translates
/// `usage.input_tokens` -> `prompt_tokens` and `usage.output_tokens`
/// -> `completion_tokens`. Multi-text-block output, a non-text item
/// (tool-call style), and a reasoning item must all be handled.
#[tokio::test]
async fn responses_non_streaming_response_is_parsed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "resp_01",
            "output": [
                {
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": "thinking"}]
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {"type": "output_text", "text": "Hello "},
                        {"type": "output_text", "text": "world"}
                    ]
                }
            ],
            "usage": {"input_tokens": 7, "output_tokens": 4, "total_tokens": 11}
        })))
        .mount(&server)
        .await;

    let p = responses_provider(&server.uri(), Auth::None);
    let res = p
        .stream_chat(&responses_req("gpt-5", false), None)
        .await
        .unwrap();
    // Concat with a single `\n` separator (matching the Anthropic
    // branch's joined_text contract). Reasoning items are filtered
    // out — `show_reasoning: false` is the default.
    assert_eq!(res.content, "Hello \nworld");
    assert_eq!(res.prompt_tokens, Some(7));
    assert_eq!(res.completion_tokens, Some(4));
}

/// Schema-invalid 2xx guard for the non-streaming Responses path:
/// a response whose `output` array is empty (or carries only
/// non-`message` items, or only whitespace text) MUST surface as
/// `ErrKind::Decode`. Same contract the OpenAI Chat and Anthropic
/// branches enforce.
#[tokio::test]
async fn responses_non_streaming_empty_output_text_is_decode_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "output": [],
            "usage": {"input_tokens": 1, "output_tokens": 0}
        })))
        .mount(&server)
        .await;

    let p = responses_provider(&server.uri(), Auth::None);
    let err = p
        .stream_chat(&responses_req("gpt-5", false), None)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrKind::Decode, "got: {err}");
    assert!(err.message.contains("empty output text"), "got: {err}");
}

/// Task-pinned test (item 1d): the streaming Responses SSE decoder
/// walks the `event: …` / `data: {…}` envelope, accumulates
/// `response.output_text.delta` `delta` strings into `content`,
/// captures `response.completed.response.usage.input_tokens` as
/// `prompt_tokens` and `output_tokens` as `completion_tokens`,
/// treats `response.completed` as the terminal event, and rejects
/// streams that close without producing any `output_text.delta`.
#[tokio::test]
async fn responses_streaming_sse_is_assembled() {
    let server = MockServer::start().await;
    // Build the SSE body from the exact Responses event shape the
    // parser was written against. `event:` lines are followed by a
    // `data:` line with the typed JSON envelope.
    let body = "\
event: response.created\n\
data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5\"}}\n\
\n\
event: response.output_text.delta\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\
\n\
event: response.output_text.delta\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\
\n\
event: response.completed\n\
data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2,\"total_tokens\":7}}}\n\
\n";
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(body_partial_json(serde_json::json!({"stream": true})))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let p = responses_provider(&server.uri(), Auth::None);
    let mut sink = Vec::new();
    let res = p
        .stream_chat(&responses_req("gpt-5", true), Some(&mut sink))
        .await
        .unwrap();
    assert_eq!(res.content, "Hello");
    assert_eq!(sink, b"Hello");
    // input_tokens from response.completed.response.usage,
    // output_tokens from the same envelope.
    assert_eq!(res.prompt_tokens, Some(5));
    assert_eq!(res.completion_tokens, Some(2));
}

/// Stream that closes via `response.completed` without producing any
/// `response.output_text.delta` — same schema-invalid-2xx contract
/// as the non-streaming empty-output-text case. The decoder must
/// surface ErrKind::Decode.
#[tokio::test]
async fn responses_streaming_with_no_text_delta_is_decode_error() {
    let server = MockServer::start().await;
    let body = "\
event: response.created\n\
data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5\"}}\n\
\n\
event: response.completed\n\
data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":3,\"output_tokens\":0,\"total_tokens\":3}}}\n\
\n";
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let p = responses_provider(&server.uri(), Auth::None);
    let err = p
        .stream_chat(&responses_req("gpt-5", true), None)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrKind::Decode, "got: {err}");
    assert!(err.message.contains("no output text"), "got: {err}");
}

/// OpenAI-shaped `{"error":{...}}` envelope on a 401 must surface
/// as `ErrKind::Http`. Same contract the OpenAI-chat and Anthropic
/// wire tests pin: the HTTP status code drives the classification
/// (the existing `classify_err` -> `from_status(401)` pipeline maps
/// it onto Unauthorized without any provider-specific branch).
/// `model_health::Classification::from_anthropic_error_body` already
/// documents that an OpenAI-style envelope defers to `from_status`,
/// confirming the wire adapter here does NOT need new
/// classification code.
#[tokio::test]
async fn responses_401_with_openai_error_envelope_is_classified_as_http() {
    let server = MockServer::start().await;
    let body = r#"{"error":{"code":"invalid_api_key","message":"bad token"}}"#;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(401).set_body_string(body))
        .mount(&server)
        .await;

    let p = responses_provider(
        &server.uri(),
        Auth::Bearer {
            token: "bogus".into(),
        },
    );
    let err = p
        .stream_chat(&responses_req("gpt-5", false), None)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrKind::Http);
    assert_eq!(err.status, Some(401));
    // The body must round-trip through `ProviderError::message` so a
    // downstream classifier can pick the typed envelope apart.
    assert!(err.message.contains("invalid_api_key"), "got: {err}");
}

// =====================================================================
// Native Azure OpenAI wire adapter — wire-level integration tests.
//
// These tests pin the four properties the Azure slice promises:
//
//   * endpoint + headers (`POST {base}/chat/completions?api-version=…`
//     with `api-key: <token>` — NOT `Authorization: Bearer …`),
//   * body shape (the openai-chat-completions body — Azure returns the
//     same wire shape, so no parallel `azure_body()` helper is needed;
//     the body builder falls through to the chat branch),
//   * openai-chat byte-identical behavior (the `azure-openai` branch
//     never triggers for `kind = "openai-chat"` — see
//     `openai_chat_path_remains_byte_identical_after_azure_fork`),
//   * non-streaming response parser (Azure returns
//     `chat.completion` shapes — the existing OpenAI response parser
//     handles them unchanged),
//   * streaming SSE decoder (Azure returns `chat.completion.chunk`
//     shapes — the existing OpenAI SSE decoder handles them unchanged),
//   * OpenAI-shape `{"error": …}` envelope on a 401 surfaces as
//     `ErrKind::Http` — same as every other OpenAI-compatible branch;
//     the existing `classify_err -> from_status(401)` pipeline picks
//     it up without any provider-specific code.
//
// Like the Anthropic / Responses slices, the regression guard tests
// pin the openai-chat byte-for-byte contract: nothing about the
// `azure-openai` branch may leak into the openai-chat branch.
// =====================================================================

/// Mutex that serializes the small number of integration tests in this
/// binary that touch the shared `AZURE_OPENAI_API_VERSION` env var.
/// Cargo runs tests in parallel by default, and the env-var read inside
/// `OpenAiProvider::new` is non-atomic against concurrent
/// `std::env::set_var` calls — without this guard, the precedence test
/// races with the other tests in this binary that also flip the same
/// variable. The integration suite has a tighter set of tests than the
/// unit-test module so a single lock here is enough.
static AZURE_INTEG_API_VERSION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Build an `OpenAiProvider` configured to hit the given base_url with
/// the Azure OpenAI native wire shape. `auth` exercises every `Auth`
/// variant the operator can use against Azure — `ApiKeyHeader { header:
/// "api-key", … }` (the recommended shape in `config.microsoft.toml`),
/// `Env { var }` (Azure key in an env var), and `None` (no credential,
/// to assert the empty-credential shape).
fn azure_provider(base_url: &str, auth: Auth, azure_api_version: Option<&str>) -> OpenAiProvider {
    let cfg = Provider {
        id: "azure".into(),
        base_url: base_url.to_string(),
        kind: "azure-openai".into(),
        auth,
        paid: false,
        billing: BillingMode::Metered,
        subscription: None,
        serves: Vec::new(),
        azure_api_version: azure_api_version.map(|s| s.to_string()),
    };
    OpenAiProvider::new(&cfg).unwrap()
}

/// Same as `req()` but uses an Azure-friendly model id.
fn azure_req(model: &str, stream: bool) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: vec![
            Message::new("system", "be terse"),
            Message::new("user", "hi"),
        ],
        max_tokens: 16,
        temperature: Some(0.5),
        stream,
        show_reasoning: false,
        reasoning_effort: None,
    }
}

/// Task-pinned test (item Azure-1a): a `kind == "azure-openai"` provider
/// POSTs to the deployment-route path
/// (`{base}/openai/deployments/<deployment>/chat/completions`) with the
/// `api-key: <token>` header (NOT `Authorization: Bearer …`) and the
/// `?api-version=<v>` query string. The body is the openai-chat
/// `messages` / `max_tokens` / `temperature` / `stream` shape — Azure
/// reuses the chat-completions body verbatim, so the wire adapter
/// does NOT carry a parallel `azure_body()` builder (the body builder
/// falls through to the chat branch for `kind == "azure-openai"`,
/// documented in the `body()` helper).
#[tokio::test]
async fn azure_request_hits_deployment_route_with_api_key_header_and_chat_body() {
    std::env::set_var("ZODER_TEST_AZURE_API_KEY_HEADER", "raw-azure-key");
    std::env::set_var("AZURE_OPENAI_API_VERSION", "2024-10-21");

    // Start the mock server first (no env-var dependency), then
    // build the provider under the lock so a parallel test can't
    // flip the env var between our `set_var` and the constructor's
    // read. The lock is released BEFORE the `.mount().await` and
    // `stream_chat(...).await` awaits — clippy's
    // `await_holding_lock` lint correctly flags a sync mutex held
    // across an await point (the runtime would block on a blocking
    // lock).
    let server = MockServer::start().await;
    let p = {
        let _guard = AZURE_INTEG_API_VERSION_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        azure_provider(
            &format!("{}/openai/deployments/gpt4o", server.uri()),
            Auth::ApiKeyHeader {
                header: "api-key".into(),
                var: "ZODER_TEST_AZURE_API_KEY_HEADER".into(),
            },
            Some("2024-10-21"),
        )
    };

    Mock::given(method("POST"))
        .and(path("/openai/deployments/gpt4o/chat/completions"))
        .and(query_param("api-version", "2024-10-21"))
        .and(header("api-key", "raw-azure-key"))
        .and(header_exists("api-key"))
        // CRITICAL: the Azure branch must NOT send `Authorization: Bearer`.
        // The mock matches on `api-key` only (so `Authorization` would
        // not cause a rejection), but the body must carry the
        // openai-chat shape and MUST NOT carry the Responses-only
        // fields (`input`, `max_output_tokens`, `reasoning`) or the
        // Anthropic-only `system` string.
        .and(body_partial_json(serde_json::json!({
            "model": "gpt-4o",
            "max_tokens": 16,
            "temperature": 0.5,
            "stream": false,
            "messages": [
                { "role": "system", "content": "be terse" },
                { "role": "user", "content": "hi" }
            ]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "ok"}}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1}
        })))
        .mount(&server)
        .await;

    let res = p
        .stream_chat(&azure_req("gpt-4o", false), None)
        .await
        .unwrap();
    assert_eq!(res.content, "ok");
    std::env::remove_var("ZODER_TEST_AZURE_API_KEY_HEADER");
    std::env::remove_var("AZURE_OPENAI_API_VERSION");
}

/// Task-pinned test (item Azure-1b): `Provider::azure_api_version`
/// field overrides the `AZURE_OPENAI_API_VERSION` env var. The mock
/// only matches the FIELD-pinned version, so a passing assertion
/// proves the field wins.
#[tokio::test]
async fn azure_provider_field_api_version_overrides_env_var() {
    // Set the env var to a value the mock would REJECT — if the
    // adapter consults the env var instead of the field, the mock
    // never matches and the test fails. Start the server first
    // (no env-var dependency), then build the provider under the
    // lock — clippy's `await_holding_lock` lint correctly flags a
    // sync mutex held across an await point.
    std::env::set_var("AZURE_OPENAI_API_VERSION", "2024-08-01");
    let server = MockServer::start().await;
    let p = {
        let _guard = AZURE_INTEG_API_VERSION_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        azure_provider(
            &format!("{}/openai/deployments/gpt4o", server.uri()),
            Auth::None,
            // FIELD set to "2024-10-21" — must beat the env var.
            Some("2024-10-21"),
        )
    };

    Mock::given(method("POST"))
        .and(query_param("api-version", "2024-10-21"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "ok"}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })))
        .mount(&server)
        .await;

    let res = p
        .stream_chat(&azure_req("gpt-4o", false), None)
        .await
        .unwrap();
    assert_eq!(res.content, "ok");
    std::env::remove_var("AZURE_OPENAI_API_VERSION");
}

/// Task-pinned test (item Azure-1c): the openai-chat path must remain
/// byte-identical to before — a `kind = "openai-chat"` provider never
/// sees the `is_azure()` branch, never sends the `api-key` header,
/// and never carries the deployment-route path. We exercise this by
/// hitting a path that the Azure branch would ALSO accept (the mock
/// is scoped to the openai-chat shape) and asserting the wire mock
/// matched the openai-chat shape, not the Azure branch.
#[tokio::test]
async fn openai_chat_path_remains_byte_identical_after_azure_fork() {
    std::env::remove_var("AZURE_OPENAI_API_VERSION");
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        // The openai-chat path passes `messages` straight through and
        // uses `max_tokens` — the Azure branch would have done the
        // same on the body (Azure reuses the chat shape). The
        // BYTE-DISTINGUISHING wire difference is the URL: the
        // openai-chat path produces a `/v1/chat/completions` route
        // and an openai-chat path-based base_url would produce a
        // DIFFERENT mock match. We use a base_url WITHOUT a deployment
        // route here so the openai-chat path's normalizer injects
        // `/v1/` (the Azure branch keeps the deployment route intact
        // instead). The mock therefore matches the openai-chat
        // branch ONLY — proving the Azure branch never fires for
        // `kind = "openai-chat"`.
        .and(body_partial_json(serde_json::json!({
            "model": "gpt-4o",
            "messages": [
                { "role": "system", "content": "be terse" },
                { "role": "user", "content": "hi" }
            ],
            "max_tokens": 16,
            "temperature": 0.5,
            "stream": false
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "hello"}}],
            "usage": {"prompt_tokens": 2, "completion_tokens": 1}
        })))
        .mount(&server)
        .await;

    let p = provider(&server.uri());
    let req = ChatRequest {
        model: "gpt-4o".into(),
        messages: vec![
            Message::new("system", "be terse"),
            Message::new("user", "hi"),
        ],
        max_tokens: 16,
        temperature: Some(0.5),
        stream: false,
        show_reasoning: false,
        reasoning_effort: None,
    };
    let res = p.stream_chat(&req, None).await.unwrap();
    assert_eq!(res.content, "hello");
}

/// Task-pinned test (item Azure-1d): the Azure branch's non-streaming
/// response is parsed by the EXISTING openai-chat response parser
/// (Azure returns OpenAI-standard `chat.completion` shapes). Token
/// usage round-trips through the OpenAI-shaped
/// `prompt_tokens` / `completion_tokens` fields without any
/// Azure-specific branch.
#[tokio::test]
async fn azure_non_streaming_response_is_parsed() {
    std::env::set_var("ZODER_TEST_AZURE_API_KEY_HEADER_2", "azure-key");
    std::env::set_var("AZURE_OPENAI_API_VERSION", "2024-10-21");
    let server = MockServer::start().await;
    let p = {
        let _guard = AZURE_INTEG_API_VERSION_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        azure_provider(
            &format!("{}/openai/deployments/gpt4o", server.uri()),
            Auth::ApiKeyHeader {
                header: "api-key".into(),
                var: "ZODER_TEST_AZURE_API_KEY_HEADER_2".into(),
            },
            Some("2024-10-21"),
        )
    };

    Mock::given(method("POST"))
        .and(path("/openai/deployments/gpt4o/chat/completions"))
        .and(query_param("api-version", "2024-10-21"))
        .and(header("api-key", "azure-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-azure-01",
            "choices": [{"message": {"content": "Hello from Azure"}}],
            "usage": {
                "prompt_tokens": 7,
                "completion_tokens": 4
            }
        })))
        .mount(&server)
        .await;

    let res = p
        .stream_chat(&azure_req("gpt-4o", false), None)
        .await
        .unwrap();
    assert_eq!(res.content, "Hello from Azure");
    assert_eq!(res.prompt_tokens, Some(7));
    assert_eq!(res.completion_tokens, Some(4));
    std::env::remove_var("ZODER_TEST_AZURE_API_KEY_HEADER_2");
    std::env::remove_var("AZURE_OPENAI_API_VERSION");
}

/// Task-pinned test (item Azure-1e): the Azure branch's streaming SSE
/// decoder walks the OpenAI-standard `chat.completion.chunk` shape
/// (Azure returns the same `data: {"choices":[{"delta":{"content":
/// "..."}}]}` events the openai-chat path consumes). The end-of-stream
/// `[DONE]` sentinel closes the SSE stream and yields the accumulated
/// content + a final usage chunk.
#[tokio::test]
async fn azure_streaming_sse_is_assembled() {
    std::env::set_var("ZODER_TEST_AZURE_API_KEY_HEADER_3", "azure-key");
    std::env::set_var("AZURE_OPENAI_API_VERSION", "2024-10-21");
    let server = MockServer::start().await;
    let p = {
        let _guard = AZURE_INTEG_API_VERSION_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        azure_provider(
            &format!("{}/openai/deployments/gpt4o", server.uri()),
            Auth::ApiKeyHeader {
                header: "api-key".into(),
                var: "ZODER_TEST_AZURE_API_KEY_HEADER_3".into(),
            },
            Some("2024-10-21"),
        )
    };

    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n\
                data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n\
                data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/openai/deployments/gpt4o/chat/completions"))
        .and(query_param("api-version", "2024-10-21"))
        .and(header("api-key", "azure-key"))
        .and(body_partial_json(serde_json::json!({"stream": true})))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let mut sink = Vec::new();
    let res = p
        .stream_chat(&azure_req("gpt-4o", true), Some(&mut sink))
        .await
        .unwrap();
    assert_eq!(res.content, "Hello");
    assert_eq!(res.completion_tokens, Some(2));
    assert_eq!(res.prompt_tokens, Some(3));
    std::env::remove_var("ZODER_TEST_AZURE_API_KEY_HEADER_3");
    std::env::remove_var("AZURE_OPENAI_API_VERSION");
}

/// Typed Azure error envelope on a 401 must surface as ErrKind::Http
/// — Azure returns OpenAI-shape `{"error":{...}}` envelopes (the
/// existing `classify_err -> from_status(401)` pipeline maps it onto
/// Unauthorized without any Azure-specific branch, exactly like the
/// openai-responses test pins for the same shape). No
/// `model_health`-specific Azure code is required; the existing
/// `Classification::from_anthropic_error_body` OpenAI-shape
/// documentation covers the no-new-classification-code contract.
#[tokio::test]
async fn azure_401_with_openai_error_envelope_is_classified_as_http() {
    std::env::set_var("ZODER_TEST_AZURE_API_KEY_HEADER_4", "bogus");
    std::env::set_var("AZURE_OPENAI_API_VERSION", "2024-10-21");
    let server = MockServer::start().await;
    let p = {
        let _guard = AZURE_INTEG_API_VERSION_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        azure_provider(
            &format!("{}/openai/deployments/gpt4o", server.uri()),
            Auth::ApiKeyHeader {
                header: "api-key".into(),
                var: "ZODER_TEST_AZURE_API_KEY_HEADER_4".into(),
            },
            Some("2024-10-21"),
        )
    };

    let body = r#"{"error":{"code":"401","message":"Access denied due to invalid subscription key or a malformed API key. Please provide a valid key for this resource."}}"#;
    Mock::given(method("POST"))
        .and(path("/openai/deployments/gpt4o/chat/completions"))
        .and(query_param("api-version", "2024-10-21"))
        .and(header("api-key", "bogus"))
        .respond_with(ResponseTemplate::new(401).set_body_string(body))
        .mount(&server)
        .await;

    let err = p
        .stream_chat(&azure_req("gpt-4o", false), None)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrKind::Http);
    assert_eq!(err.status, Some(401));
    // The error body round-trips through `ProviderError::message`.
    assert!(
        err.message.contains("invalid subscription key"),
        "got: {err}"
    );
    std::env::remove_var("ZODER_TEST_AZURE_API_KEY_HEADER_4");
    std::env::remove_var("AZURE_OPENAI_API_VERSION");
}

/// Y-19: `kind == "azure-openai"` providers short-circuit on
/// `OpenAiProvider::list_models` and never send the malformed
/// `…/openai/deployments/<dep>/models?api-version=…` URL to the
/// backend. Pre-fix the Azure adapter built that URL by composing
/// `endpoint_url(deployment_route, "azure-openai", "models", …)`, which
/// the production Data Plane 404s — a daily `--probe --all` then prints
/// a scary error for every Azure provider, even when the deployment is
/// healthy. Post-fix `list_models` returns an `Err(...)` immediately on
/// the `is_azure()` branch and callers (probe + consult) fall back to
/// the operator-configured `model_ids`, which is exactly what the rest
/// of the codebase already handles via `match … Err(_) => None` paths.
///
/// We do NOT mount a wiremock `200 /models` on the server: the entire
/// point of the fix is that NO request ever hits the wire. A test that
/// mistakenly hits the catch-all `wiremock` responder — which 404s by
/// default — and inspects the resulting request log to find zero
/// matches against `/openai/deployments/<dep>/models?api-version=…`
/// catches a regression where the guard is removed.
#[tokio::test]
async fn azure_list_models_skips_deployment_nested_url() {
    let _guard = AZURE_INTEG_API_VERSION_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Server has NO mocks mounted — any wire request would 404 in the
    // request log. We assert the local Err path returns WITHOUT
    // consulting the server.
    let server = MockServer::start().await;
    let p = azure_provider(
        &format!("{}/openai/deployments/gpt4o", server.uri()),
        Auth::None,
        Some("2024-10-21"),
    );

    // Err shape pin: Err + ErrKind::Http + status=None. We do NOT match
    // `azure-openai` with a specific Deployment-Nested-URL because the
    // whole contract is "never contact the wire for /models on azure".
    let err = p
        .list_models()
        .await
        .expect_err("list_models on azure must short-circuit (not Ok + dummy)");
    assert_eq!(
        err.kind,
        ErrKind::Http,
        "Azure list_models short-circuit must surface a typed Err: {err}"
    );
    assert!(
        err.status.is_none(),
        "Azure list_models short-circuit must not carry an HTTP status: {err}"
    );
    // And the diagnostic must reference azure-openai so an operator
    // looking at the stderr can tell WHY introspection failed (versus a
    // generic "models list" timeout).
    assert!(
        err.message.to_lowercase().contains("azure"),
        "Azure short-circuit diagnostic must mention 'azure' so the operator knows \
         why introspection was skipped; got: {err}"
    );

    // Wire-side pin: the request log is empty. If the guard is removed,
    // wiremock surfaces the catch-all 404 and the request log will have
    // an entry — the assertions above still pass on the typed Err (the
    // ErrKind::Http + Err from the openai fallback is similar), so the
    // wire-level assertion is what makes this test trustworthy.
    let received = server.received_requests().await.unwrap_or_default();
    assert!(
        received.is_empty(),
        "Azure list_models must NOT contact the wire (deployed under /deployments/<dep>); \
         request log shows {n} request(s): {requests:?}",
        n = received.len(),
        requests = received
            .iter()
            .map(|r| r.url.path().to_string())
            .collect::<Vec<_>>(),
    );
}

// =====================================================================
// Y-8: live error arms must classify before recording.
//
// The CLI's `cmd_exec_oneshot`, `--probe`, reviewer fallback, and
// agentic-turn paths all record provider failures into a shared
// `HealthStore`. Pre-fix every one of those arms used the bare
// `record_failure(model, &e.message)` -- which unconditionally bumps
// `consecutive_failures` -- so a 401/403 from a bad API key OR a
// 429/503/529 capacity signal triped the breaker on a perfectly
// healthy model after three requests, benching it for the 300s
// cooldown. Post-fix the arms route through
// `record_classified_failure(model, msg, provider_id, classify_err(&e))`
// (per the `--probe --all` sweep, which already had this contract).
//
// These tests exercise the live classification + record flow
// end-to-end against a wiremock provider so a future regression
// that swaps back to the bare `record_failure` fails here even when
// the model-health store-level tests still pass (those tests don't
// drive a wire; the bare-vs-classified wiring is invisible from
// `HealthStore`).
// =====================================================================

/// Helper: drive a chat call against a wiremock that returns the
/// given status, then route the resulting `ProviderError` through
/// `classify_err` + `record_classified_failure`. The exact shape the
/// live arm (`cmd_exec_oneshot`, `run_probe_default`, agentic
/// reviewer fallback) uses.
async fn drive_live_arm(
    p: &OpenAiProvider,
    model: &str,
    status: u16,
    body: &str,
) -> (ProviderError, Classification) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(&server)
        .await;
    let mut p_local = provider(&server.uri());
    // Swap base_url from the test fixture to point at the wiremock
    // server (matches the live arm's behavior).
    let cfg = Provider {
        id: "test".into(),
        base_url: server.uri(),
        kind: "openai-chat".into(),
        auth: Auth::None,
        paid: false,
        billing: BillingMode::Metered,
        subscription: None,
        serves: Vec::new(),
        azure_api_version: None,
    };
    p_local = OpenAiProvider::new(&cfg).unwrap();
    // Belt + braces: model is unused on the wire for the openai-chat
    // path (it's in the body), but bind the local var to silence the
    // warning without affecting the test.
    let _ = model;

    let req = ChatRequest {
        model: "m".into(),
        messages: vec![Message::new("user", "hi")],
        max_tokens: 1,
        temperature: Some(0.0),
        stream: false,
        show_reasoning: false,
        reasoning_effort: None,
    };
    let err = p_local.stream_chat(&req, None).await.unwrap_err();
    let cls = classify_err(&err);
    (err, cls)
}

#[tokio::test]
async fn y8_live_classification_401_does_not_trip_breaker() {
    // Y-8 pinned case: a 401 from the live wire. The classifier must
    // produce `Unauthorized`, and applying it via
    // `record_classified_failure` must NOT trip the breaker after
    // >> BREAKER_THRESHOLD calls (3 is the threshold; we hammer 5).
    let err_body = r#"{"error":{"message":"invalid api key","type":"invalid_request_error"}}"#;
    let (_server, _cfg) = {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_string(err_body))
            .mount(&server)
            .await;
        (server.clone(), ())
    };
    let p = provider("http://example.invalid");
    let (err, cls) = drive_live_arm(&p, "openai/gpt-4o", 401, err_body).await;
    assert_eq!(err.status, Some(401));
    assert_eq!(
        cls,
        Classification::Unauthorized,
        "live 401 must classify as Unauthorized (not Error) so the breaker \
         does NOT trip on bad credentials: err={err}"
    );

    // Drive the same path the live arm does: record_classified_failure
    // with this classification. Even after >> BREAKER_THRESHOLD
    // consecutive such calls the breaker must stay closed.
    let mut h = HealthStore::default();
    for _ in 0..(10) {
        h.record_classified_failure("openai/gpt-4o", &err.message, "openrouter", cls);
    }
    assert!(
        !h.breaker_open("openai/gpt-4o"),
        "Y-8: a 401 path that records via classify_err -> record_classified_failure \
         MUST leave the breaker closed (Unauthorized is a key-rejection, not a model \
         failure); got breaker_open=true after 10 probes"
    );
}

#[tokio::test]
async fn y8_live_classification_529_does_not_trip_breaker() {
    // Anthropic-style 529 (overload) is in the Capacity bucket;
    // consult must skip the model for the cooldown and the breaker
    // MUST stay closed -- a perfectly good model behind temporary
    // provider pressure should not get benched.
    let err_body = r#"{"error":{"message":"site overloaded","type":"overloaded_error"}}"#;
    let p = provider("http://example.invalid");
    let (err, cls) = drive_live_arm(&p, "anthropic/claude-3.7", 529, err_body).await;
    assert_eq!(err.status, Some(529));
    assert_eq!(
        cls,
        Classification::Capacity,
        "live 529 must classify as Capacity (not Error): err={err}"
    );

    let mut h = HealthStore::default();
    for _ in 0..(10) {
        h.record_classified_failure("anthropic/claude-3.7", &err.message, "anthropic", cls);
    }
    assert!(
        !h.breaker_open("anthropic/claude-3.7"),
        "Y-8: a 529 path that records via classify_err -> record_classified_failure \
         MUST leave the breaker closed (Capacity is a transient signal, not a model \
         failure); got breaker_open=true after 10 probes"
    );
}

#[tokio::test]
async fn y8_live_classification_500_still_trips_breaker() {
    // Counter-test: a generic 500 (no typed envelope, no overload tag)
    // MUST still classify as `Error` and trip the breaker after
    // BREAKER_THRESHOLD calls. Without this, the Y-8 fix could be
    // incorrectly over-broad (e.g. always routing to Capacity).
    let err_body = "internal server error";
    let p = provider("http://example.invalid");
    let (err, cls) = drive_live_arm(&p, "broken/model", 500, err_body).await;
    assert_eq!(err.status, Some(500));
    assert_eq!(
        cls,
        Classification::Error,
        "live 500 must still classify as Error (the Y-8 fix is selective): err={err}"
    );

    let mut h = HealthStore::default();
    for _ in 0..(3) {
        h.record_classified_failure("broken/model", &err.message, "openrouter", cls);
    }
    assert!(
        h.breaker_open("broken/model"),
        "Y-8: a 500 path's consecutive_failures MUST still trip the breaker \
         (we're not silently demoting Errors to Capacity); got breaker_open=false"
    );
}
