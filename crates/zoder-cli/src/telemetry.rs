//! Structured telemetry: human text or one JSON object per line on stderr.
//!
//! The format is resolved once per process ([`format`]):
//!
//! * `ZODER_LOG_FORMAT=json` selects JSON; `text`, `human` or `plain` force
//!   human text. Any other value keeps human text and says so once: a typo
//!   must never switch an operator's terminal to JSON silently.
//! * Unset, a `[deployment] mode = "fleet"` config selects JSON (fleet hosts
//!   feed a log pipeline). Any failure to read the config means human.
//!
//! Human mode is today's output byte for byte: the tracing `fmt` layer as it
//! was, and operator messages printed by `eprintln!`. JSON mode installs
//! [`JsonLayer`] instead, and three event families go through it:
//!
//! * `zoder::op` -- operator messages, sent through [`oplog!`] (the seam that
//!   replaces a bare `eprintln!("[loop] ...")`). The leading `[component] ` or
//!   `component: ` prefix moves into a `component` field.
//! * `zoder::turn` -- one [`TurnEvent`] (`zoder.turn/1`) per loop author turn.
//! * `zoder::run` -- one [`RunEvent`] (`zoder.run/1`) per process.
//!
//! The event structs are serialized into a `json` field that the layer merges
//! into the line, so numbers and nulls stay typed instead of being nested as a
//! string. Their shape is pinned by a snapshot test
//! (`tests/data/telemetry_event_schema.snap.json`).

use std::fmt;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

/// Environment variable that selects the stderr format.
pub(crate) const LOG_FORMAT_ENV: &str = "ZODER_LOG_FORMAT";

/// Schema tag of [`TurnEvent`].
pub(crate) const TURN_SCHEMA: &str = "zoder.turn/1";
/// Schema tag of [`RunEvent`].
pub(crate) const RUN_SCHEMA: &str = "zoder.run/1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogFormat {
    Human,
    Json,
}

/// How [`resolve_format`] reached its answer, so an unknown value can be
/// reported once instead of being swallowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resolution {
    pub format: LogFormat,
    /// The `ZODER_LOG_FORMAT` value that was not recognised, if any.
    pub rejected: Option<String>,
}

/// The format for an environment value and the deployment mode.
/// `fleet_mode` is only consulted when the variable is unset or empty.
pub(crate) fn resolve_format(env: Option<&str>, fleet_mode: impl FnOnce() -> bool) -> Resolution {
    match env.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => match value.to_ascii_lowercase().as_str() {
            "json" => Resolution {
                format: LogFormat::Json,
                rejected: None,
            },
            "text" | "human" | "plain" => Resolution {
                format: LogFormat::Human,
                rejected: None,
            },
            _ => Resolution {
                format: LogFormat::Human,
                rejected: Some(value.to_string()),
            },
        },
        None => Resolution {
            format: if fleet_mode() {
                LogFormat::Json
            } else {
                LogFormat::Human
            },
            rejected: None,
        },
    }
}

fn configured_fleet_mode() -> bool {
    zoder_core::config::Config::load_unvalidated()
        .map(|cfg| cfg.deployment.mode.is_fleet())
        .unwrap_or(false)
}

static FORMAT: OnceLock<Resolution> = OnceLock::new();

fn resolution() -> &'static Resolution {
    FORMAT.get_or_init(|| {
        resolve_format(
            std::env::var(LOG_FORMAT_ENV).ok().as_deref(),
            configured_fleet_mode,
        )
    })
}

/// The process's stderr format.
pub(crate) fn format() -> LogFormat {
    resolution().format
}

