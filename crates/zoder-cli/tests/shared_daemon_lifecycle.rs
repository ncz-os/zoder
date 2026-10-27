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
//! The repro is a stand-in engine daemon (a small Unix-socket server that
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
//! Every test here drives the PRODUCTION code, end to end, through the
//! `zoder` binary:
//!
//! * the daemon is started by the PRODUCTION spawn path
//!   (`ensure_engine_daemon`), so the daemon record — and with it the OWNER
//!   that started the daemon — is written by the product and not by the
//!   test;
//! * the clients run the PRODUCTION reap scope (`zoder rescue`), which is
//!   the only path that owns the stop decision
//!   (`reaper::settle_unconfirmed_cancel`). A client that never opens that
//!   scope never reaches the decision at all, so a test that drove
//!   `zoder run` would prove nothing about it: the decision would be
//!   vacuously absent and every assertion about it would pass for the
//!   wrong reason;
//! * the stale-socket tests drive `ensure_engine_daemon` itself, so
//!   deleting the cleanup call site fails them.
//!
//! No real model calls, offline.
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use std::os::unix::net::UnixStream;

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
    #[cfg(target_os = "linux")]
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

/// The last `max` characters of a file, for a failure message.
fn tail(path: &Path, max: usize) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let text = text.trim_end();
    if text.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

/// The stand-in engine daemon.
///
/// It is installed as the `zeroclaw` executable, so the PRODUCTION spawn
/// path (`ensure_engine_daemon` -> `zeroclaw daemon --ephemeral
/// --config-dir DIR`) starts it exactly as it starts the real daemon: the
/// daemon record, and with it the OWNER that started the daemon, is written
/// by the product and not by the test.
///
/// Two things make it a faithful stand-in:
///
/// * it counts the SIGTERMs it receives, so "the daemon was stopped" is an
///   OBSERVATION (a signal the daemon actually received) and not an
///   inference from a pid that stopped existing;
/// * it records every instance that started, so "a second daemon was
///   started" is observable and not merely inferred from a missing record.
///
/// It deliberately does NOT unlink the socket path before binding it: a
/// daemon that finds the path taken must fail, and it is the production
/// stale-socket cleanup that is supposed to make the path free. A daemon
/// that unlinked the path itself would hide exactly the defect these tests
/// exist to catch.
///
/// It also counts every `session/cancel` it receives in a side file, but
/// NEVER acknowledges a cancel.
const FAKE_ZEROCLAW_PY: &str = r#"#!/usr/bin/env python3
import json, os, signal, socket, sys, threading, time

argv = sys.argv[1:]
SOCKET = None
STATE = None
MODEL = "codestral/codestral-2405"
i = 0
while i < len(argv):
    a = argv[i]
    if a == "--socket" and i + 1 < len(argv):
        SOCKET = argv[i + 1]
        i += 2
    elif a == "--state-dir" and i + 1 < len(argv):
        STATE = argv[i + 1]
        i += 2
    elif a == "--config-dir" and i + 1 < len(argv):
        cfg = argv[i + 1]
        SOCKET = os.path.join(cfg, "data", "daemon.sock")
        STATE = os.path.join(cfg, "data")
        i += 2
    else:
        i += 1

if SOCKET is None or STATE is None:
    sys.stderr.write("zeroclaw: no socket path given\n")
    sys.exit(2)

os.makedirs(STATE, exist_ok=True)
CANCEL_FILE = os.path.join(STATE, "cancel_count")
SIGNAL_FILE = os.path.join(STATE, "signal_count")
INSTANCE_FILE = os.path.join(STATE, "instances")

LOCK = threading.Lock()
CONNS = [0]
CANCELS = [0]
SIGNALS = [0]


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

# This instance is running: append its pid so a second daemon is visible.
with LOCK:
    with open(INSTANCE_FILE, "a") as f:
        f.write(str(os.getpid()) + "\n")

srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(SOCKET)
srv.listen(64)


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
                                "uri": "https://gateway.example.invalid/v1"}}
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
                        CANCELS[0] += 1
                        with open(CANCEL_FILE, "w") as f:
                            f.write(str(CANCELS[0]) + "\n")
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
            CONNS[0] += 1
            n = CONNS[0]
        threading.Thread(target=handle, args=(conn, "sess-" + str(n)), daemon=True).start()


