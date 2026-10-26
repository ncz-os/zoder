//! MiniMax M Plan (Token Plan) quota awareness.
//!
//! The M Plan subscription ("Yearly M Plan Explore" et al.) has a rolling
//! 5-hour usage limit and a weekly limit (text/image/audio models are
//! subject to both; video to the weekly only). When either window is
//! exhausted the API rejects every call with HTTP 429:
//!
//! ```json
//! {"type":"rate_limit_error",
//!  "message":"Token Plan usage limit reached: Upgrade your Token Plan or
//!           purchase Credits for more usage. (2056)"}
//! ```
//!
//! Code **2056** is the Token Plan wall. It is NOT transient: retrying into
//! it until the window resets is guaranteed to fail. The 429 itself carries
//! no reset time (no `retry_after` body field, no rate-limit response
//! headers — verified against the live API 2026-10-17), but the plan usage
//! IS queryable:
//!
//! ```text
//! GET https://api.minimax.io/v1/token_plan/remains
//!     Authorization: Bearer <Subscription Key>
//! ```
//!
//! (documented at platform.minimax.io/docs/m-plan/faq, "How can I check
//! M Plan usage? Method 3"; the docs' curl shows a POST to
//! `www.minimax.io`, which 404s — GET on `api.minimax.io` is the shape that
//! works. The body is `{"model_remains":[...],"base_resp":{...}}` with
//! epoch-millisecond window boundaries and per-window remaining percent.)
//!
//! This module:
//! * parses the 429/2056 error body into a distinct [`TokenPlanLimit`]
//!   state (provider + reset time when available);
//! * queries and parses the `token_plan/remains` endpoint;
//! * persists the observed limit state to
//!   `~/.zoder/minimax-quota.json` so routing can skip the provider until
//!   the reset even when no fresh 429 is in flight;
//! * offers `is_exhausted` / `is_exhausted_at` so the router's
//!   subscription-fallback path can fall through to the next configured
//!   route instead of retrying into the wall.
//!
//! The reset time is authoritative only when the API says so: the 429
//! carries none, so a freshly-observed 429/2056 (without a live usage
//! refresh) is treated as "exhausted until proven otherwise" and self-clears
//! after a grace period bounded by the plan's own 5-hour window.

use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

/// The Token Plan wall error code, as it appears in the 429 message
/// ("…purchase Credits for more usage. (2056)").
pub const TOKEN_PLAN_LIMIT_CODE: u32 = 2056;

/// The error envelope `type` MiniMax uses for rate-limit rejections.
pub const RATE_LIMIT_ERROR_TYPE: &str = "rate_limit_error";

/// Grace period after a 429/2056 without a machine-observed reset: keep
/// skipping the provider for at most this long, then let one probe through
/// (the 5-hour window is the longest window the wall can represent, and in
/// practice the wall reset is measured against it).
pub const DEFAULT_EXHAUSTED_GRACE_SECS: u64 = 5 * 60 * 60;

/// Extract the trailing numeric code from a MiniMax rate-limit message:
/// `"Token Plan usage limit reached: … (2056)"` -> `Some(2056)`.
/// Returns `None` when the message does not end in `( <number> )`.
pub fn parse_limit_code(message: &str) -> Option<u32> {
    let m = message.trim();
    let open = m.rfind('(')?;
    let close = m.rfind(')')?;
    if close != m.len() - 1 || open >= close {
        return None;
    }
    m[open + 1..close].trim().parse::<u32>().ok()
}

/// Whether an error body is the MiniMax Token Plan wall (429,
/// `rate_limit_error`, code 2056). Accepts both envelope shapes the API
/// emits: the raw `{ "type": ..., "message": ... }` and the OpenAI-compat
/// wrapper `{ "error": { "type": ..., "message": ... } }`.
pub fn is_token_plan_limit_error(body: &str) -> bool {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let inner = v.get("error").unwrap_or(&v);
    let is_rl = inner
        .get("type")
        .and_then(|t| t.as_str())
        .is_some_and(|t| t == RATE_LIMIT_ERROR_TYPE);
    let msg = inner.get("message").and_then(|m| m.as_str()).unwrap_or("");
    is_rl && parse_limit_code(msg).is_some_and(|c| c == TOKEN_PLAN_LIMIT_CODE)
}

