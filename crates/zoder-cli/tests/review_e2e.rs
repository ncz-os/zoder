//! End-to-end guard: a held ledger lock must not hang `zoder review`.
//!
//! `zoder review` takes the ledger's exclusive lock twice before it can
//! reach the reviewer's provider: the routing snapshot reads the ledger and
//! the reservation preallocates the billable row. Both used to wait on
//! `flock(2)` with no deadline, so one process holding `ledger.jsonl.lock`
//! left `zoder review` running for ever having made ZERO provider calls —
//! a 30 s-plus hang with nothing to show for it and no provider spend to
//! blame.
//!
//! These tests run the REAL binary against a scripted fake provider:
//!
//!  - with the lock free, the review completes and the provider sees the
//!    call (the fixture itself is trustworthy);
//!  - with the lock held by a second open file description, the review
//!    exits non-zero within its budget and the provider saw 0 calls.
//!
//! The helper "process" is this test binary itself: `flock(2)` is held on
//! an open file description, not on a process, so a second open of the same
//! path from another thread is exactly what another `zoder` process does.
//! (The thread keeps its `File` alive until the test releases it.)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use fs2::FileExt;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The ledger-lock budget the held-lock case runs under. Well inside the
/// 20 s the job allows the command, and far above the ~5 ms the acquisition
/// is supposed to need, so the assertion measures the deadline and not a
/// slow machine.
const LOCK_BUDGET_SECS: &str = "1";

/// A git repository with one committed file and one uncommitted change, so
/// `zoder review` has a real diff to review.
fn seed_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).expect("create repo");
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.name", "Jason Perlow"]);
    git(&["config", "user.email", "jperlow@gmail.com"]);
    fs::write(repo.join("src.txt"), "one\n").expect("write file");
    git(&["add", "src.txt"]);
    git(&["commit", "-q", "-m", "seed"]);
    fs::write(repo.join("src.txt"), "one\ntwo\n").expect("edit file");
    repo
}

/// `$ZODER_HOME` with one free provider pointed at `base_url`, its corpus,
/// and the ledger at `home/ledger.jsonl` (whose lock file the test holds).
fn seed_home(dir: &Path, base_url: &str) {
    let corpus = serde_json::json!({
        "source": "test",
        "models": [{"id": "seed-model/one", "free": true, "routable": true}],
    });
    fs::write(
        dir.join("model_corpus.json"),
        serde_json::to_string(&corpus).expect("serialize corpus"),
    )
    .expect("write corpus");
    let config = serde_json::json!({
        "providers": [{
            "id": "seed",
            "base_url": base_url,
            "kind": "openai-chat",
            "auth": {"type": "none"},
            "billing": "free",
            "serves": ["seed-model/"],
        }],
        "default_provider": "seed",
        "strict_free": false,
        "corpus_path": dir.join("model_corpus.json"),
        "ledger_path": dir.join("ledger.jsonl"),
        "health_path": dir.join("health.json"),
        "reviewer_model": "seed-model/one",
    });
    fs::write(
        dir.join("config.json"),
        serde_json::to_string(&config).expect("serialize config"),
    )
    .expect("write config");
}

/// The `ledger.jsonl.lock` sidecar zoder-core derives from the ledger path.
fn ledger_lock_path(home: &Path) -> PathBuf {
    let mut path = home.join("ledger.jsonl").into_os_string();
    path.push(".lock");
    PathBuf::from(path)
}

/// `zoder` with an isolated `$ZODER_HOME` and no engine daemon.
fn zoder(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zoder"));
    cmd.env("ZODER_HOME", home)
        .env("ZEROCLAW_CONFIG_DIR", home)
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("MINIMAX_API_KEY")
        .env_remove("ZODER_API_KEY")
        .env("ZEROCLAW_SOCKET", home.join("no-such-daemon.sock"));
    cmd
}

/// A wiremock provider that answers every POST with an APPROVE verdict, plus
/// a scoped mock whose `received_requests` counts the calls it served.
async fn scripted_provider() -> (MockServer, wiremock::MockGuard) {
    let provider = MockServer::start().await;
    let guard = Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "mock",
            "object": "chat.completion",
            "created": 0,
            "model": "seed-model/one",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "{\"verdict\":\"approve\",\"summary\":\"No concrete defects.\",\"findings\":[],\"next_steps\":[]}",
                },
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
        })))
        .mount_as_scoped(&provider)
        .await;
    (provider, guard)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Hold `path`'s exclusive flock from a SECOND open file description (as
