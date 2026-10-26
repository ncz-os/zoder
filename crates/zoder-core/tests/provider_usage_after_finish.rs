//! Usage-after-finish regression tests for the OpenAI-compatible chat
//! stream reader: after a `finish_reason` frame the reader must keep
//! reading until `data: [DONE]` so the usage-only chunk that
//! `stream_options.include_usage` sends after the finish frame is
//! captured (it used to be dropped, leaving token counts `None`).
//!
//! A bare TCP SSE server (not wiremock) writes each part in a separate
//! flushed write with a delay between them, so the finish frame and the
//! usage frame arrive in DIFFERENT network reads, as on a real
//! OpenAI-compatible provider.

use std::time::{Duration, Instant};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zoder_core::{Auth, BillingMode, ChatRequest, Message, OpenAiProvider, Provider};

fn provider(base_url: &str) -> OpenAiProvider {
    let cfg = Provider {
        id: "test".into(),
        engine_provider_ref: None,
        base_url: base_url.to_string(),
        kind: "openai-chat".into(),
        auth: Auth::None,
        paid: false,
        billing: BillingMode::Metered,
        subscription: None,
        serves: Vec::new(),
        azure_api_version: None,
        max_context_tokens: None,
        model_aliases: Vec::new(),
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
        top_p: None,
        top_k: None,
        presence_penalty: None,
        chat_template_kwargs: None,
        response_format: None,
    }
}

const SSE_CONTENT_FRAMES: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n\
     data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n";
const SSE_FINISH_FRAME: &str =
    "data: {\"choices\":[{\"delta\":{\"content\":\"\"},\"finish_reason\":\"stop\"}]}\n\n";
const SSE_USAGE_FRAME: &str = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1234,\"completion_tokens\":56,\"prompt_tokens_details\":{\"cached_tokens\":1000}}}\n\n";
const SSE_DONE: &str = "data: [DONE]\n\n";

/// A bare TCP SSE server on a localhost port.
async fn sse_server() -> (String, tokio::net::TcpListener) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = listener.local_addr().unwrap();
    (format!("http://127.0.0.1:{}", local.port()), listener)
}