/// Parsed state of a Token Plan limit event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenPlanLimit {
    /// Provider id from the zoder config (e.g. `"minimax"`).
    pub provider_id: String,
    /// The wall was observed at this instant (from a 429 response).
    pub observed_at: DateTime<Utc>,
    /// When the exhausted window resets. `None` when the API did not say
    /// (the 429 body carries no reset time) — routing then falls back to
    /// the grace period of [`DEFAULT_EXHAUSTED_GRACE_SECS`].
    pub resets_at: Option<DateTime<Utc>>,
}

impl TokenPlanLimit {
    pub fn new(provider_id: impl Into<String>, observed_at: DateTime<Utc>) -> Self {
        Self {
            provider_id: provider_id.into(),
            observed_at,
            resets_at: None,
        }
    }

    /// Whether the provider is still exhausted at `now`: until the
    /// machine-observed reset, or — absent one — until the grace period
    /// after the observation elapses.
    pub fn is_exhausted_at(&self, now: DateTime<Utc>) -> bool {
        match self.resets_at {
            Some(reset) => now < reset,
            None => {
                now - self.observed_at
                    < chrono::TimeDelta::seconds(DEFAULT_EXHAUSTED_GRACE_SECS as i64)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// token_plan/remains usage endpoint
// ---------------------------------------------------------------------------

/// One row of the `GET /v1/token_plan/remains` response (model `general`):
/// the rolling 5-hour window plus the weekly window for the subscription.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenPlanRemains {
    /// 5h window start, epoch milliseconds.
    pub start_time: i64,
    /// 5h window end (the 5-hour reset), epoch milliseconds.
    pub end_time: i64,
    /// Milliseconds remaining in the 5h window.
    pub remains_time: i64,
    /// The model bucket this row covers (the live API returns `"general"`).
    /// Optional: early fixtures may omit it.
    #[serde(default)]
    pub model_name: Option<String>,
    /// 5h window quota, in tokens.
    pub current_interval_total_count: i64,
    /// Tokens consumed in the current 5h window.
    pub current_interval_usage_count: i64,
    /// Weekly quota, in tokens.
    pub current_weekly_total_count: i64,
    /// Tokens consumed in the current weekly window.
    pub current_weekly_usage_count: i64,
    /// Weekly window start, epoch milliseconds.
    pub weekly_start_time: i64,
    /// Weekly window end (the weekly reset), epoch milliseconds.
    pub weekly_end_time: i64,
    /// Milliseconds remaining in the weekly window.
    pub weekly_remains_time: i64,
    /// 5h window status (1 = active).
    pub current_interval_status: i64,
    /// Percent of the 5h quota REMAINING (100 = full headroom). An absent
    /// field reads as full headroom, never as an exhausted window.
    #[serde(default = "full_headroom_percent")]
    pub current_interval_remaining_percent: i64,
    /// Weekly window status (1 = active).
    pub current_weekly_status: i64,
    /// Percent of the weekly quota REMAINING (100 = full headroom). An
    /// absent field reads as full headroom.
    #[serde(default = "full_headroom_percent")]
    pub current_weekly_remaining_percent: i64,
}

fn full_headroom_percent() -> i64 {
    100
}

impl TokenPlanRemains {
    /// Fraction (0..=1) of the 5-hour quota consumed. The live API often
    /// reports zero token counts and only the remaining percent, so the
    /// percent is used whenever there is no token cap.
    pub fn interval_used_pct(&self) -> f64 {
        used_fraction(
            self.current_interval_usage_count,
            self.current_interval_total_count,
            self.current_interval_remaining_percent,
        )
    }

    /// Fraction (0..=1) of the weekly quota consumed (same fallback).
    pub fn weekly_used_pct(&self) -> f64 {
        used_fraction(
            self.current_weekly_usage_count,
            self.current_weekly_total_count,
            self.current_weekly_remaining_percent,
        )
    }

    /// True when the 5-hour window is full.
    pub fn interval_exhausted(&self) -> bool {
        self.interval_used_pct() >= 1.0
    }

    /// True when the weekly window is full.
    pub fn weekly_exhausted(&self) -> bool {
        self.weekly_used_pct() >= 1.0
    }

    /// When the 5-hour window resets, per the API.
    pub fn interval_reset_at(&self) -> DateTime<Utc> {
        epoch_ms_to_utc(self.end_time)
    }

    /// When the weekly window resets, per the API.
    pub fn weekly_reset_at(&self) -> DateTime<Utc> {
        epoch_ms_to_utc(self.weekly_end_time)
    }

    /// True when either window is (or is past) its full quota: the next
    /// text call will hit the 429/2056 wall.
    pub fn is_exhausted(&self) -> bool {
        self.interval_exhausted() || self.weekly_exhausted()
    }

    /// When the wall lifts, per the API's window boundaries: the 5-hour
    /// reset when only the 5-hour window is full, the weekly reset when the
    /// weekly window is full. A full 5-hour window says nothing about the
    /// weekly one, so it must not hold the provider until the weekly reset.
    /// `None` when neither window is full.
    pub fn exhausted_until(&self) -> Option<DateTime<Utc>> {
        let interval = self.interval_exhausted().then(|| self.interval_reset_at());
        let weekly = self.weekly_exhausted().then(|| self.weekly_reset_at());
        interval.into_iter().chain(weekly).max()
    }

    /// Like [`Self::exhausted_until`], but measured from `base` with the
    /// `remains_time` fields instead of the absolute window ends.
    fn exhausted_until_from(&self, base: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let interval = self
            .interval_exhausted()
            .then(|| base + chrono::TimeDelta::milliseconds(self.remains_time));
        let weekly = self
            .weekly_exhausted()
            .then(|| base + chrono::TimeDelta::milliseconds(self.weekly_remains_time));
        interval.into_iter().chain(weekly).max()
    }
}

/// Consumed fraction from token counts, or from the remaining percent when
/// the API reports no token cap.
fn used_fraction(used: i64, total: i64, remaining_percent: i64) -> f64 {
    if total > 0 {
        return used as f64 / total as f64;
    }
    (100 - remaining_percent.clamp(0, 100)) as f64 / 100.0
}

fn epoch_ms_to_utc(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(
        ms.div_euclid(1000),
        (ms.rem_euclid(1000) * 1_000_000) as u32,
    )
    .single()
    .unwrap_or_else(|| Utc.timestamp_opt(0, 0).single().expect("epoch is valid"))
}

/// The full `GET /v1/token_plan/remains` envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenPlanRemainsResponse {
    pub model_remains: Vec<TokenPlanRemains>,
    #[serde(default)]
    pub base_resp: TokenPlanBaseResp,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenPlanBaseResp {
    #[serde(default)]
    pub status_code: i64,
    #[serde(default)]
    pub status_msg: String,
}

/// Parse a recorded (or live) `token_plan/remains` body.
pub fn parse_token_plan_remains(body: &str) -> anyhow::Result<TokenPlanRemainsResponse> {
    let v: TokenPlanRemainsResponse = serde_json::from_str(body)
        .map_err(|e| anyhow::anyhow!("malformed token_plan/remains body: {e}"))?;
    if v.base_resp.status_code != 0 {
        anyhow::bail!(
            "token_plan/remains base_resp failure: {} ({})",
            v.base_resp.status_code,
            v.base_resp.status_msg
        );
    }
    Ok(v)
}

/// The path of the MiniMax plan-usage endpoint, relative to the API base
/// URL (`https://api.minimax.io`), i.e. `GET {base}/v1/token_plan/remains`.
pub const REMAINS_ENDPOINT_PATH: &str = "v1/token_plan/remains";

// ---------------------------------------------------------------------------
// persisted quota state
// ---------------------------------------------------------------------------

/// On-disk state for one MiniMax provider: the last observed limit event
/// plus the freshest machine-observed window reset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ProviderQuotaState {
    /// Last 429/2056 observation.
    pub limit: Option<TokenPlanLimit>,
    /// Last live `token_plan/remains` refresh (authoritative reset times).
    pub last_remains: Option<TokenPlanRemains>,
    /// When the last refresh happened.
    pub refreshed_at: Option<DateTime<Utc>>,
}

impl ProviderQuotaState {
    /// Whether this provider must be skipped in routing at `now`.
    ///
    /// Precedence: a limit event with an explicit reset time wins; then a
    /// live usage refresh that shows a full window; then a limit event
    /// without a reset time, honored for the grace period.
    ///
    /// The relief is the reset of the window that is full: the 5-hour reset
    /// for a full 5-hour window, the weekly reset for a full weekly window
    /// (the later of the two when both are full).
    pub fn is_exhausted_at(&self, now: DateTime<Utc>) -> bool {
        if let Some(limit) = &self.limit {
            if let Some(reset) = limit.resets_at {
                return now < reset;
            }
        }
        if let (Some(remains), Some(base)) = (&self.last_remains, self.refreshed_at) {
            // Anchor the relief to the time the window was LAST SEEN full
            // (the refresh time) plus the window's remaining time: the
            // honest answer to "how long is the wall up" is "until that
            // window rolls over from when we last saw it full". Without a
            // refresh time there is no anchor, and the limit/grace logic
            // below decides.
            if let Some(relief) = remains.exhausted_until_from(base) {
                if now < relief {
                    return true;
                }
            }
        }
        match &self.limit {
            Some(limit) if limit.resets_at.is_none() => limit.is_exhausted_at(now),
            _ => false,
        }
    }
}

/// The whole on-disk store: one entry per MiniMax provider id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct QuotaState {
    #[serde(default)]
    pub providers: std::collections::BTreeMap<String, ProviderQuotaState>,
}

impl QuotaState {
    pub fn provider(&self, id: &str) -> Option<&ProviderQuotaState> {
        self.providers.get(id)
    }