/// another process would) on a worker thread, until the returned sender is
/// dropped. Returns the join handle and the release sender.
fn hold_lock(path: &Path) -> (std::thread::JoinHandle<()>, std::sync::mpsc::Sender<()>) {
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let path = path.to_path_buf();
    let handle = std::thread::spawn(move || {
        fs::create_dir_all(path.parent().expect("lock path has a parent"))
            .expect("create lock dir");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .expect("open lockfile");
        file.lock_exclusive().expect("take flock(2)");
        ready_tx.send(()).expect("signal lock held");
        // Block until the test drops the sender, then release via `file`.
        let _ = release_rx.recv();
        let _ = FileExt::unlock(&file);
    });
    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the helper must take the lock");
    (handle, release_tx)
}

/// Fixture check: with the ledger lock free, the scripted provider answers
/// one call and the review succeeds. This is what the held-lock case must
/// NOT do, and it proves the fixture reaches the provider at all.
#[tokio::test]
async fn an_uncontended_ledger_lock_lets_the_review_reach_the_provider() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seed_repo(dir.path());
    let (provider, mock) = scripted_provider().await;
    seed_home(dir.path(), &provider.uri());

    let output = zoder(dir.path())
        .current_dir(&repo)
        .args(["--json", "review"])
        .output()
        .expect("spawn zoder review");

    assert_eq!(
        output.status.code(),
        Some(0),
        "an approved review must exit 0\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(
        mock.received_requests().await.len(),
        1,
        "the scripted provider must see exactly one call\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
}

/// REGRESSION: a second holder of `ledger.jsonl.lock` must make `zoder
/// review` FAIL FAST — non-zero within the budget and the 20 s ceiling —
/// having made ZERO provider calls. On the unbounded implementation the
/// command was still running when the test killed it, with 0 calls made.
#[tokio::test]
async fn a_held_ledger_lock_fails_the_review_within_the_deadline_and_makes_no_provider_call() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seed_repo(dir.path());
    let (provider, mock) = scripted_provider().await;
    seed_home(dir.path(), &provider.uri());

    let (holder, release) = hold_lock(&ledger_lock_path(dir.path()));

    let started = Instant::now();
    let mut child = zoder(dir.path())
        .current_dir(&repo)
        .env("ZODER_LEDGER_LOCK_TIMEOUT_S", LOCK_BUDGET_SECS)
        .args(["--json", "review"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn zoder review");

    let status = loop {
        if let Some(status) = child.try_wait().expect("poll zoder review") {
            break status;
        }
        if started.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            let output = child.wait_with_output().expect("collect killed output");
            drop(release);
            holder.join().expect("join holder");
            panic!(
                "zoder review was still running 20 s after start with the ledger lock held \
                 (it must give up at the {LOCK_BUDGET_SECS} s budget)\nstdout:\n{}\nstderr:\n{}",
                stdout(&output),
                stderr(&output)
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let elapsed = started.elapsed();
    // The child has exited (that is why the poll loop broke), so collection
    // cannot block. Release the holder first anyway, so the holder's lifetime
    // never straddles the assertions: the lock is meant to be held only while
    // the child is running.
    drop(release);
    holder.join().expect("join holder");
    let output = child.wait_with_output().expect("collect output");

    assert!(
        status.code().is_some_and(|code| code != 0),
        "a review that could not take the ledger lock must exit non-zero, got {status:?}\n\
         stdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "zoder review took {elapsed:?} with a {LOCK_BUDGET_SECS} s lock budget"
    );
    assert_eq!(
        mock.received_requests().await.len(),
        0,
        "no provider call may be made when the ledger lock budget expires\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    let combined = format!("{}\n{}", stdout(&output), stderr(&output));
    // The exact timeout text, not just the word "lock": any earlier failure
    // (a bad config, a missing corpus) that merely mentions the ledger would
    // otherwise satisfy this test without the bound ever firing.
    assert!(
        combined.contains(&format!(
            "timed out after {LOCK_BUDGET_SECS}s waiting for the ledger lock at"
        )),
        "the failure must be the lock deadline firing, not an earlier error:\n{combined}"
    );
    // And it must not have bailed out immediately: a lower bound with slack
    // shows acquisition was attempted and polled to the budget. The slack is
    // required — the deadline is checked only after a failed try_lock, so a
    // correct implementation can cross it a few ms early.
    let floor = Duration::from_millis(800);
    assert!(
        elapsed >= floor,
        "zoder review failed in {elapsed:?}, well before the {LOCK_BUDGET_SECS} s budget \
         could expire, so the bound may never have been exercised"
    );
}
