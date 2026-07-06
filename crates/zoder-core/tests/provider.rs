use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zoder_core::{
    backoff_delay, Auth, BillingMode, ChatRequest, ErrKind, Message, OpenAiProvider, Provider,
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
    };
    OpenAiProvider::new(&cfg).unwrap()
}

fn req(model: &str, stream: bool) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: vec![Message::new("user", "hi")],
        max_tokens: 16,
        temperature: 0.0,
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