    /// Record a fresh 429/2056 observation. `resets_at` is filled from a
    /// concurrent live usage read when one is available.
    pub fn record_limit(&mut self, id: &str, limit: TokenPlanLimit) {
        let state = self.providers.entry(id.to_string()).or_default();
        state.limit = Some(limit);
    }

    /// Record a fresh `token_plan/remains` refresh.
    pub fn record_remains(&mut self, id: &str, remains: TokenPlanRemains, now: DateTime<Utc>) {
        let state = self.providers.entry(id.to_string()).or_default();
        state.last_remains = Some(remains);
        state.refreshed_at = Some(now);
    }

    /// The wall horizon from `id`'s last live refresh, if any: the reset of
    /// the full window, or the 5-hour reset when the refresh shows no full
    /// window (the counters can lag the wall; the 5-hour window is the
    /// shortest horizon that can lift it). Only `id`'s own entry counts:
    /// another account's reset says nothing about this one.
    pub fn observed_reset_at(&self, id: &str) -> Option<DateTime<Utc>> {
        self.provider(id)?
            .last_remains
            .as_ref()
            .map(|r| r.exhausted_until().unwrap_or_else(|| r.interval_reset_at()))
    }

    pub fn is_exhausted(&self, id: &str) -> bool {
        self.provider(id)
            .map(|s| s.is_exhausted_at(Utc::now()))
            .unwrap_or(false)
    }
}

/// Default store path: `<zoder home>/minimax-quota.json`. Returns `None`
/// (like [`crate::utilization::default_store_path`]) when the home dir
/// cannot be resolved, so callers treat absence as "no persisted state".
pub fn default_state_path() -> Option<std::path::PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".zoder").join("minimax-quota.json"))
}

