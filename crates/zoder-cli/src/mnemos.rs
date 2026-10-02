//! Optional MNEMOS work history. Credentials are read only from the environment.
use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::path::Path;
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

#[cfg(test)]
mod tests {
    use super::*;
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
    #[test]
    fn work_ledger_doc_matches_source_behavior() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let doc_path = manifest.ancestors().nth(2).unwrap().join("docs/MNEMOS-WORK-LEDGER.md");
        let doc = std::fs::read_to_string(&doc_path)
            .unwrap_or_else(|e| panic!("work-ledger doc {} missing: {e}", doc_path.display()));
        let readme = std::fs::read_to_string(manifest.ancestors().nth(2).unwrap().join("README.md"))
            .expect("README.md readable from repo root");
        // The README must link the doc from the root.
        assert!(
            readme.contains("docs/MNEMOS-WORK-LEDGER.md"),
            "README.md must link docs/MNEMOS-WORK-LEDGER.md"
        );
        // Every user-facing fact documented here must match the source above:
        let required = [
            // env contract
            "MNEMOS_URL",
            "MNEMOS_TOKEN",
            "without embedded credentials",
            // CLI surface (mutually exclusive flags, fixed request shapes)
            "zoder mnemos --search",
            "zoder mnemos --record",
            "/v1/memories/search",
            "/v1/memories",
            "\"limit\": 5",
            "\"subcategory\": \"zoder-work\"",
            "\"source_agent\": \"zoder\"",
            // local ledger path + remote payload metadata
            "zoder-work-ledger.jsonl",
            "git rev-parse --absolute-git-dir",
            "metadata.job_id",
            // job context + free-text retrieval by parent ID
            "HIVE_JOB_ID",
            // failure messages emitted by the code
            "[zoder] MNEMOS checkpoint pending",
            // documented limits
            "not proof",
            "not currently auto-replay",
        ];
        for needle in required {
            assert!(
                doc.contains(needle),
                "docs/MNEMOS-WORK-LEDGER.md must document {needle:?} to match source behavior"
            );
        }
    }
}