/// The filter: `RUST_LOG` wins; otherwise `-v`/`-vv` set the level. In JSON
/// mode the three zoder event families are pinned on at `info`, so the
/// default `warn` level never drops an operator message.
fn env_filter(verbose: u8, format: LogFormat) -> tracing_subscriber::EnvFilter {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(match verbose {
            0 => "warn",
            1 => "info,zoder=debug,zoder_core=debug",
            _ => "debug,zoder=trace,zoder_core=trace",
        })
    });
    if format == LogFormat::Human {
        return filter;
    }
    ["zoder::op=info", "zoder::turn=info", "zoder::run=info"]
        .into_iter()
        .fold(filter, |filter, directive| {
            filter.add_directive(directive.parse().expect("static directive parses"))
        })
}

/// Install the process's tracing subscriber.
pub(crate) fn init(verbose: u8) {
    use tracing_subscriber::prelude::*;
    let resolution = resolution();
    match resolution.format {
        LogFormat::Human => {
            // Unchanged from before structured telemetry existed.
            let _ = tracing_subscriber::fmt()
                .with_env_filter(env_filter(verbose, LogFormat::Human))
                .with_writer(std::io::stderr)
                .without_time()
                .try_init();
            if let Some(value) = &resolution.rejected {
                eprintln!(
                    "zoder: warning: {LOG_FORMAT_ENV}={value:?} is not a log format \
                     (json, text); using text"
                );
            }
        }
        LogFormat::Json => {
            let _ = tracing_subscriber::registry()
                .with(
                    JsonLayer::new(std::io::stderr)
                        .with_filter(env_filter(verbose, LogFormat::Json)),
                )
                .try_init();
        }
    }
}

/// `message` with the leading newlines (terminal spacing) and the
/// `[component] ` or `component: ` prefix removed.
pub(crate) fn strip_prefix<'a>(component: &str, message: &'a str) -> &'a str {
    let trimmed = message.trim_start_matches('\n');
    if let Some(rest) = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_prefix(component))
        .and_then(|rest| rest.strip_prefix(']'))
    {
        return rest.strip_prefix(' ').unwrap_or(rest);
    }
    if let Some(rest) = trimmed
        .strip_prefix(component)
        .and_then(|rest| rest.strip_prefix(':'))
    {
        return rest.strip_prefix(' ').unwrap_or(rest);
    }
    trimmed
}

/// The body of [`oplog!`]: `eprintln!` in human mode, a `zoder::op` event in
/// JSON mode.
pub(crate) fn emit(component: &'static str, args: fmt::Arguments<'_>) {
    match format() {
        LogFormat::Human => eprintln!("{args}"),
        LogFormat::Json => {
            let raw = args.to_string();
            let message = strip_prefix(component, &raw);
            tracing::info!(target: "zoder::op", component, message = %message);
        }
    }
}

/// An operator message: `oplog!("loop", "[loop] iter {i}: ...")`. Prints
/// exactly what `eprintln!` would in human mode; in JSON mode the `[loop] `
/// prefix becomes `"component":"loop"`.
#[macro_export]
macro_rules! oplog {
    ($component:literal, $($arg:tt)*) => {
        $crate::telemetry::emit($component, format_args!($($arg)*))
    };
}

/// One author turn of `zoder loop` (`zoder.turn/1`).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct TurnEvent {
    pub schema: &'static str,
    pub iteration: usize,
    pub phase: &'static str,
    /// `provider=model`; empty when no route was chosen.
    pub route: String,
    pub provider: String,
    pub model: String,
    pub agent_alias: String,
    /// `free`, `subscription`, `metered`, or `unknown` when no row was written.
    pub billing: String,
    /// The engine's own cost estimate; null when it reported none.
    pub estimated_cost_usd: Option<f64>,
    /// The billed cost; null when it is not known.
    pub billed_cost_usd: Option<f64>,
    pub cost_unknown: bool,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// `completed`, `cancelled`, `failed`, `timeout` or `error`.
    pub outcome: String,
    /// `not_needed`, `confirmed`, `unconfirmed` or `no_session`.
    pub cancel_outcome: &'static str,
    /// The `--check` status: 0 passed, 1 failed or killed, null when no check
    /// ran.
    pub exit_code: Option<i32>,
    pub elapsed_ms: u64,
}