/// Accept one connection, drain the request headers, then write the SSE
/// `parts` in separate flushed writes with a `gap` between each.
async fn serve_sse_delayed(listener: tokio::net::TcpListener, parts: Vec<String>, gap: Duration) {
    use tokio::io::AsyncWriteExt;
    let (mut sock, _) = listener.accept().await.unwrap();
    {
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let _ = sock.readable().await;
            let n = sock.try_read(&mut tmp).map_err(|_| ()).unwrap_or(0);
            if n > 0 {
                buf.extend_from_slice(&tmp[..n]);
            }
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
    let _ = sock.write_all(head.as_bytes()).await;
    let _ = sock.flush().await;
    for part in parts {
        let _ = sock.write_all(part.as_bytes()).await;
        let _ = sock.flush().await;
        tokio::time::sleep(gap).await;
    }
    let _ = sock.shutdown().await;
}

/// The stream completes with a `finish_reason` frame and then the
/// connection stays open (no `[DONE]`, no usage): the call must return
/// within the post-finish grace bound, successfully, with usage `None`.
async fn serve_sse_finish_then_stall(listener: tokio::net::TcpListener) {
    use tokio::io::AsyncWriteExt;
    let (mut sock, _) = listener.accept().await.unwrap();
    {
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let _ = sock.readable().await;
            let n = sock.try_read(&mut tmp).map_err(|_| ()).unwrap_or(0);
            if n > 0 {
                buf.extend_from_slice(&tmp[..n]);
            }
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
    let _ = sock.write_all(head.as_bytes()).await;
    let _ = sock.flush().await;
    let _ = sock.write_all(SSE_CONTENT_FRAMES.as_bytes()).await;
    let _ = sock.flush().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = sock.write_all(SSE_FINISH_FRAME.as_bytes()).await;
    let _ = sock.flush().await;
    // Really hold the connection open: the test aborts this task when done.
    // (A `try_read` loop would treat WouldBlock as EOF and close at once.)
    tokio::time::sleep(Duration::from_secs(60)).await;
    drop(sock);
}

#[tokio::test]
async fn streaming_sse_usage_frame_after_finish_is_captured() {
    let (base, listener) = sse_server().await;
    let p = provider(&base);
    let serve = tokio::spawn(serve_sse_delayed(
        listener,
        vec![
            SSE_CONTENT_FRAMES.to_string(),
            SSE_FINISH_FRAME.to_string(),
            SSE_USAGE_FRAME.to_string(),
            SSE_DONE.to_string(),
        ],
        Duration::from_millis(100),
    ));

    let res = p.stream_chat(&req("m", true), None).await.unwrap();
    assert_eq!(res.content, "Hello");
    assert_eq!(
        res.prompt_tokens,
        Some(1234),
        "usage frame after finish must be read"
    );
    assert_eq!(res.completion_tokens, Some(56));
    assert_eq!(res.cached_prompt_tokens, Some(1000));
    let _ = serve.await;
}

#[tokio::test]
async fn streaming_sse_usage_in_same_write_as_finish_still_captured() {
    // Regression guard for today's behaviour: a provider that bundles the
    // usage into the same write as the finish frame (or a fast server
    // coalescing both into one read) must keep working.
    let (base, listener) = sse_server().await;
    let p = provider(&base);
    let combined = format!("{}{}", SSE_FINISH_FRAME, SSE_USAGE_FRAME);
    let serve = tokio::spawn(serve_sse_delayed(
        listener,
        vec![
            SSE_CONTENT_FRAMES.to_string(),
            combined,
            SSE_DONE.to_string(),
        ],
        Duration::from_millis(50),
    ));

    let res = p.stream_chat(&req("m", true), None).await.unwrap();
    assert_eq!(res.content, "Hello");
    assert_eq!(res.prompt_tokens, Some(1234));
    assert_eq!(res.completion_tokens, Some(56));
    assert_eq!(res.cached_prompt_tokens, Some(1000));
    let _ = serve.await;
}

#[tokio::test]
async fn streaming_sse_finish_without_usage_is_success_with_none_usage() {
    // A provider that honors finish_reason but never sends usage: the
    // call succeeds and the token counts stay None.
    let (base, listener) = sse_server().await;
    let p = provider(&base);
    let serve = tokio::spawn(serve_sse_delayed(
        listener,
        vec![
            SSE_CONTENT_FRAMES.to_string(),
            SSE_FINISH_FRAME.to_string(),
            SSE_DONE.to_string(),
        ],
        Duration::from_millis(50),
    ));

    let res = p.stream_chat(&req("m", true), None).await.unwrap();
    assert_eq!(res.content, "Hello");
    assert_eq!(res.prompt_tokens, None);
    assert_eq!(res.completion_tokens, None);
    let _ = serve.await;
}

#[tokio::test]
async fn streaming_sse_finish_then_silent_keepalive_returns_within_grace() {
    // The server sends finish_reason and then goes silent (keep-alive
    // connection, no [DONE], no usage). The reader must not wait out the
    // idle budget: it returns success within the post-finish grace.
    let (base, listener) = sse_server().await;
    let p = provider(&base).with_post_finish_grace(Duration::from_millis(300));
    let serve = tokio::spawn(serve_sse_finish_then_stall(listener));

    let started = Instant::now();
    let res = p.stream_chat(&req("m", true), None).await.unwrap();
    let elapsed = started.elapsed();
    assert_eq!(res.content, "Hello");
    assert_eq!(res.prompt_tokens, None);
    assert_eq!(res.completion_tokens, None);
    assert!(
        elapsed < Duration::from_secs(3),
        "post-finish wait must be bounded by the grace, took {:?}",
        elapsed
    );
    serve.abort();
}

#[tokio::test]
async fn streaming_sse_content_after_finish_is_not_appended() {
    // A content-bearing frame after finish_reason is provider
    // misbehavior: it must be dropped (the answer keeps the pre-finish
    // text) while usage from the same frame is still captured.
    let (base, listener) = sse_server().await;
    let p = provider(&base);
    let stray = "data: {\"choices\":[{\"delta\":{\"content\":\" EXTRANEOUS\"}}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\n";
    let serve = tokio::spawn(serve_sse_delayed(
        listener,
        vec![
            SSE_CONTENT_FRAMES.to_string(),
            SSE_FINISH_FRAME.to_string(),
            stray.to_string(),
            SSE_DONE.to_string(),
        ],
        Duration::from_millis(50),
    ));

    let res = p.stream_chat(&req("m", true), None).await.unwrap();
    assert_eq!(
        res.content, "Hello",
        "content after finish_reason must not be appended"
    );
    assert_eq!(
        res.prompt_tokens,
        Some(7),
        "usage on the stray frame is still captured"
    );
    assert_eq!(res.completion_tokens, Some(3));
    let _ = serve.await;
}

#[tokio::test]
async fn streaming_sse_request_carries_include_usage_non_stream_does_not() {
    // The streamed call must request `stream_options.include_usage: true`;
    // a non-streamed call must not carry `stream_options` at all.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SSE_DONE))
        .mount(&server)
        .await;
    let p = provider(&server.uri());
    // The SSE body is just `[DONE]` — the reader will fail with a decode
    // error (no choices), but the request is already captured. We only
    // need to check the request body, so unwrap the error.
    let _ = p.stream_chat(&req("m", true), None).await;
    let captured = server.received_requests().await.unwrap();
    assert_eq!(
        captured[0].body_json::<serde_json::Value>().unwrap()["stream_options"]["include_usage"],
        serde_json::json!(true),
        "streamed calls must request include_usage"
    );

    let server2 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server2)
        .await;
    let p2 = provider(&server2.uri());
    // Non-streamed: the mock returns 200 with empty body — the call will
    // fail to parse, but the request is captured. We only check the body.
    let _ = p2.stream_chat(&req("m", false), None).await;
    let captured2 = server2.received_requests().await.unwrap();
    assert!(
        captured2[0]
            .body_json::<serde_json::Value>()
            .unwrap()
            .get("stream_options")
            .is_none(),
        "non-streamed calls must not carry stream_options"
    );
}

/// Like `serve_sse_delayed`, but after the last part the socket stays open
/// (no shutdown) until the client hangs up.
async fn serve_sse_then_hold(listener: tokio::net::TcpListener, parts: Vec<String>) {
    use tokio::io::AsyncWriteExt;
    let (mut sock, _) = listener.accept().await.unwrap();
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let _ = sock.readable().await;
        let n = sock.try_read(&mut tmp).map_err(|_| ()).unwrap_or(0);
        if n > 0 {
            buf.extend_from_slice(&tmp[..n]);
        }
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
    let _ = sock.write_all(head.as_bytes()).await;
    for part in parts {
        let _ = sock.write_all(part.as_bytes()).await;
        let _ = sock.flush().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Really hold the connection open: the test aborts this task when done.
    // (A `try_read` loop would treat WouldBlock as EOF and close at once.)
    tokio::time::sleep(Duration::from_secs(60)).await;
    drop(sock);
}

#[tokio::test]
async fn streaming_sse_done_marker_ends_the_read_without_waiting_for_close() {
    // `data: [DONE]` is the end of the stream. A server that sends it and
    // then keeps the connection open must not make the reader wait out the
    // post-finish grace (set long here) or the idle budget.
    let (base, listener) = sse_server().await;
    let p = provider(&base).with_post_finish_grace(Duration::from_secs(10));
    let parts = vec![
        SSE_CONTENT_FRAMES.to_string(),
        SSE_FINISH_FRAME.to_string(),
        SSE_USAGE_FRAME.to_string(),
        SSE_DONE.to_string(),
    ];
    let serve = tokio::spawn(serve_sse_then_hold(listener, parts));
    let started = Instant::now();
    let res = p.stream_chat(&req("m", true), None).await.unwrap();
    let elapsed = started.elapsed();
    assert_eq!(res.content, "Hello");
    assert_eq!(res.prompt_tokens, Some(1234));
    assert_eq!(res.completion_tokens, Some(56));
    assert!(
        elapsed < Duration::from_secs(3),
        "[DONE] must end the read at once, took {elapsed:?}"
    );
    serve.abort();
}