/// Load the persisted state; a missing file yields an empty store, and a
/// corrupt file is quarantined (not silently deleted) so it can be
/// inspected, matching the health-store convention.
pub fn load_state(path: &Path) -> QuotaState {
    match std::fs::read_to_string(path) {
        Ok(s) => match serde_json::from_str(&s) {
            Ok(st) => st,
            Err(e) => {
                eprintln!(
                    "zoder: warning: minimax quota state {} is corrupt ({e}); ignoring",
                    path.display()
                );
                let backup =
                    path.with_extension(format!("json.corrupt.{}", Utc::now().timestamp()));
                let _ = std::fs::rename(path, &backup);
                QuotaState::default()
            }
        },
        Err(_) => QuotaState::default(),
    }
}

// ---------------------------------------------------------------------------
// live observation (called from the provider layer on a 429/2056)
// ---------------------------------------------------------------------------

use crate::config::Auth;

/// Short budget for the best-effort live refresh performed when the wall is
/// hit: it runs on the error path of a chat call, so its ceiling is added
/// latency on every failure-recovery turn. Keep it tight (2s) — the
/// persisted grace-bound limit already gives routing what it needs, and the
/// dedicated `zoder quota minimax` command does the authoritative 10s fetch
/// off the hot path.
const REMAINS_REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

