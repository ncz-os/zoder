//! Fault-injection harness for the reaper's cancel phase (ZD-20 / CF-29).
//!
//! Mounted from `reaper.rs`'s test module with `#[path]`, so it reaches the
//! private cancel-phase functions and the shared test lock.
//!
//! * [`FaultDaemon`] is an in-process stand-in for the zeroclaw daemon: a
//!   tokio Unix-socket server on the test's own runtime that speaks
//!   `initialize`, `session/new`, `session/prompt`, `session/cancel` and
//!   `session/abort`, records every frame (method, target session, arrival
//!   time on tokio's clock), and answers each cancel as its [`Script`] says.
//! * [`stepped`] runs a future under a paused clock that advances 1 ms per
//!   scheduler round. Plain `start_paused` jumps the clock to the next timer
//!   whenever the runtime is idle, including while a socket read is pending,
//!   so a timeout would fire before the in-process daemon had a chance to
//!   answer.

use super::tests::reap_test_lock;
use super::*;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// How the daemon answers one `session/cancel`.
#[derive(Debug, Clone)]
pub(super) enum OnCancel {
    /// Never answer.
    Silent,
    /// Answer "there is no turn to cancel" (an error reply the client reads
    /// as settled).
    AlreadyIdle,
    /// Send `turn_complete` for the session after the delay.
    AckAfter(Duration),
    /// Ignore the cancel; acknowledge a later `session/abort` instead.
    IgnoreCancelHonourAbort,
    /// Close the connection.
    CloseConnection,
}

/// How the daemon answers `session/prompt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OnPrompt {
    /// Reply, then send `turn_complete`.
    Complete,
    /// Reply and send one message chunk, never complete.
    Hang,
}

/// Behaviour for one session. `on_cancel[n]` answers the n-th cancel of the
/// session; the last entry repeats.
#[derive(Debug, Clone)]
pub(super) struct Script {
    pub session_id: String,
    pub on_prompt: OnPrompt,
    pub on_cancel: Vec<OnCancel>,
}

impl Script {
    pub fn cancel(session_id: &str, on_cancel: Vec<OnCancel>) -> Self {
        Self {
            session_id: session_id.to_string(),
            on_prompt: OnPrompt::Hang,
            on_cancel,
        }
    }
}

/// One frame the daemon received.
#[derive(Debug, Clone)]
pub(super) struct Frame {
    pub method: String,
    pub session_id: Option<String>,
    pub at: tokio::time::Instant,
}

#[derive(Default)]
struct State {
    frames: Vec<Frame>,
    cancels: HashMap<String, usize>,
    minted: usize,
}

pub(super) struct FaultDaemon {
    pub socket: PathBuf,
    state: Arc<std::sync::Mutex<State>>,
    task: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Drop for FaultDaemon {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn frame_line(value: serde_json::Value) -> Vec<u8> {
    let mut line = serde_json::to_vec(&value).expect("frame serializes");
    line.push(b'\n');
    line
}

fn turn_complete(session_id: &str) -> Vec<u8> {
    frame_line(serde_json::json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {"session_id": session_id, "type": "turn_complete"},
    }))
}

impl FaultDaemon {
    pub async fn start(scripts: Vec<Script>) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind fault daemon");
        let state = Arc::new(std::sync::Mutex::new(State::default()));
        let scripts = Arc::new(scripts);
        let task = {
            let state = state.clone();
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(serve(stream, state.clone(), scripts.clone()));
                }
            })
        };
        Self {
            socket,
            state,
            task,
            _dir: dir,
        }
    }

    pub fn frames(&self) -> Vec<Frame> {
        self.state.lock().unwrap().frames.clone()
    }

    /// Frames of `method` for `session`.
    pub fn count(&self, method: &str, session: &str) -> usize {
        self.frames()
            .iter()
            .filter(|f| f.method == method && f.session_id.as_deref() == Some(session))
            .count()
    }
}

