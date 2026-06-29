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

#[test]
fn backoff_honors_retry_after_floor() {
    // With a 10s server hint, the delay must be at least 10s regardless of attempt.
    let d = backoff_delay(0, Some(Duration::from_secs(10)));
    assert!(d >= Duration::from_secs(10));
    // Without a hint, attempt 0 is sub-second-ish (<= ~1s with jitter).
    let d0 = backoff_delay(0, None);
    assert!(d0 <= Duration::from_millis(1200));
}