threading.Thread(target=accept_loop, daemon=True).start()
sys.stdout.write("ready\n")
sys.stdout.flush()
while True:
    time.sleep(3600)
"#;

/// Install the stand-in engine daemon as the `zeroclaw` executable that the
/// production spawn path looks for.
fn install_fake_zeroclaw(bin_dir: &Path) -> PathBuf {
    let path = bin_dir.join("zeroclaw");
    std::fs::write(&path, FAKE_ZEROCLAW_PY).expect("write the stand-in zeroclaw");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)
            .expect("stat the stand-in zeroclaw")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("make the stand-in zeroclaw executable");
    }
    path
}

/// A stand-in daemon the test starts itself, for the fixtures that need a
/// daemon that is there before `zoder` looks for one.
struct FakeDaemon {
    child: Child,
    pid: u32,
}

/// Start the stand-in daemon directly. The socket and the state files live
/// under `data_dir`.
fn start_fake_daemon(data_dir: &Path, socket: &Path) -> FakeDaemon {
    let py = data_dir.join("stand_in_daemon.py");
    std::fs::write(&py, FAKE_ZEROCLAW_PY).expect("write the stand-in daemon script");
    let err_path = data_dir.join("stand_in_daemon.err");
    let err = std::fs::File::create(&err_path).expect("create the daemon stderr file");
    let child = Command::new("python3")
        .arg(&py)
        .arg("--socket")
        .arg(socket)
        .arg("--state-dir")
        .arg(data_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err))
        .spawn()
        .expect("spawn the stand-in daemon");
    FakeDaemon {
        pid: child.id(),
        child,
    }
}

/// How many SIGTERMs the stand-in daemon has actually received.
///
/// This is the OBSERVATION that the daemon was stopped: a pid that stopped
/// existing could also have been killed by anything else, but a signal the
/// daemon counted is the daemon being asked to stop.
fn signal_count(signal_file: &Path) -> usize {
    std::fs::read_to_string(signal_file)
        .map(|s| s.trim().parse().unwrap_or(0))
        .unwrap_or(0)
}

/// How many `session/cancel` frames the stand-in daemon has received.
#[cfg(target_os = "linux")]
fn cancel_count(cancel_file: &Path) -> usize {
    std::fs::read_to_string(cancel_file)
        .map(|s| s.trim().parse().unwrap_or(0))
        .unwrap_or(0)
}

/// How many daemon instances have started in this scratch home.
fn instance_count(instances_file: &Path) -> usize {
    instance_pids(instances_file).len()
}