async fn serve(
    stream: tokio::net::UnixStream,
    state: Arc<std::sync::Mutex<State>>,
    scripts: Arc<Vec<Script>>,
) {
    let (read, write) = stream.into_split();
    let write = Arc::new(tokio::sync::Mutex::new(write));
    let mut lines = BufReader::new(read).lines();
    let script_for = |sid: &str| scripts.iter().find(|s| s.session_id == sid).cloned();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(frame) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let method = frame["method"].as_str().unwrap_or_default().to_string();
        let id = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let sid = frame
            .pointer("/params/session_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let nth_cancel = {
            let mut st = state.lock().unwrap();
            st.frames.push(Frame {
                method: method.clone(),
                session_id: sid.clone(),
                at: tokio::time::Instant::now(),
            });
            if method == "session/cancel" {
                let n = st
                    .cancels
                    .entry(sid.clone().unwrap_or_default())
                    .or_default();
                *n += 1;
                *n - 1
            } else {
                0
            }
        };
        let reply = |result: serde_json::Value| {
            frame_line(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}))
        };
        let mut out: Vec<Vec<u8>> = Vec::new();
        match method.as_str() {
            "initialize" => out.push(reply(
                serde_json::json!({"protocol_version": 1, "agent_capabilities": {}}),
            )),
            "session/new" => {
                let minted = {
                    let mut st = state.lock().unwrap();
                    st.minted += 1;
                    st.minted
                };
                let sid = scripts
                    .get(minted - 1)
                    .map_or_else(|| format!("fault-{minted}"), |s| s.session_id.clone());
                out.push(reply(serde_json::json!({"session_id": sid})));
            }
            "session/prompt" => {
                let sid = sid.unwrap_or_default();
                let on_prompt = script_for(&sid).map_or(OnPrompt::Hang, |s| s.on_prompt);
                out.push(reply(serde_json::json!({})));
                match on_prompt {
                    OnPrompt::Complete => out.push(turn_complete(&sid)),
                    OnPrompt::Hang => out.push(frame_line(serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {"session_id": sid, "type": "agent_message_chunk",
                                   "update": {"sessionUpdate": "agent_message_chunk",
                                              "content": {"type": "text", "text": "working"}}},
                    }))),
                }
            }
            "session/cancel" => {
                let sid = sid.unwrap_or_default();
                let plan = script_for(&sid).map(|s| s.on_cancel).unwrap_or_default();
                let action = plan
                    .get(nth_cancel)
                    .or(plan.last())
                    .cloned()
                    .unwrap_or(OnCancel::Silent);
                match action {
                    OnCancel::Silent | OnCancel::IgnoreCancelHonourAbort => {}
                    OnCancel::AlreadyIdle => out.push(frame_line(serde_json::json!({
                        // `session/cancel` is a notification, so this echoes a null id;
                        // echoing keeps the reply correct for a daemon that is sent one.
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32000, "message": format!("no active turn for session {sid}")},
                    }))),
                    OnCancel::AckAfter(delay) => {
                        let write = write.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(delay).await;
                            let _ = write.lock().await.write_all(&turn_complete(&sid)).await;
                        });
                    }
                    OnCancel::CloseConnection => return,
                }
            }
            "session/abort" => {
                let sid = sid.unwrap_or_default();
                let honours = script_for(&sid).is_some_and(|s| {
                    s.on_cancel
                        .iter()
                        .any(|c| matches!(c, OnCancel::IgnoreCancelHonourAbort))
                });
                if !id.is_null() {
                    out.push(reply(serde_json::json!({})));
                }
                if honours {
                    out.push(turn_complete(&sid));
                }
            }
            _ => {
                if !id.is_null() {
                    out.push(reply(serde_json::json!({})));
                }
            }
        }
        let mut w = write.lock().await;
        for bytes in out {
            if w.write_all(&bytes).await.is_err() {
                return;
            }
        }
    }
}

/// Run `fut` with the paused clock advancing 1 ms per scheduler round. The
/// caller's runtime must be paused (`start_paused = true`).
pub(super) async fn stepped<F: std::future::Future>(fut: F) -> F::Output {
    let stepper = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_millis(1)).await;
        }
    });
    let out = fut.await;
    stepper.abort();
    out
}