/// One zoder process (`zoder.run/1`).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct RunEvent {
    pub schema: &'static str,
    /// The clap subcommand name; `exec` for a bare prompt.
    pub command: String,
    pub exit_code: i32,
    pub duration_ms: u64,
    /// `not_needed`, `confirmed`, `unconfirmed` or `no_session`.
    pub cancel_outcome: &'static str,
    pub sessions_seen: usize,
    pub sessions_settled: usize,
    pub sessions_cancelled: usize,
    pub sessions_unreachable: usize,
    pub deadline_hit: bool,
    pub error: Option<String>,
}

fn emit_json<T: Serialize>(target_turn: bool, event: &T) {
    let Ok(json) = serde_json::to_string(event) else {
        return;
    };
    if target_turn {
        tracing::info!(target: "zoder::turn", json = %json);
    } else {
        tracing::info!(target: "zoder::run", json = %json);
    }
}

/// Emit a turn event. A no-op in human mode.
pub(crate) fn emit_turn(event: &TurnEvent) {
    if format() == LogFormat::Json {
        emit_json(true, event);
    }
}

/// Emit a run event. A no-op in human mode.
pub(crate) fn emit_run(event: &RunEvent) {
    if format() == LogFormat::Json {
        emit_json(false, event);
    }
}

static DEADLINE_HIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static RUN_EMITTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static COMMAND: Mutex<Option<String>> = Mutex::new(None);

/// The loop's whole-run deadline fired.
pub(crate) fn note_deadline_hit() {
    DEADLINE_HIT.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Record the command label for the run event.
pub(crate) fn set_command(command: &str) {
    *COMMAND.lock().unwrap_or_else(|e| e.into_inner()) = Some(command.to_string());
}

/// Emit this process's run event, once. `started` is the process start.
pub(crate) fn finish_run(started: std::time::Instant, exit_code: i32, error: Option<String>) {
    if format() != LogFormat::Json || RUN_EMITTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let report = crate::reaper::cancel_report();
    emit_run(&RunEvent {
        schema: RUN_SCHEMA,
        command: COMMAND
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(|| "exec".to_string()),
        exit_code,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        cancel_outcome: report.outcome,
        sessions_seen: report.seen,
        sessions_settled: report.settled,
        sessions_cancelled: report.cancelled,
        sessions_unreachable: report.unreachable,
        deadline_hit: DEADLINE_HIT.load(std::sync::atomic::Ordering::Relaxed),
        error,
    });
}

/// Emit the run event and exit: for the commands that end the process
/// themselves instead of returning to `main`.
pub(crate) fn exit(code: i32) -> ! {
    finish_run(crate::process_start_instant(), code, None);
    std::process::exit(code);
}

/// A tracing layer that writes one JSON object per event:
/// `ts`, `level`, `target`, then the event's fields. A field named `json`
/// holding a JSON object is merged into the line rather than nested.
pub(crate) struct JsonLayer<W> {
    make_writer: W,
}

impl<W> JsonLayer<W> {
    pub(crate) fn new(make_writer: W) -> Self {
        Self { make_writer }
    }
}

#[derive(Default)]
struct JsonVisitor {
    fields: serde_json::Map<String, serde_json::Value>,
}

impl JsonVisitor {
    fn insert(&mut self, field: &Field, value: serde_json::Value) {
        self.fields.insert(field.name().to_string(), value);
    }
}

impl Visit for JsonVisitor {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.insert(field, serde_json::json!(value));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field, value.into());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field, value.into());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field, value.into());
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert(field, value.into());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.insert(field, format!("{value:?}").into());
    }
}