fn state_path_or_default() -> Option<std::path::PathBuf> {
    default_state_path()
}

/// Best-effort `GET {api base}/v1/token_plan/remains` with the provider's
/// bearer credential. Returns the freshest `general` row, or `None` when the
/// endpoint is unreachable, unauthenticated, or shaped differently. Never
/// fails the caller: this runs on the error path of a chat call.
pub async fn fetch_remains(
    base_url: &str,
    auth: &Auth,
) -> Option<(TokenPlanRemains, DateTime<Utc>)> {
    let token = auth.resolve()?;
    // The endpoint is versioned at the API host (`https://api.minimax.io`);
    // configured bases may or may not already carry the `/v1` prefix.
    let base = base_url.trim_end_matches('/');
    let url = if base.ends_with("/v1") {
        format!("{base}/token_plan/remains")
    } else {
        format!("{base}/v1/token_plan/remains")
    };
    let client = reqwest::Client::builder()
        .timeout(REMAINS_REFRESH_TIMEOUT)
        .build()
        .ok()?;
    let resp = client.get(&url).bearer_auth(token).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    let parsed = parse_token_plan_remains(&body).ok()?;
    // The live response carries one `general` row (recorded 2026-10-17);
    // take the first row.
    parsed
        .model_remains
        .into_iter()
        .next()
        .map(|r| (r, Utc::now()))
}

/// The wall horizon recorded by the most recent successful live refresh for
/// `provider_id`, if any (see [`QuotaState::observed_reset_at`]).
pub fn observed_reset_at(provider_id: &str) -> Option<DateTime<Utc>> {
    let path = state_path_or_default()?;
    load_state(&path).observed_reset_at(provider_id)
}

/// Record a fresh 429/2056 observation for `provider_id`: persist the limit
/// event (grace-bound when no reset is known) and, best-effort, refresh the
/// live `token_plan/remains` so the persisted reset time is machine-observed
/// rather than grace-bound. Returns the recorded limit.
///
/// This is best-effort by contract: any IO/parse failure still records the
/// grace-bound limit so routing skips the wall, and the chat call's fallback
/// is never blocked on the refresh.
pub async fn observe_token_plan_limit(
    provider_id: &str,
    base_url: &str,
    auth: &Auth,
) -> Option<TokenPlanLimit> {
    let now = Utc::now();
    let path = state_path_or_default()?;
    // Best-effort live refresh FIRST so the persisted limit can carry a
    // machine-observed reset time when the endpoint answers. It is awaited
    // on the caller's runtime (bounded by REMAINS_REFRESH_TIMEOUT), never on
    // a blocking thread.
    let refreshed = fetch_remains(base_url, auth).await;

    let mut state = load_state(&path);
    if let Some((remains, at)) = &refreshed {
        state.record_remains(provider_id, remains.clone(), *at);
        // The wall just fired, so at least one window is full; when the
        // endpoint's counters agree, anchor the limit to the machine-observed
        // reset instead of the grace period.
        if let Some(reset) = remains.exhausted_until() {
            let mut limit = TokenPlanLimit::new(provider_id, now);
            limit.resets_at = Some(reset);
            state.record_limit(provider_id, limit);
            let _ = save_state(&path, &state);
            return state.provider(provider_id).and_then(|p| p.limit.clone());
        }
    }
    let limit = TokenPlanLimit::new(provider_id, now);
    state.record_limit(provider_id, limit.clone());
    let _ = save_state(&path, &state);
    Some(limit)
}