/// Self-test: each scripted cancel behaviour reaches the real
/// `zoder_core::cancel_session` as intended.
#[tokio::test(start_paused = true)]
async fn harness_scripts_each_cancel_behaviour() {
    let daemon = FaultDaemon::start(vec![
        Script::cancel("silent", vec![OnCancel::Silent]),
        Script::cancel("idle", vec![OnCancel::AlreadyIdle]),
        Script::cancel("ack", vec![OnCancel::AckAfter(Duration::from_millis(50))]),
        Script::cancel("abort-only", vec![OnCancel::IgnoreCancelHonourAbort]),
        Script::cancel("close", vec![OnCancel::CloseConnection]),
        Script::cancel(
            "second-ack",
            vec![
                OnCancel::Silent,
                OnCancel::AckAfter(Duration::from_millis(10)),
            ],
        ),
        Script {
            session_id: "prompt-done".into(),
            on_prompt: OnPrompt::Complete,
            on_cancel: vec![OnCancel::Silent],
        },
    ])
    .await;
    let budget = Duration::from_millis(300);
    let cancel = |sid: &'static str| {
        let socket = daemon.socket.clone();
        async move { stepped(zoder_core::cancel_session(&socket, sid, budget)).await }
    };
    let started = tokio::time::Instant::now();
    let silent = cancel("silent").await;
    assert!(silent.is_err(), "a silent daemon never acknowledges");
    assert!(
        started.elapsed() >= budget,
        "the silent cancel waited its budget"
    );
    assert!(
        cancel("idle").await.is_ok(),
        "already idle settles the cancel"
    );
    let started = tokio::time::Instant::now();
    assert!(
        cancel("ack").await.is_ok(),
        "a late ack confirms the cancel"
    );
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(50) && waited < budget,
        "the ack arrived after its delay: {waited:?}"
    );
    assert!(cancel("abort-only").await.is_err(), "a cancel is ignored");
    let err = cancel("close").await.unwrap_err().to_string();
    assert!(err.contains("closed connection"), "{err}");
    assert!(
        cancel("second-ack").await.is_err(),
        "first cancel is dropped"
    );
    assert!(
        cancel("second-ack").await.is_ok(),
        "the second is acknowledged"
    );
    assert_eq!(daemon.count("session/cancel", "second-ack"), 2);
    assert_eq!(daemon.count("session/cancel", "silent"), 1);
    // Every connection began with initialize.
    let frames = daemon.frames();
    assert_eq!(
        frames.iter().filter(|f| f.method == "initialize").count(),
        frames
            .iter()
            .filter(|f| f.method == "session/cancel")
            .count()
    );

    // `session/abort` is honoured only where the script says so, and a
    // prompt hangs or completes as scripted (raw client, no ACP driver).
    use tokio::io::AsyncReadExt;
    let mut raw = tokio::net::UnixStream::connect(&daemon.socket)
        .await
        .unwrap();
    raw.write_all(&frame_line(serde_json::json!({"jsonrpc": "2.0", "id": 1,
        "method": "session/abort", "params": {"session_id": "abort-only"}})))
        .await
        .unwrap();
    let mut seen = String::new();
    let _ = stepped(tokio::time::timeout(Duration::from_millis(50), async {
        let mut buf = [0u8; 4096];
        while !seen.contains("turn_complete") {
            let n = raw.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    }))
    .await;
    assert!(seen.contains("turn_complete"), "abort was honoured: {seen}");
    assert_eq!(daemon.count("session/abort", "abort-only"), 1);

    let mut seen = String::new();
    raw.write_all(&frame_line(serde_json::json!({"jsonrpc": "2.0", "id": 2,
        "method": "session/prompt", "params": {"session_id": "prompt-done"}})))
        .await
        .unwrap();
    let _ = stepped(tokio::time::timeout(Duration::from_millis(50), async {
        let mut buf = [0u8; 4096];
        while !seen.contains("turn_complete") {
            let n = raw.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    }))
    .await;
    assert!(
        seen.contains("turn_complete"),
        "a Complete prompt completes: {seen}"
    );
    // Frames are recorded in arrival order on tokio's clock.
    let frames = daemon.frames();
    assert!(frames.windows(2).all(|w| w[0].at <= w[1].at));
}

/// Two running sessions; the daemon drops A's first cancel and acknowledges
/// every later cancel within 50 ms. Both sessions must be asked, and A must
/// end confirmed: its first attempt must not spend the window its retries
/// need (the starvation shape of CF-27). Real clock. Still red on master: the
/// window is per session now, but the first attempt waits all of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "pins the ZD-18 follow-up: the first cancel attempt still takes the whole per-session window, so a dropped first cancel is never retried; capping it changes the slow-tool-step case and needs a decision"]
async fn first_attempt_does_not_starve_retries() {
    let _lock = reap_test_lock();
    let daemon = FaultDaemon::start(vec![
        Script::cancel(
            "starve-a",
            vec![
                OnCancel::Silent,
                OnCancel::AckAfter(Duration::from_millis(50)),
            ],
        ),
        Script::cancel(
            "starve-b",
            vec![OnCancel::AckAfter(Duration::from_millis(50))],
        ),
    ])
    .await;
    note_session("starve-a");
    note_session("starve-b");
    let cleanup =
        cancel_recorded_sessions_with_cleanup(&daemon.socket, Duration::from_millis(750)).await;
    assert!(
        daemon.count("session/cancel", "starve-a") >= 2,
        "A was retried"
    );
    assert!(
        daemon.count("session/cancel", "starve-b") >= 1,
        "B was asked"
    );
    assert!(
        CANCELLED.lock().unwrap().contains("starve-a"),
        "A ends confirmed"
    );
    assert!(
        CANCELLED.lock().unwrap().contains("starve-b"),
        "B ends confirmed"
    );
    assert_eq!(cleanup, ExitCleanup::Confirmed);
}

/// The cancel phase's per-session window must be measured on tokio's clock,
/// the clock every wait inside `cancel_session` uses. One silent session, a
/// 750 ms window, a paused clock: the phase must end within the window. With
/// the deadline on `std::time::Instant` the window barely moves while tokio's
/// time runs, so the retries are planned from an unspent window and the
/// phase overruns it (ZD-18 / CF-27).
#[tokio::test(start_paused = true)]
async fn phase_deadline_follows_the_tokio_clock() {
    let _lock = reap_test_lock();
    let daemon = FaultDaemon::start(vec![Script::cancel("clock", vec![OnCancel::Silent])]).await;
    note_session("clock");
    let budget = Duration::from_millis(750);
    let started = tokio::time::Instant::now();
    let cleanup = stepped(cancel_recorded_sessions_with_cleanup(
        &daemon.socket,
        budget,
    ))
    .await;
    let virtual_elapsed = started.elapsed();
    assert_eq!(cleanup, ExitCleanup::Unconfirmed);
    assert!(
        virtual_elapsed <= budget + SCHEDULING_SLACK,
        "a {budget:?} cancel window took {virtual_elapsed:?} of tokio time ({} cancels sent)",
        daemon.count("session/cancel", "clock")
    );
}
