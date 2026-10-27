#![cfg(target_os = "linux")]
//! Integration tests for the shared engine daemon lifecycle.
//!
//! The regression these tests guard:
//!
//! Two `zoder` processes on the same host share one engine daemon (one
//! `<data dir>/daemon.sock`). When one of them exits — its loop ended, its
//! model hit its context window, or it received SIGTERM — its teardown must
//! NOT stop the daemon while the other process is still driving sessions
//! through it. Before the lease mechanism, an unacknowledged
//! `session/cancel` on a shared daemon killed the daemon by pid (see
//! `settle_unconfirmed_cancel`), which severed the other process's engine
//! mid-session and left it with "engine unreachable" / "daemon closed
//! connection without acknowledging session/cancel".
//!
//! The repro is a fake engine daemon (a small Unix-socket server that
//! speaks enough of the ACP JSON-RPC protocol for `zoder`'s setup path)
//! that deliberately does not acknowledge `session/cancel`. Two `zoder`
//! client processes are run against it:
//!
//! * **On BASE** the client that exits first kills the daemon (or removes
//!   its socket) while the other client is mid-session, so the surviving
//!   client's engine is lost and the test FAILS.
//! * **On the fix branch** the surviving client keeps its engine; the
//!   exiting client's session is abandoned; the daemon's socket still
//!   exists and accepts new connections after the first client exits; and
//!   after BOTH clients exit, the daemon has been stopped EXACTLY ONCE — by
//!   the last client, which is the only one left that could prove it was
//!   alone.
//!
//! Every test here drives the PRODUCTION code: the fake daemon writes the
//! same daemon record the spawner writes (a real start time, so the record
//! names a live process rather than a recycled pid), and the cleanup tests
//! call the production cleanup entry point instead of a copy of its rules.
//!
//! No real model calls, offline.
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Where the built `zoder` binary lives, relative to this test binary.
fn zoder_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    let dep_dir = exe.parent().expect("deps dir");
    let candidate = dep_dir.join("..").join("zoder");
    if candidate.exists() {
        return candidate;
    }
    let candidate = dep_dir.join("..").join("release").join("zoder");
    if candidate.exists() {
        return candidate;
    }
    panic!("cannot locate the zoder binary next to {}", exe.display());
}