/// Atomically persist the state.
pub fn save_state(path: &Path, state: &QuotaState) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let data = serde_json::to_string_pretty(state)?;
    let tmp = path.with_extension(format!(
        "json.tmp.{}.{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::write(&tmp, data).and_then(|_| std::fs::rename(&tmp, path))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded live response from
    /// `GET https://api.minimax.io/v1/token_plan/remains`, 2026-10-17
    /// 06:40 UTC (values: 5h window used 47%, weekly used 46%).
    const RECORDS_REMAINS_BODY: &str = r#"{
  "model_remains": [
    {
      "start_time": 1792217402790,
      "end_time": 1792235402790,
      "remains_time": 16199415,
      "current_interval_total_count": 0,
      "current_interval_usage_count": 0,
      "model_name": "general",
      "current_weekly_total_count": 0,
      "current_weekly_usage_count": 0,
      "weekly_start_time": 1792080658973,
      "weekly_end_time": 1792685458973,
      "weekly_remains_time": 466255598,
      "current_interval_status": 1,
      "current_interval_remaining_percent": 53,
      "current_weekly_status": 1,
      "current_weekly_remaining_percent": 54
    }
  ],
  "base_resp": { "status_code": 0, "status_msg": "success" }
}"#;

    /// The exact 429 body observed on the 2026-10-17 04:53Z quota wall.
    const RECORDS_429_BODY: &str = r#"{"type":"rate_limit_error","message":"Token Plan usage limit reached: Upgrade your Token Plan or purchase Credits for more usage. (2056)"}"#;

    #[test]
    fn parses_the_recorded_429_token_plan_wall() {
        assert!(
            is_token_plan_limit_error(RECORDS_429_BODY),
            "the recorded 429/2056 body must be recognized as the plan wall"
        );
        let code = parse_limit_code(
            "Token Plan usage limit reached: Upgrade your Token Plan or purchase Credits for more usage. (2056)",
        );
        assert_eq!(code, Some(TOKEN_PLAN_LIMIT_CODE));
    }

    #[test]
    fn generic_rate_limits_are_not_plan_walls() {
        // A plain throttling 429 (no 2056 code) must NOT be treated as a
        // wall — it is transient and retryable.
        let plain = r#"{"type":"rate_limit_error","message":"Requests rate limit exceeded. Please try again later."}"#;
        assert!(!is_token_plan_limit_error(plain));
        let wrong_code = r#"{"type":"rate_limit_error","message":"Too many tokens. (1002)"}"#;
        assert!(!is_token_plan_limit_error(wrong_code));
        let not_json = "429 Too Many Requests";
        assert!(!is_token_plan_limit_error(not_json));
        let wrong_type = r#"{"type":"api_error","message":"quota exceeded (2056)"}"#;
        assert!(!is_token_plan_limit_error(wrong_type));
    }

    #[test]
    fn parses_the_openai_compat_error_wrapper() {
        let wrapped = format!("{{\"error\":{}}}", RECORDS_429_BODY);
        assert!(
            is_token_plan_limit_error(&wrapped),
            "the OpenAI-compat `error` envelope must also be recognized"
        );
    }

    #[test]
    fn parses_the_recorded_remains_payload() {
        let resp = parse_token_plan_remains(RECORDS_REMAINS_BODY).expect("recorded body parses");
        assert_eq!(resp.model_remains.len(), 1);
        let r = &resp.model_remains[0];
        // The recorded capture had zero token counts (percent-only fields
        // were the live signal): usage comes from the remaining percent
        // (53% and 54% left), and the math stays 0/0-safe.
        assert!((r.interval_used_pct() - 0.47).abs() < 1e-9);
        assert!((r.weekly_used_pct() - 0.46).abs() < 1e-9);
        assert!(!r.is_exhausted());
        // Millisecond-precision window boundaries from the recorded
        // epoch-millisecond fields.
        assert_eq!(
            r.interval_reset_at(),
            Utc.with_ymd_and_hms(2026, 10, 17, 11, 10, 2).unwrap()
                + chrono::TimeDelta::milliseconds(790)
        );
        assert_eq!(
            r.weekly_reset_at(),
            Utc.with_ymd_and_hms(2026, 10, 22, 16, 10, 58).unwrap()
                + chrono::TimeDelta::milliseconds(973)
        );
    }

    #[test]
    fn remains_full_window_is_exhausted_until_that_window_resets() {
        let mut r = parse_token_plan_remains(RECORDS_REMAINS_BODY)
            .expect("recorded body parses")
            .model_remains
            .pop()
            .unwrap();
        // 5h window full (100/100), weekly not. The relief is anchored to the
        // refresh time plus the 5h window's remaining time.
        r.current_interval_total_count = 100;
        r.current_interval_usage_count = 100;
        r.remains_time = 16_200; // 16.2s until the 5h window rolls over
        assert!(r.is_exhausted());
        assert!(r.interval_exhausted() && !r.weekly_exhausted());

        // Fix the clock at the recorded refresh instant so the test is
        // deterministic (the recorded `remains_time` fields are measured
        // from that instant, not from the wall-clock "now").
        let refreshed = Utc.with_ymd_and_hms(2026, 10, 17, 6, 40, 0).unwrap();
        let state = ProviderQuotaState {
            last_remains: Some(r.clone()),
            refreshed_at: Some(refreshed),
            ..Default::default()
        };
        assert!(
            state.is_exhausted_at(refreshed),
            "full 5h window is exhausted at the refresh instant"
        );
        assert!(
            state.is_exhausted_at(refreshed + chrono::TimeDelta::milliseconds(16_100)),
            "still exhausted 100ms before the 5h window rolls over"
        );
        // A full 5h window says nothing about the weekly one: the wall lifts
        // when the 5h window rolls over, not at the weekly reset days later.
        assert!(
            !state.is_exhausted_at(refreshed + chrono::TimeDelta::milliseconds(16_300)),
            "relieved just after the 5h window rolls over"
        );
        assert_eq!(r.exhausted_until(), Some(r.interval_reset_at()));

        // A weekly-full window is exhausted at the refresh instant and
        // relieves at the weekly reset.
        let weekly_relief = refreshed + chrono::TimeDelta::milliseconds(466_255_598);
        let mut r2 = r.clone();
        r2.current_interval_usage_count = 0;
        r2.current_weekly_usage_count = 100;
        r2.current_weekly_total_count = 100;
        r2.weekly_remains_time = 466_255_598; // ~5.4 days, from the record
        let state2 = ProviderQuotaState {
            last_remains: Some(r2.clone()),
            refreshed_at: Some(refreshed),
            ..Default::default()
        };
        assert!(
            state2.is_exhausted_at(refreshed),
            "full weekly window is exhausted at the refresh instant"
        );
        assert!(state2.is_exhausted_at(weekly_relief - chrono::TimeDelta::seconds(1)));
        assert!(!state2.is_exhausted_at(weekly_relief + chrono::TimeDelta::seconds(1)));
        assert_eq!(r2.exhausted_until(), Some(r2.weekly_reset_at()));
    }

    /// The live endpoint reports zero token counts and only the remaining
    /// percent. A 5h window at 0% remaining with the weekly at 70% must read
    /// as exhausted, and only until the 5h reset.
    #[test]
    fn percent_only_remains_detects_the_wall_per_window() {
        let mut r = parse_token_plan_remains(RECORDS_REMAINS_BODY)
            .expect("recorded body parses")
            .model_remains
            .pop()
            .unwrap();
        assert_eq!(r.current_interval_total_count, 0);
        r.current_interval_remaining_percent = 0;
        r.current_weekly_remaining_percent = 70;
        assert!(r.interval_exhausted());
        assert!(!r.weekly_exhausted());
        assert_eq!(r.interval_used_pct(), 1.0);
        assert!((r.weekly_used_pct() - 0.3).abs() < 1e-9);
        assert_eq!(r.exhausted_until(), Some(r.interval_reset_at()));

        let mut state = QuotaState::default();
        state.record_remains("minimax", r.clone(), Utc::now());
        assert_eq!(
            state.observed_reset_at("minimax"),
            Some(r.interval_reset_at())
        );
    }

    /// A remains row without the percent fields must not read as a full
    /// window.
    #[test]
    fn missing_percent_fields_mean_headroom() {
        let body = r#"{"model_remains":[{"start_time":0,"end_time":1,"remains_time":1,
            "current_interval_total_count":0,"current_interval_usage_count":0,
            "current_weekly_total_count":0,"current_weekly_usage_count":0,
            "weekly_start_time":0,"weekly_end_time":1,"weekly_remains_time":1,
            "current_interval_status":1,"current_weekly_status":1}],
            "base_resp":{"status_code":0,"status_msg":"success"}}"#;
        let r = parse_token_plan_remains(body)
            .unwrap()
            .model_remains
            .pop()
            .unwrap();
        assert!(!r.is_exhausted());
        assert_eq!(r.exhausted_until(), None);
    }

    #[test]
    fn observed_reset_is_scoped_to_the_provider() {
        let r = parse_token_plan_remains(RECORDS_REMAINS_BODY)
            .expect("recorded body parses")
            .model_remains
            .pop()
            .unwrap();
        let refreshed = Utc.with_ymd_and_hms(2026, 10, 17, 6, 40, 0).unwrap();
        let mut state = QuotaState::default();
        state.record_remains("minimax-a", r.clone(), refreshed);
        // The recorded refresh shows no full window, so the horizon is the
        // shortest one that can lift a wall: the 5h reset.
        assert_eq!(
            state.observed_reset_at("minimax-a"),
            Some(r.interval_reset_at())
        );
        // A second account, or a non-MiniMax provider that got a plain 429,
        // must not inherit another account's reset as its wall.
        assert_eq!(state.observed_reset_at("minimax-b"), None);
        assert_eq!(state.observed_reset_at("other-provider"), None);
    }

    #[test]
    fn full_window_without_a_refresh_time_is_not_anchored_to_the_clock() {
        let mut r = parse_token_plan_remains(RECORDS_REMAINS_BODY)
            .expect("recorded body parses")
            .model_remains
            .pop()
            .unwrap();
        r.current_interval_total_count = 100;
        r.current_interval_usage_count = 100;
        assert!(r.is_exhausted());
        let state = ProviderQuotaState {
            last_remains: Some(r),
            refreshed_at: None,
            ..Default::default()
        };
        let now = Utc.with_ymd_and_hms(2026, 10, 17, 6, 40, 0).unwrap();
        assert!(
            !state.is_exhausted_at(now),
            "with no refresh time there is no anchor; the answer must not \
             depend on the wall clock"
        );
    }

    #[test]
    fn limit_event_without_reset_uses_grace_period() {
        let observed = Utc.with_ymd_and_hms(2026, 10, 17, 4, 53, 0).unwrap();
        let limit = TokenPlanLimit::new("minimax", observed);
        assert!(limit.is_exhausted_at(observed + chrono::Duration::minutes(10)));
        assert!(limit.is_exhausted_at(observed + chrono::Duration::hours(4)));
        assert!(!limit
            .is_exhausted_at(observed + chrono::Duration::hours(5) + chrono::Duration::seconds(1)));
    }

    #[test]
    fn limit_event_with_explicit_reset_wins_over_grace() {
        let observed = Utc.with_ymd_and_hms(2026, 10, 17, 4, 53, 0).unwrap();
        let reset = observed + chrono::Duration::minutes(30);
        let mut state = QuotaState::default();
        let mut limit = TokenPlanLimit::new("minimax", observed);
        limit.resets_at = Some(reset);
        state.record_limit("minimax", limit);

        // Before the reset: exhausted.
        let before_reset = reset - chrono::Duration::minutes(1);
        assert!(state
            .provider("minimax")
            .unwrap()
            .is_exhausted_at(before_reset));
        // Past the reset: not exhausted, even though the observation is
        // still well inside the 5h grace period — the explicit reset must
        // beat the grace.
        let past = reset + chrono::Duration::hours(3);
        assert!(
            !state.provider("minimax").unwrap().is_exhausted_at(past),
            "an explicit reset must beat the 5h grace period"
        );
    }

    #[test]
    fn base_resp_failure_is_an_error() {
        let body =
            r#"{"model_remains":[],"base_resp":{"status_code":1004,"status_msg":"auth failed"}}"#;
        assert!(parse_token_plan_remains(body).is_err());
    }

    #[test]
    fn state_round_trips_through_json() {
        let mut state = QuotaState::default();
        state.record_limit("minimax", TokenPlanLimit::new("minimax", Utc::now()));
        let s = serde_json::to_string(&state).unwrap();
        let back: QuotaState = serde_json::from_str(&s).unwrap();
        assert_eq!(state, back);
    }
}
