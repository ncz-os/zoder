//! Optional MNEMOS work history. Credentials are read only from the environment.
use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::time::Duration;

async fn request(base: &str, token: &str, path: &str, body: Value) -> anyhow::Result<Value> {
    let url = reqwest::Url::parse(base).context("invalid MNEMOS_URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("MNEMOS_URL must be an HTTP(S) URL without embedded credentials");
    }
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .post(format!("{}{path}", base.trim_end_matches('/')))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("MNEMOS request failed"))?;
    if !response.status().is_success() {
        bail!("MNEMOS returned HTTP {}", response.status().as_u16());
    }
    response
        .json()
        .await
        .context("invalid MNEMOS JSON response")
}

pub(crate) async fn command(query: Option<&str>, record: Option<&str>) -> anyhow::Result<()> {
    let base = std::env::var("MNEMOS_URL").context("MNEMOS_URL is required")?;
    let token = std::env::var("MNEMOS_TOKEN").context("MNEMOS_TOKEN is required")?;
    let (path, body) = match (query, record) {
        (Some(q), None) => (
            "/v1/memories/search",
            json!({"query":q,"limit":5,"subcategory":"zoder-work"}),
        ),
        (None, Some(content)) => (
            "/v1/memories",
            json!({"content":content,"category":"projects","subcategory":"zoder-work","source_agent":"zoder"}),
        ),
        _ => bail!("select exactly one of --search or --record"),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&request(&base, &token, path, body).await?)?
    );
    Ok(())
}

/// Small factual checkpoint, excluding prompts, model output and environment.
/// Network failure must not discard work or prevent local recovery.
pub(crate) async fn checkpoint(cwd: &std::path::Path, evidence: Value) {
    let run = std::process::Command::new("git")
        .args(["rev-parse", "--absolute-git-dir"])
        .current_dir(cwd)
        .output();
    if let Ok(run) = run {
        if run.status.success() {
            use std::io::Write;
            let directory = String::from_utf8_lossy(&run.stdout).trim().to_owned();
            let path = std::path::Path::new(&directory).join("zoder-work-ledger.jsonl");
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                if writeln!(file, "{evidence}").is_err() {
                    eprintln!("[zoder] local MNEMOS ledger write failed");
                }
            } else {
                eprintln!("[zoder] local MNEMOS ledger could not be opened");
            }
        } else {
            eprintln!("[zoder] local MNEMOS ledger unavailable: not a Git checkout");
        }
    } else {
        eprintln!("[zoder] local MNEMOS ledger unavailable: Git failed");
    }
    let (Ok(base), Ok(token)) = (std::env::var("MNEMOS_URL"), std::env::var("MNEMOS_TOKEN")) else {
        return;
    };
    let body = json!({"content":evidence.to_string(),"category":"projects","subcategory":"zoder-work","source_agent":"zoder","metadata":{"job_id":std::env::var("HIVE_JOB_ID").ok()}});
    match request(&base, &token, "/v1/memories", body).await {
        Ok(value) => eprintln!("[zoder] MNEMOS checkpoint {}", value["id"]),
        Err(error) => eprintln!("[zoder] MNEMOS checkpoint pending: {error}"),
    }
}

static EVENT_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Body of one loop-progress event (`projects/zoder-loop-events`). One memory
/// per event, written as each phase finishes, so a loop that dies mid-way still
/// leaves a trail a doctor can read. Payloads are small factual summaries:
/// never prompts, diffs, model output or credentials.
pub(crate) fn event_body(job_id: &str, seq: u32, phase: &str, payload: Value) -> Value {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut content = payload;
    if let Value::Object(map) = &mut content {
        map.insert("phase".into(), json!(phase));
    }
    json!({
        "content": content.to_string(),
        "category": "projects",
        "subcategory": "zoder-loop-events",
        "source_agent": "zoder",
        "metadata": {"job_id": job_id, "seq": seq, "phase": phase, "ts": ts},
    })
}