impl<S, W> Layer<S> for JsonLayer<W>
where
    S: Subscriber,
    W: for<'a> MakeWriter<'a> + 'static,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = JsonVisitor::default();
        event.record(&mut visitor);
        let mut line = serde_json::Map::new();
        line.insert(
            "ts".into(),
            chrono::Utc::now()
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string()
                .into(),
        );
        line.insert("level".into(), event.metadata().level().as_str().into());
        line.insert("target".into(), event.metadata().target().into());
        for (key, value) in visitor.fields {
            if key == "json" {
                if let Some(serde_json::Value::Object(merged)) = value
                    .as_str()
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                {
                    line.extend(merged);
                    continue;
                }
            }
            line.insert(key, value);
        }
        let Ok(mut bytes) = serde_json::to_vec(&serde_json::Value::Object(line)) else {
            return;
        };
        bytes.push(b'\n');
        let _ = self.make_writer.make_writer().write_all(&bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn format_resolution_precedence() {
        let fleet = || true;
        let standalone = || false;
        assert_eq!(
            resolve_format(Some("json"), standalone).format,
            LogFormat::Json
        );
        assert_eq!(
            resolve_format(Some("JSON"), standalone).format,
            LogFormat::Json
        );
        // The variable wins over the deployment mode, both ways.
        for value in ["text", "human", "plain"] {
            assert_eq!(resolve_format(Some(value), fleet).format, LogFormat::Human);
        }
        // Unset or empty: the deployment mode decides.
        assert_eq!(resolve_format(None, fleet).format, LogFormat::Json);
        assert_eq!(resolve_format(None, standalone).format, LogFormat::Human);
        assert_eq!(resolve_format(Some("  "), fleet).format, LogFormat::Json);
        // An unknown value keeps human text, even on a fleet host, and is
        // reported rather than swallowed.
        let unknown = resolve_format(Some("yaml"), fleet);
        assert_eq!(unknown.format, LogFormat::Human);
        assert_eq!(unknown.rejected.as_deref(), Some("yaml"));
        // The config is not read when the variable decides.
        let resolved = resolve_format(Some("json"), || panic!("config must not be read"));
        assert_eq!(resolved.format, LogFormat::Json);
    }

    #[test]
    fn prefix_is_lifted_into_the_component_field() {
        assert_eq!(
            strip_prefix("loop", "[loop] iter 1/3: author…"),
            "iter 1/3: author…"
        );
        assert_eq!(
            strip_prefix("loop", "\n[loop] iter 1/3: author…"),
            "iter 1/3: author…"
        );
        assert_eq!(strip_prefix("zoder", "zoder: error: boom"), "error: boom");
        assert_eq!(
            strip_prefix("zoder", "[zoder] reaped pid=1"),
            "reaped pid=1"
        );
        // A different component's prefix is not stripped.
        assert_eq!(strip_prefix("loop", "[zoder] x"), "[zoder] x");
        // A component that is only a prefix of the word is not stripped.
        assert_eq!(strip_prefix("loop", "[loops] x"), "[loops] x");
        assert_eq!(strip_prefix("loop", "looping: x"), "looping: x");
        assert_eq!(strip_prefix("loop", "no prefix"), "no prefix");
    }

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Buffer {
        type Writer = Buffer;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn json_layer_writes_one_object_per_line_with_merged_event_fields() {
        use tracing_subscriber::prelude::*;
        let buffer = Buffer::default();
        let subscriber = tracing_subscriber::registry().with(JsonLayer::new(buffer.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "zoder::op", component = "loop", message = %"iter 1/1: author…");
            let turn = sample_turn();
            tracing::info!(target: "zoder::turn", json = %serde_json::to_string(&turn).unwrap());
            tracing::warn!(attempts = 3_u64, "plain {} message", "tracing");
        });
        let text = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).expect("every line is one JSON object"))
            .collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert_eq!(lines[0]["target"], "zoder::op");
        assert_eq!(lines[0]["level"], "INFO");
        assert_eq!(lines[0]["component"], "loop");
        assert_eq!(lines[0]["message"], "iter 1/1: author…");
        assert!(lines[0]["ts"].as_str().unwrap().ends_with('Z'));
        // The turn's fields are merged and typed, not nested as a string.
        assert_eq!(lines[1]["schema"], TURN_SCHEMA);
        assert!(lines[1].get("json").is_none(), "{}", lines[1]);
        assert_eq!(lines[1]["tokens_in"], 120);
        assert!(lines[1]["estimated_cost_usd"].is_null());
        assert_eq!(lines[1]["billed_cost_usd"], 0.25);
        assert_eq!(lines[1]["exit_code"], 0);
        assert_eq!(lines[2]["level"], "WARN");
        assert_eq!(lines[2]["message"], "plain tracing message");
        assert_eq!(lines[2]["attempts"], 3);
    }

    fn sample_turn() -> TurnEvent {
        TurnEvent {
            schema: TURN_SCHEMA,
            iteration: 1,
            phase: "author",
            route: "seed=seed-model/one".into(),
            provider: "seed".into(),
            model: "seed-model/one".into(),
            agent_alias: "default".into(),
            billing: "metered".into(),
            estimated_cost_usd: None,
            billed_cost_usd: Some(0.25),
            cost_unknown: false,
            tokens_in: 120,
            tokens_out: 30,
            outcome: "completed".into(),
            cancel_outcome: "not_needed",
            exit_code: Some(0),
            elapsed_ms: 1500,
        }
    }

    fn sample_run() -> RunEvent {
        RunEvent {
            schema: RUN_SCHEMA,
            command: "loop".into(),
            exit_code: 1,
            duration_ms: 2000,
            cancel_outcome: "confirmed",
            sessions_seen: 1,
            sessions_settled: 0,
            sessions_cancelled: 1,
            sessions_unreachable: 0,
            deadline_hit: false,
            error: Some("loop ended unresolved".into()),
        }
    }

    /// The JSON type of every field, so a renamed, removed, added or retyped
    /// field changes the snapshot. Nullable fields are sampled both ways.
    fn shape(value: &serde_json::Value) -> serde_json::Value {
        let serde_json::Value::Object(map) = value else {
            panic!("event is not an object");
        };
        map.iter()
            .map(|(key, value)| {
                let kind = match value {
                    serde_json::Value::Null => "null",
                    serde_json::Value::Bool(_) => "boolean",
                    serde_json::Value::Number(n) if n.is_f64() => "number",
                    serde_json::Value::Number(_) => "integer",
                    serde_json::Value::String(_) => "string",
                    serde_json::Value::Array(_) => "array",
                    serde_json::Value::Object(_) => "object",
                };
                (key.clone(), serde_json::Value::from(kind))
            })
            .collect::<serde_json::Map<_, _>>()
            .into()
    }

    #[test]
    fn turn_and_run_event_schema_snapshot() {
        let mut turn_null = sample_turn();
        turn_null.billed_cost_usd = None;
        turn_null.exit_code = None;
        let mut turn_set = sample_turn();
        turn_set.estimated_cost_usd = Some(0.5);
        let mut run_ok = sample_run();
        run_ok.error = None;
        let actual = serde_json::json!({
            "zoder.turn/1": {
                "set": shape(&serde_json::to_value(&turn_set).unwrap()),
                "null": shape(&serde_json::to_value(&turn_null).unwrap()),
            },
            "zoder.run/1": {
                "set": shape(&serde_json::to_value(sample_run()).unwrap()),
                "null": shape(&serde_json::to_value(&run_ok).unwrap()),
            },
        });
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/telemetry_event_schema.snap.json");
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("reading {}: {e}", path.display())),
        )
        .unwrap();
        assert_eq!(
            actual,
            expected,
            "the telemetry event schema changed; if that is intended, bump the schema \
             version and update {}:\n{}",
            path.display(),
            serde_json::to_string_pretty(&actual).unwrap()
        );
    }
}