/// Wait for `pid` to die, up to `budget`. Returns true if it did.
fn wait_for_exit(pid: u32, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if !pid_alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// True if `pid` names a live (non-zombie) process.
fn pid_alive(pid: u32) -> bool {
    {
        // A zombie holds its pid but runs nothing, and the daemon fixtures
        // are reaped by their parent, so the state field is what decides.
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => match stat.rfind(')') {
                Some(pos) => {
                    let state = stat[pos + 1..].split_whitespace().next().unwrap_or("");
                    state != "Z" && state != "X" && !state.is_empty()
                }
                None => true,
            },
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Signal 0 checks for existence without delivering anything. The
        // fixtures are reaped before this is asked, so a zombie never
        // answers here.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
}

/// Wait for a file to exist, up to `budget`.
fn wait_for_file(path: &Path, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if path.exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The fake engine daemon as a Python script.
///
/// It answers `initialize`, `agents/status`, `config/get`, `session/new`,
/// and `session/prompt`, streams one `session/update`, and then goes silent
/// (no `turn_complete`), so every client's turn runs until its budget
/// elapses.
///
/// Two things make it a faithful stand-in for the real daemon:
///
/// * it writes its own daemon record in the SAME shape the spawner writes —
///   the pid AND the process start time — so the record names a live
///   process instead of a recycled one. Without the start time the record
///   is unreadable as "this daemon", and every stop decision that depends
///   on it is skipped: the test would then prove nothing about the stop
///   path it is written for.
/// * it counts the SIGTERMs it receives, so "the daemon was stopped" is an
///   OBSERVATION (a signal the daemon actually received) and not an
///   inference from a pid that stopped existing.
///
/// It also counts every `session/cancel` it receives in a side file, but
/// NEVER acknowledges a cancel.
const FAKE_DAEMON_PY: &str = r#"
import json, os, signal, socket, sys, threading, time

SOCKET = sys.argv[1]
PIDFILE = sys.argv[2]
COUNT_FILE = sys.argv[3]
MODEL = sys.argv[4]
SIGNAL_FILE = sys.argv[5]


def start_ticks():
    # The process start time, the way the daemon record names it: stat
    # field 22, which is the 20th field after the command name.
    try:
        with open("/proc/self/stat") as f:
            raw = f.read()
        tail = raw.rsplit(")", 1)[1].split()
        return int(tail[19])
    except Exception:
        return 0


os.makedirs(os.path.dirname(SOCKET), exist_ok=True)
try:
    os.unlink(SOCKET)
except FileNotFoundError:
    pass
srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(SOCKET)
srv.listen(64)
with open(PIDFILE, "w") as f:
    f.write(f"{os.getpid()} {start_ticks()}\n")

COUNTER = [0]
SIGNALS = [0]
LOCK = threading.Lock()
CANCELS = []


def on_term(signum, frame):
    # The daemon was asked to stop. Record that it happened, then leave the
    # socket file behind: an exiting daemon does NOT unlink its own socket,
    # and the stale entry it leaves is the shape the cleanup handles.
    with LOCK:
        SIGNALS[0] += 1
        with open(SIGNAL_FILE, "w") as f:
            f.write(str(SIGNALS[0]) + "\n")
    os._exit(0)


signal.signal(signal.SIGTERM, on_term)


def reply(conn, rid, result):
    try:
        conn.sendall((json.dumps({"jsonrpc": "2.0", "id": rid, "result": result}) + "\n").encode())
    except Exception:
        pass


def handle(conn, sid):
    buf = b""
    try:
        while True:
            data = conn.recv(65536)
            if not data:
                break
            buf += data
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                line = line.strip()
                if not line:
                    continue
                try:
                    frame = json.loads(line)
                except Exception:
                    continue
                method = frame.get("method", "")
                rid = frame.get("id")
                if method == "initialize":
                    reply(conn, rid, {"protocol_version": 1, "agent_capabilities": {}})
                elif method == "agents/status":
                    reply(conn, rid, {"agents": [{"alias": "default", "enabled": True,
                        "live_sessions": 0, "persisted_sessions": 0, "channels": []}]})
                elif method == "config/get":
                    reply(conn, rid, {
                        "agents": {"default": {"model": MODEL, "model_provider": "fake.default"}},
                        "providers": {
                            "models": {"fake.default": {"model": MODEL, "kind": "openai",
                                "uri": "http://127.0.0.1:1"}}
                        },
                        "acp": {"default_agent": "default"},
                    })
                elif method == "session/new":
                    reply(conn, rid, {"session_id": sid})
                elif method == "session/prompt":
                    reply(conn, rid, {})
                    # One update to keep the client's turn "in progress";
                    # then NO turn_complete, so the turn runs until the
                    # client's budget elapses.
                    try:
                        conn.sendall((json.dumps({
                            "jsonrpc": "2.0", "method": "session/update",
                            "params": {"session_id": sid,
                                        "type": "agent_message_chunk",
                                        "update": {"sessionUpdate": "agent_message_chunk",
                                                    "content": {"type": "text",
                                                                "text": "fake turn"}}}}) + "\n").encode())
                    except Exception:
                        pass
                elif method == "session/cancel":
                    # Count but do NOT acknowledge: the shape of the bug.
                    with LOCK:
                        CANCELS.append(sid)
                        with open(COUNT_FILE, "w") as f:
                            f.write(str(len(CANCELS)) + "\n")
                else:
                    pass
    except Exception:
        pass
    finally:
        try:
            conn.close()
        except Exception:
            pass


def accept_loop():
    while True:
        try:
            conn, _ = srv.accept()
        except OSError:
            return
        with LOCK:
            COUNTER[0] += 1
            n = COUNTER[0]
        threading.Thread(target=handle, args=(conn, "sess-" + str(n)), daemon=True).start()


threading.Thread(target=accept_loop, daemon=True).start()
sys.stdout.write("ready\n")
sys.stdout.flush()
while True:
    time.sleep(3600)
"#;

/// The fake daemon's side files: the cancel counter and the SIGTERM counter.
struct FakeDaemon {
    child: Child,
    pid: u32,
    /// Only the shared-daemon test reads it, and that test is Linux only.
    cancel_count_file: PathBuf,
    signal_count_file: PathBuf,
}

/// Start the fake daemon. The socket, pid file, cancel counter and signal
/// counter live under `data_dir`.
fn start_fake_daemon(data_dir: &Path, socket: &Path, model: &str) -> FakeDaemon {
    let py = data_dir.join("fake_daemon.py");
    std::fs::write(&py, FAKE_DAEMON_PY).expect("write the fake daemon script");
    let count_file = data_dir.join("cancel_count");
    let signal_file = data_dir.join("signal_count");
    let child = Command::new("python3")
        .arg(&py)
        .arg(socket)
        .arg(data_dir.join("daemon.pid"))
        .arg(&count_file)
        .arg(model)
        .arg(&signal_file)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the fake daemon");
    FakeDaemon {
        pid: child.id(),
        child,
        cancel_count_file: count_file,
        signal_count_file: signal_file,
    }
}

/// Read the fake daemon's cancel count from its side file.
fn cancel_count(count_file: &Path) -> usize {
    std::fs::read_to_string(count_file)
        .map(|s| s.trim().parse().unwrap_or(0))
        .unwrap_or(0)
}

/// How many SIGTERMs the fake daemon has actually received.
///
/// This is the OBSERVATION that the daemon was stopped: a pid that stopped
/// existing could also have been killed by anything else, but a signal the
/// daemon counted is the daemon being asked to stop.
fn signal_count(signal_file: &Path) -> usize {
    std::fs::read_to_string(signal_file)
        .map(|s| s.trim().parse().unwrap_or(0))
        .unwrap_or(0)
}

/// Spawn a `zoder` client process that drives one (deliberately slow) turn
/// against the fake daemon. The client acquires its lease in
/// `ensure_engine_daemon` before `session/new`, so the lease file appears
/// while the turn is in flight.
fn spawn_client(
    zoder_bin: &Path,
    scratch_dir: &Path,
    env: &[(String, String)],
    prompt: &str,
    args: &[&str],
) -> Child {
    let mut cmd = Command::new(zoder_bin);
    cmd.envs(env.iter().cloned());
    // Run from the scratch dir, never the repo root: the client must only
    // ever touch its scratch home, and the relative paths in its config are
    // resolved against the process cwd.
    cmd.current_dir(scratch_dir);
    for a in args {
        cmd.arg(a);
    }
    // The prompt is passed via stdin: `zoder run` reads the task from stdin
    // when no `-t` is given, and it does NOT accept a positional prompt arg.
    let prompt_bytes = prompt.as_bytes().to_vec();
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the zoder client");
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin
            .write_all(&prompt_bytes)
            .expect("write prompt to stdin");
    }
    child
}

/// THE regression test: two concurrent zoder clients share one fake daemon.
/// Client B exits (its turn budget elapses first) while client A is still
/// mid-session. Before the fix, B's teardown stopped the recorded daemon by
/// pid (the unacknowledged `session/cancel` path), which killed the fake
/// daemon and severed A's engine mid-session. After the fix, B's teardown
/// sees A's live lease, abandons B's session instead, and the daemon's
/// socket survives B's exit.
///
/// The daemon is stopped EXACTLY ONCE, and only after the LAST client is
/// gone: B must not stop it (A is still using it), and A must stop it (A is
/// the only live consumer left, and a daemon that outlives every user of it
/// is a leak).
#[test]
fn two_clients_share_one_daemon_and_the_first_exit_does_not_kill_it() {
    let dir = tempfile::tempdir().expect("scratch dir");

    // Keep the AF_UNIX path short: `<ZEROCLAW_CONFIG_DIR>/data/daemon.sock`
    // must stay under ~108 bytes. Use a short config dir next to the
    // tempdir and point both clients' ZEROCLAW_CONFIG_DIR at it.
    let cfg_dir = dir.path().join("cf");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let data_dir = cfg_dir.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let socket = data_dir.join("daemon.sock");
    assert!(
        socket.as_os_str().len() < 100,
        "socket path must stay short: {}",
        socket.display()
    );

    // --- The zoder home: one scratch home for BOTH clients (one shared
    // lease directory, one shared daemon.pid). A minimal config.json routes
    // the `default` agent to a fake model on a fake provider so the CLI's
    // pre-dispatch verification agrees with the fake daemon's config/get.
    let zoder_home = dir.path().join("home");
    std::fs::create_dir_all(&zoder_home).unwrap();
    let model = "codestral/codestral-2405";
    let corpus_path = zoder_home.join("model_corpus.json");
    let config_body = format!(
        r#"{{
  "default_provider": "fake",
  "primary_model": "{model}",
  "corpus_path": "{}",
  "ledger_path": "ledger.jsonl",
  "health_path": "health.json",
  "providers": [
    {{
      "id": "fake",
      "kind": "openai-chat",
      "base_url": "http://127.0.0.1:1",
      "serves": ["{model}"],
      "billing": "free",
      "auth": {{ "type": "none" }}
    }}
  ],
  "agents": {{
    "default": {{ "model": "{model}", "model_provider": "fake.default" }}
  }}
}}"#,
        corpus_path.display()
    );
    std::fs::write(zoder_home.join("config.json"), config_body).unwrap();
    // The corpus: the real one (a static model list; no network).
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    std::fs::copy(
        repo_root.join("corpus").join("model_corpus.json"),
        &corpus_path,
    )
    .expect("copy the corpus into the scratch home");
    // The engine (zeroclaw) side of the route registry: `verify_live_zeroclaw_route`
    // compares the daemon's agent route against this on-disk registry before
    // dispatching, so the `default` agent must name the same fake model here.
    // (The fake daemon answers `config/get` with the matching route, so the
    // live re-verification agrees too.)
    let engine_config = format!(
        r#"[acp]
default_agent = "default"

[agents.default]
model = "{model}"
model_provider = "fake.default"

[providers.models."fake.default"]
model = "{model}"
kind = "openai"
uri = "http://127.0.0.1:1"
"#
    );
    std::fs::write(cfg_dir.join("config.toml"), engine_config).unwrap();

    let env_base: Vec<(String, String)> = vec![
        (
            "ZEROCLAW_CONFIG_DIR".to_string(),
            cfg_dir.to_string_lossy().to_string(),
        ),
        (
            "ZODER_HOME".to_string(),
            zoder_home.to_string_lossy().to_string(),
        ),
        ("ZODER_SKIP_CATALOG_ENRICHMENT".to_string(), "1".to_string()),
        ("HOME".to_string(), dir.path().to_string_lossy().to_string()),
        (
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        ),
    ];

    // --- Start the fake daemon.
    let daemon = start_fake_daemon(&data_dir, &socket, model);
    let daemon_pid = daemon.pid;
    let cancel_file = daemon.cancel_count_file.clone();
    let signal_file = daemon.signal_count_file.clone();
    // Wait for the daemon's pid file (proof it bound the socket).
    assert!(
        wait_for_file(&data_dir.join("daemon.pid"), Duration::from_secs(15)),
        "the fake daemon must write its pid file after binding"
    );
    assert!(
        wait_for_file(&socket, Duration::from_secs(15)),
        "the fake daemon must bind the socket"
    );
    // Drain the readiness line on a background thread so stdout does not
    // fill its pipe and block the daemon; the thread reaps the daemon when
    // the test ends (kill + wait) so no zombie is left behind.
    std::thread::spawn(move || {
        let mut child = daemon.child;
        // Give the accept loop a moment to start.
        std::thread::sleep(Duration::from_millis(200));
        let _ = child.wait();
    });

    let zoder_bin = zoder_binary();

    // --- Client A: starts first, so its lease is the EARLIEST. Its turn
    // budget is long, so it is still mid-session when B exits.
    let mut client_a = spawn_client(
        &zoder_bin,
        dir.path(),
        &env_base,
        "client A: a turn that runs until its budget elapses",
        &[
            "run",
            "--agent",
            "default",
            "--agent-timeout",
            "25",
            "--allow-paid",
        ],
    );
    let a_pid = client_a.id();
    let lease_a = data_dir.join("daemons").join(a_pid.to_string());
    assert!(
        wait_for_file(&lease_a, Duration::from_secs(30)),
        "client A must acquire a lease on the daemon before B starts"
    );

    // --- Client B: starts second. Its turn budget is shorter, so it exits
    // while A is still mid-session.
    let mut client_b = spawn_client(
        &zoder_bin,
        dir.path(),
        &env_base,
        "client B: a turn that ends while A is still running",
        &[
            "run",
            "--agent",
            "default",
            "--agent-timeout",
            "8",
            "--allow-paid",
        ],
    );
    let b_pid = client_b.id();
    let lease_b = data_dir.join("daemons").join(b_pid.to_string());
    assert!(
        wait_for_file(&lease_b, Duration::from_secs(30)),
        "client B must acquire a lease on the daemon"
    );

    // Both clients are now mid-session (both leases exist, both turns are
    // running against the silent fake daemon). B's 8 s budget elapses
    // first: B sends a `session/cancel` the daemon never acknowledges, and
    // B's teardown runs.
    //
    // On BASE, B's `settle_unconfirmed_cancel` stops the recorded daemon by
    // pid — the fake daemon (a real python process) is killed — and A's
    // engine is severed mid-session.
    //
    // On the fix branch, B's teardown sees A's live lease and abandons B's
    // session instead; the daemon survives.
    let b_status = client_b.wait().expect("wait for client B");

    // THE KEY ASSERTION: the daemon must still be alive (its pid, its
    // socket) after B exits, because A is still mid-session.
    assert!(
        pid_alive(daemon_pid),
        "the shared daemon must still be running after client B exits; on \
         BASE, B's teardown stops the recorded daemon by pid (the cancel was \
         never acknowledged), which kills the daemon and severs client A's \
         engine mid-session"
    );
    assert_eq!(
        signal_count(&signal_file),
        0,
        "client B must not stop the daemon: client A is still using it"
    );
    assert!(
        socket.exists(),
        "the daemon socket must still exist after client B exits; on BASE, \
         the daemon's death takes the socket with it"
    );
    match UnixStream::connect(&socket) {
        Ok(_) => {}
        Err(e) => panic!(
            "a new connection to the daemon must be accepted after client B \
             exits (the daemon is still serving client A); got {e}"
        ),
    }

    // Now wait for A to exit (its 25 s budget elapses). A's turn was never
    // severed: A created a session and attempted a cancel on the live
    // daemon.
    let a_status = client_a.wait().expect("wait for client A");
    let _ = (a_status, b_status);

    // Both clients attempted to cancel their sessions on the shared daemon.
    assert!(
        cancel_count(&cancel_file) >= 2,
        "both clients must have attempted session/cancel on the shared \
         daemon; the fake daemon saw {} cancel frame(s)",
        cancel_count(&cancel_file)
    );

    // After BOTH clients have exited, no live lease may remain: this is the
    // exact condition the "last client stops the daemon" rule checks, so a
    // daemon started for two clients is cleaned up exactly once, by the
    // last client.
    assert!(
        !zoder_core::any_daemon_lease(&socket),
        "after both clients exit, no live lease may remain (the last client \
         is the one that stops the daemon, exactly once)"
    );

    // The last client stopped the daemon — and stopped it EXACTLY ONCE.
    // The count is the daemon's own count of the SIGTERMs it received, so
    // this is an observation of the daemon being asked to stop, not an
    // inference from a pid that stopped existing. No manual kill is
    // involved: the test never signals the daemon.
    assert!(
        wait_for_exit(daemon_pid, Duration::from_secs(10)),
        "the last client must stop the daemon it started; a daemon that \
         outlives every user of it is a leak"
    );
    assert_eq!(
        signal_count(&signal_file),
        1,
        "the daemon must be stopped EXACTLY ONCE, by the last client"
    );

    // Clean up: the daemon is already gone; the reaper thread (which owns
    // the Child) collects it.
}