/// Fail-soft progress event. Bounded wait; any error is a debug line only and
/// never changes the loop result. No-op without MNEMOS credentials or a job id.
pub(crate) async fn event(job_id: Option<&str>, phase: &str, payload: Value) {
    let Some(job_id) = job_id else { return };
    let (Ok(base), Ok(token)) = (std::env::var("MNEMOS_URL"), std::env::var("MNEMOS_TOKEN")) else {
        return;
    };
    let seq = EVENT_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let body = event_body(job_id, seq, phase, payload);
    let sent = tokio::time::timeout(
        Duration::from_secs(4),
        request(&base, &token, "/v1/memories", body),
    )
    .await;
    if !matches!(sent, Ok(Ok(_))) {
        eprintln!("[zoder] loop event {phase} not recorded");
    }
}

/// Blocking variant for `Drop` (a crashing loop cannot await). Same fail-soft
/// contract; bounded to 4 s.
pub(crate) fn event_blocking(job_id: Option<&str>, phase: &str, payload: Value) {
    let Some(job_id) = job_id else { return };
    let (Ok(base), Ok(token)) = (std::env::var("MNEMOS_URL"), std::env::var("MNEMOS_TOKEN")) else {
        return;
    };
    let Ok(url) = reqwest::Url::parse(&base) else {
        return;
    };
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return;
    }
    let seq = EVENT_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let body = event_body(job_id, seq, phase, payload);
    let sent = std::thread::spawn(move || {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(4))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .and_then(|c| {
                c.post(format!("{}/v1/memories", base.trim_end_matches('/')))
                    .bearer_auth(token)
                    .json(&body)
                    .send()
            })
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    })
    .join()
    .unwrap_or(false);
    if !sent {
        eprintln!("[zoder] loop event {phase} not recorded");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    use wiremock::{
        matchers::{header, method, path},
        Mock, MockServer, ResponseTemplate,
    };
    #[tokio::test]
    async fn authenticated_record_and_search() {
        let server = MockServer::start().await;
        for endpoint in ["/v1/memories", "/v1/memories/search"] {
            Mock::given(method("POST"))
                .and(path(endpoint))
                .and(header("authorization", "Bearer fixture"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok":true})))
                .mount(&server)
                .await;
            assert_eq!(
                request(&server.uri(), "fixture", endpoint, json!({}))
                    .await
                    .unwrap()["ok"],
                true
            );
        }
    }
    #[tokio::test]
    async fn auth_error_does_not_expose_credentials_or_response_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("sensitive body"))
            .mount(&server)
            .await;
        let error = request(&server.uri(), "secret", "/v1/memories", json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "MNEMOS returned HTTP 401");
    }
    #[tokio::test]
    async fn loop_event_is_recorded_with_job_metadata() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/memories"))
            .and(header("authorization", "Bearer fixture"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"m1"})))
            .expect(1)
            .mount(&server)
            .await;
        std::env::set_var("MNEMOS_URL", server.uri());
        std::env::set_var("MNEMOS_TOKEN", "fixture");
        event(
            Some("job-1"),
            "author_end",
            json!({"iteration":1,"status":"ok"}),
        )
        .await;
        let got = &server.received_requests().await.unwrap()[0];
        let body: Value = serde_json::from_slice(&got.body).unwrap();
        assert_eq!(body["subcategory"], "zoder-loop-events");
        assert_eq!(body["metadata"]["job_id"], "job-1");
        assert_eq!(body["metadata"]["phase"], "author_end");
        let content: Value = serde_json::from_str(body["content"].as_str().unwrap()).unwrap();
        assert_eq!(content["phase"], "author_end");
        assert_eq!(content["status"], "ok");
    }
    #[tokio::test]
    async fn loop_event_is_fail_soft_when_mnemos_is_unreachable() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("MNEMOS_URL", "http://127.0.0.1:9");
        std::env::set_var("MNEMOS_TOKEN", "fixture");
        // must return, not panic, and not hang past the bounded wait
        let t = std::time::Instant::now();
        event(Some("job-2"), "teardown", json!({"reason":"crash"})).await;
        assert!(t.elapsed() < Duration::from_secs(6));
        event(None, "teardown", json!({})).await;
    }
}