/// The pids of the daemon instances that have started in this scratch home,
/// in the order they started.
fn instance_pids(instances_file: &Path) -> Vec<u32> {
    std::fs::read_to_string(instances_file)
        .map(|s| {
            s.lines()
                .filter_map(|l| l.trim().parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The daemon pid named by the production daemon record.
///
/// The record is `<daemon pid> <start ticks> <owner pid> <owner ticks>`; the
/// first field is the daemon the record describes.
#[cfg(target_os = "linux")]
fn record_daemon_pid(record: &Path) -> Option<u32> {
    let raw = std::fs::read_to_string(record).ok()?;
    raw.split_whitespace().next()?.parse::<u32>().ok()
}

/// A `zoder` client process, with its output captured in files.
///
/// The output goes to FILES, not to pipes: a client whose transcript is
/// larger than a pipe buffer would block on the write, and a test that hangs
/// on a full pipe says nothing about the behaviour it is testing.
struct ClientProc {
    child: Child,
    stderr_path: PathBuf,
}

impl ClientProc {
    /// The pid of the client process.
    #[cfg(target_os = "linux")]
    fn id(&self) -> u32 {
        self.child.id()
    }

    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait()
    }

    /// The tail of the client's stderr, for a failure message.
    fn stderr_tail(&self) -> String {
        tail(&self.stderr_path, 2000)
    }
}

/// Spawn a `zoder` client process.
///
/// The client acquires its lease in `ensure_engine_daemon` before it talks to
/// the daemon, so the lease file appears while the turn is in flight.
fn spawn_client(
    zoder_bin: &Path,
    scratch_dir: &Path,
    env: &[(String, String)],
    tag: &str,
    args: &[&str],
) -> ClientProc {
    let stdout_path = scratch_dir.join(format!("{tag}.out"));
    let stderr_path = scratch_dir.join(format!("{tag}.err"));
    let out = std::fs::File::create(&stdout_path).expect("create the client's stdout file");
    let err = std::fs::File::create(&stderr_path).expect("create the client's stderr file");
    let mut cmd = Command::new(zoder_bin);
    cmd.envs(env.iter().cloned());
    // Run from the scratch dir, never the repo root: the client must only
    // ever touch its scratch home, and the relative paths in its config are
    // resolved against the process cwd.
    cmd.current_dir(scratch_dir);
    for a in args {
        cmd.arg(a);
    }
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .expect("spawn the zoder client");
    ClientProc { child, stderr_path }
}

/// The environment every client (and every daemon the production spawn path
/// starts) runs with.
///
/// `bin_dir` is FIRST on `$PATH` so the stand-in `zeroclaw` is the engine
/// the product starts.
fn client_env(
    cfg_dir: &Path,
    zoder_home: &Path,
    bin_dir: &Path,
    scratch_dir: &Path,
) -> Vec<(String, String)> {
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    vec![
        (
            "ZEROCLAW_CONFIG_DIR".to_string(),
            cfg_dir.to_string_lossy().to_string(),
        ),
        (
            "ZODER_HOME".to_string(),
            zoder_home.to_string_lossy().to_string(),
        ),
        ("ZODER_SKIP_CATALOG_ENRICHMENT".to_string(), "1".to_string()),
        (
            "HOME".to_string(),
            scratch_dir.to_string_lossy().to_string(),
        ),
        ("PATH".to_string(), path),
    ]
}

/// Write the scratch home the clients read: the zoder config, the model
/// corpus, and the engine's own config.
///
/// A minimal config.json routes the `default` agent to a fake model on a
/// fake provider so the CLI's pre-dispatch verification agrees with the
/// stand-in daemon's config/get. The engine (zeroclaw) side of the route
/// registry lives in `config.toml`, because
/// `verify_live_zeroclaw_route` compares the daemon's agent route against
/// that on-disk registry before dispatching.
fn write_scratch_home(cfg_dir: &Path, zoder_home: &Path) -> &'static str {
    std::fs::create_dir_all(zoder_home).expect("create the scratch home");
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
      "base_url": "https://gateway.example.invalid/v1",
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
    std::fs::write(zoder_home.join("config.json"), config_body).expect("write the scratch config");
    // The corpus: the real one (a static model list; no network).
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    std::fs::copy(
        repo_root.join("corpus").join("model_corpus.json"),
        &corpus_path,
    )
    .expect("copy the corpus into the scratch home");
    let engine_config = format!(
        r#"[acp]
default_agent = "default"

[agents.default]
model = "{model}"
model_provider = "fake.default"

[providers.models."fake.default"]
model = "{model}"
kind = "openai"
uri = "https://gateway.example.invalid/v1"
"#
    );
    std::fs::write(cfg_dir.join("config.toml"), engine_config).expect("write the engine config");
    model
}

/// The scratch layout every test here uses: the engine's config dir (which
/// holds the socket), the zoder home, and the bin dir the stand-in
/// `zeroclaw` is installed in.
struct Scratch {
    dir: tempfile::TempDir,
    cfg_dir: PathBuf,
    data_dir: PathBuf,
    socket: PathBuf,
    zoder_home: PathBuf,
    bin_dir: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("scratch dir");
        // Keep the AF_UNIX path short: `<ZEROCLAW_CONFIG_DIR>/data/daemon.sock`
        // must stay under ~108 bytes. Use a short config dir next to the
        // tempdir and point every client at it.
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
        let zoder_home = dir.path().join("home");
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        write_scratch_home(&cfg_dir, &zoder_home);
        install_fake_zeroclaw(&bin_dir);
        Self {
            dir,
            cfg_dir,
            data_dir,
            socket,
            zoder_home,
            bin_dir,
        }
    }

    fn env(&self) -> Vec<(String, String)> {
        client_env(
            &self.cfg_dir,
            &self.zoder_home,
            &self.bin_dir,
            self.dir.path(),
        )
    }

    /// The lease file the client with this pid writes when it reaches the
    /// daemon.
    #[cfg(target_os = "linux")]
    fn lease_path(&self, client_pid: u32) -> PathBuf {
        self.data_dir.join("daemons").join(client_pid.to_string())
    }
}

/// THE regression test: two concurrent zoder clients share one daemon.
///
/// Client B exits (its turn budget elapses first) while client A is still
/// mid-session. Before the fix, B's teardown stopped the recorded daemon by
/// pid (the unacknowledged `session/cancel` path), which killed the daemon
/// and severed A's engine mid-session. After the fix, B's teardown sees A's
/// live lease, abandons B's session instead, and the daemon's socket
/// survives B's exit.
///
/// The daemon is stopped EXACTLY ONCE, and only after the LAST client is
/// gone: B must not stop it (A is still using it), and A must stop it (A is
/// the only live consumer left, and a daemon that outlives every user of it
/// is a leak).
#[test]
#[cfg(target_os = "linux")]
fn two_clients_share_one_daemon_and_the_first_exit_does_not_kill_it() {
    let scratch = Scratch::new();
    let env_base = scratch.env();
    let zoder_bin = zoder_binary();

    // --- Client A: it starts the daemon, so it is the daemon's OWNER. Its
    // turn budget is long, so it is still mid-session when B exits.
    let mut client_a = spawn_client(
        &zoder_bin,
        scratch.dir.path(),
        &env_base,
        "client-a",
        &[
            "rescue",
            "--agent",
            "default",
            "--agent-timeout",
            "20",
            "--allow-paid",
            "client A: a turn that runs until its budget elapses",
        ],
    );
    let a_pid = client_a.id();
    // A's lease appears once A has reached the daemon, which is also the
    // point at which the daemon is up.
    assert!(
        wait_for_file(&scratch.lease_path(a_pid), Duration::from_secs(60)),
        "client A must acquire a lease on the daemon before B starts; \
         client A stderr:\n{}",
        client_a.stderr_tail()
    );
    assert!(
        wait_for_file(&scratch.socket, Duration::from_secs(30)),
        "client A must have started the daemon"
    );

    // The production daemon record names the daemon AND its owner. The
    // owner is the process that started it, and only the spawner knows it.
    let record = scratch.data_dir.join("daemon.pid");
    let daemon_pid = record_daemon_pid(&record).expect("the production daemon record");
    assert!(
        pid_alive(daemon_pid),
        "the daemon the record names must be running"
    );
    let owner_pid: u32 = std::fs::read_to_string(&record)
        .expect("read the daemon record")
        .split_whitespace()
        .nth(2)
        .and_then(|f| f.parse().ok())
        .expect("the record must name the owner that started the daemon");
    assert_eq!(
        owner_pid, a_pid,
        "the owner in the daemon record must be the client that started the daemon"
    );

    let signal_file = scratch.data_dir.join("signal_count");
    let cancel_file = scratch.data_dir.join("cancel_count");

    // --- Client B: starts second. Its turn budget is shorter, so it exits
    // while A is still mid-session.
    let mut client_b = spawn_client(
        &zoder_bin,
        scratch.dir.path(),
        &env_base,
        "client-b",
        &[
            "rescue",
            "--agent",
            "default",
            "--agent-timeout",
            "6",
            "--allow-paid",
            "client B: a turn that ends while A is still running",
        ],
    );
    let b_pid = client_b.id();
    assert!(
        wait_for_file(&scratch.lease_path(b_pid), Duration::from_secs(30)),
        "client B must acquire a lease on the daemon; client B stderr:\n{}",
        client_b.stderr_tail()
    );

    // Both clients are now mid-session (both leases exist, both turns are
    // running against the silent stand-in daemon). B's 6 s budget elapses
    // first: B sends a `session/cancel` the daemon never acknowledges, and
    // B's teardown runs.
    //
    // On BASE, B's `settle_unconfirmed_cancel` stops the recorded daemon by
    // pid — the stand-in daemon (a real python process) is killed — and A's
    // engine is severed mid-session.
    //
    // On the fix branch, B's teardown sees A's live lease and abandons B's
    // session instead; the daemon survives.
    let b_status = client_b.wait().expect("wait for client B");
    let _ = b_status;

    // THE KEY ASSERTION: the daemon must still be alive (its pid, its
    // socket) after B exits, because A is still mid-session.
    assert!(
        pid_alive(daemon_pid),
        "the shared daemon must still be running after client B exits; on \
         BASE, B's teardown stops the recorded daemon by pid (the cancel was \
         never acknowledged), which kills the daemon and severs client A's \
         engine mid-session; client B stderr:\n{}",
        client_b.stderr_tail()
    );
    assert_eq!(
        signal_count(&signal_file),
        0,
        "client B must not stop the daemon: client A is still using it"
    );
    assert!(
        scratch.socket.exists(),
        "the daemon socket must still exist after client B exits; on BASE, \
         the daemon's death takes the socket with it"
    );
    match UnixStream::connect(&scratch.socket) {
        Ok(_) => {}
        Err(e) => panic!(
            "a new connection to the daemon must be accepted after client B \
             exits (the daemon is still serving client A); got {e}"
        ),
    }

    // Now wait for A to exit (its 20 s budget elapses). A's turn was never
    // severed: A created a session and attempted a cancel on the live
    // daemon.
    let a_status = client_a.wait().expect("wait for client A");
    let _ = a_status;

    // Both clients attempted to cancel their sessions on the shared daemon.
    assert!(
        cancel_count(&cancel_file) >= 2,
        "both clients must have attempted session/cancel on the shared \
         daemon; the stand-in daemon saw {} cancel frame(s)",
        cancel_count(&cancel_file)
    );

    // After BOTH clients have exited, no live lease may remain: this is the
    // exact condition the "last client stops the daemon" rule checks, so a
    // daemon started for two clients is cleaned up exactly once, by the
    // last client.
    assert!(
        !zoder_core::any_daemon_lease(&scratch.socket),
        "after both clients exit, no live lease may remain (the last client \
         is the one that stops the daemon, exactly once)"
    );

    // The last client stopped the daemon — and stopped it EXACTLY ONCE.
    // The count is the daemon's own count of the SIGTERMs it received, so
    // this is an observation of the daemon being asked to stop, not an
    // inference from a pid that stopped existing. No manual kill is
    // involved: the test never signals the daemon.
    assert!(
        wait_for_exit(daemon_pid, Duration::from_secs(15)),
        "the last client must stop the daemon it started; a daemon that \
         outlives every user of it is a leak; client A stderr:\n{}",
        client_a.stderr_tail()
    );
    assert_eq!(
        signal_count(&signal_file),
        1,
        "the daemon must be stopped EXACTLY ONCE, by the last client"
    );
}

/// A socket with no live daemon (stale) is the shape the stale-socket
/// cleanup handles: the path is there, nothing answers on it.
///
/// The test drives the PRODUCTION start path — the `zoder` binary, which
/// calls `ensure_engine_daemon` — so the probe, the identity capture, the
/// daemon lock, the removal and the respawn are exercised together. A test
/// that only called the cleanup helper would keep passing if the product
/// stopped calling the cleanup at all.
#[test]
fn a_stale_socket_is_removed_and_the_daemon_is_respawned_by_the_production_start() {
    let scratch = Scratch::new();
    let env_base = scratch.env();

    // A real daemon that dies without unbinding its socket: the shape of a
    // stale socket left behind by a daemon that was killed.
    let mut daemon = start_fake_daemon(&scratch.data_dir, &scratch.socket);
    assert!(
        wait_for_file(&scratch.socket, Duration::from_secs(15)),
        "the stand-in daemon must bind the socket"
    );
    let daemon_pid = daemon.pid;
    let stale_identity =
        zoder_core::socket_identity(&scratch.socket).expect("the socket entry has an identity");
    // SIGKILL: the daemon gets no chance to clean up, which is exactly the
    // shape of a socket left behind by a daemon that died.
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(daemon_pid.to_string())
        .output();
    let _ = daemon.child.wait();
    assert!(
        !pid_alive(daemon_pid),
        "the daemon fixture must be gone before the cleanup runs"
    );
    assert!(
        scratch.socket.exists(),
        "precondition: the dead daemon's socket file is still on disk"
    );

    // The production start path. `zoder agents` calls `ensure_engine_daemon`,
    // which is the function that owns the stale-socket cleanup.
    let mut client = spawn_client(
        &zoder_binary(),
        scratch.dir.path(),
        &env_base,
        "stale",
        &["agents"],
    );
    let status = client.wait().expect("wait for the client");
    assert!(
        status.success(),
        "the production daemon start must succeed after a stale socket was \
         removed; client stderr:\n{}",
        client.stderr_tail()
    );

    // The stale entry is gone AND a fresh daemon took its place: the socket
    // is a NEW directory entry (a new inode), so the stale one really was
    // removed and re-bound. A daemon that could not bind would have failed
    // its startup instead.
    assert!(
        scratch.socket.exists(),
        "the daemon must be listening on a socket again"
    );
    let new_identity =
        zoder_core::socket_identity(&scratch.socket).expect("the socket entry has an identity");
    assert_ne!(
        new_identity, stale_identity,
        "the stale socket must have been removed and a new one bound: the \
         same directory entry is still there, so the cleanup never ran"
    );
    assert_eq!(
        instance_count(&scratch.data_dir.join("instances")),
        2,
        "the dead daemon and the respawned one are the only two instances"
    );

    // The respawned daemon is alive, and it is a NEW process.
    let instances = instance_pids(&scratch.data_dir.join("instances"));
    let respawned = *instances.last().expect("the respawned daemon");
    assert!(pid_alive(respawned), "the respawned daemon must be running");
    assert_ne!(
        respawned, daemon_pid,
        "the respawned daemon must be a NEW process, not the dead one"
    );

    // The production daemon record names the respawned daemon. The record
    // carries the process start time, which is read from /proc, so it
    // exists only on the platform the fleet runs on.
    #[cfg(target_os = "linux")]
    {
        let record_pid = record_daemon_pid(&scratch.data_dir.join("daemon.pid"))
            .expect("the production daemon record must name the respawned daemon");
        assert_eq!(
            record_pid, respawned,
            "the production record must name the respawned daemon"
        );
    }

    // Clean up: the respawned daemon outlives the client that started it
    // (that is what a shared daemon is), so the test stops it by exact pid.
    let _ = Command::new("kill").arg(respawned.to_string()).output();
}

/// A LIVE daemon's socket survives the production start.
///
/// The cleanup unlinks a socket only when it has proved that nothing answers
/// on it. A daemon that is running — even one that does not answer the
/// handshake — keeps its socket: unlinking it would leave a daemon that
/// nothing can reach any more.
#[test]
fn a_live_daemons_socket_survives_the_production_start() {
    let scratch = Scratch::new();
    let env_base = scratch.env();

    // A real daemon that answers the handshake: the shape of a healthy
    // shared daemon.
    let mut daemon = start_fake_daemon(&scratch.data_dir, &scratch.socket);
    assert!(
        wait_for_file(&scratch.socket, Duration::from_secs(15)),
        "the stand-in daemon must bind the socket"
    );
    let daemon_pid = daemon.pid;
    let identity =
        zoder_core::socket_identity(&scratch.socket).expect("the socket entry has an identity");

    // The production start path finds the live daemon and uses it.
    let mut client = spawn_client(
        &zoder_binary(),
        scratch.dir.path(),
        &env_base,
        "live",
        &["agents"],
    );
    let status = client.wait().expect("wait for the client");
    assert!(
        status.success(),
        "the production daemon start must succeed against a live daemon; \
         client stderr:\n{}",
        client.stderr_tail()
    );

    assert_eq!(
        zoder_core::socket_identity(&scratch.socket),
        Some(identity),
        "a live daemon's socket must survive: it is the SAME directory \
         entry, not a replacement"
    );
    assert!(
        pid_alive(daemon_pid),
        "the live daemon must still be running"
    );
    assert_eq!(
        signal_count(&scratch.data_dir.join("signal_count")),
        0,
        "the live daemon must not be signalled"
    );
    assert_eq!(
        instance_count(&scratch.data_dir.join("instances")),
        1,
        "no second daemon may be started while one is live"
    );

    // Clean up: stop the fixture by exact pid and reap it. The daemon
    // counts the SIGTERM it receives, which is the same observation the
    // shared-daemon test uses to prove the daemon was stopped exactly once.
    let _ = Command::new("kill").arg(daemon_pid.to_string()).output();
    let _ = daemon.child.wait();
    assert!(
        wait_for_exit(daemon_pid, Duration::from_secs(5)),
        "the daemon fixture must be gone"
    );
    assert_eq!(
        signal_count(&scratch.data_dir.join("signal_count")),
        1,
        "the fixture must count the SIGTERM it received: the shared-daemon \
         test reads this counter to prove the daemon was stopped exactly once"
    );
}
