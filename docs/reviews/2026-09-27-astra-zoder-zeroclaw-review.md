# zoder / zeroclaw review — 2026-09-27

Reviewed zoder `432c0814201587fd6667993db8ad82f0eecf172f` (the local `master` and starting branch HEAD) against the build-time checkout `.zeroclaw-src` at `24e7324dc69f525797fc46d80aa0b439d0b418ae`. Upstream paths below refer to that exact checkout, which is not tracked by zoder. No upstream changes, fetches, or pushes were made.

**Result: 9 CONFIRMED findings, 0 SUSPECTED findings.** CONFIRMED means the failure follows from the inspected implementation; it does not imply a live-provider reproduction. This is a targeted review of routing, provider classification, catalog/RPC contracts, corpus publication, and agent lifecycle, not an exhaustive audit of every Rust module.

The supplied bare-ID/unbenched-prior defect is accepted as the already-root-caused exemplar and is not counted or re-derived. The recurring problem here is treating missing evidence as positive evidence: partial output as completion, an absent baseline as acceptable freshness, missing latency as competitive rank, or hidden reasoning as an answer.

**Change first:** require an explicit successful terminal event before an agent run can report `completed` (F1). Preserve partial work with an `interrupted` outcome. This is a small local change with broad impact on automation, exit status, and health learning.

Ranked by impact per implementation risk:

| Rank | Finding | Main impact | Change risk |
|---|---|---|---|
| F1 | Disconnected streams become successful runs | Incomplete coding work reported complete | Low; update partial-outcome contract/tests |
| F2 | Per-model quota limits are counted and applied provider-wide | Healthy subscription models demoted to metered routes | Medium; thread model scope through usage and selection |
| F3 | CI never retains its freshness baseline | Large corpus regressions remain publishable | Low; persist baseline and verify clean-checkout workflow |
| F4 | Live catalog client does not match upstream RPC | Enrichment never works against this engine | Medium; correct both request and response contracts |
| F5 | Explicit session resume errors silently create a new session | Lost context and unintended fresh execution | Low; distinguish explicit resume from optional reuse |
| F6 | Malformed tool-protocol exhaustion returns success upstream | No tools execute, but turn succeeds with fallback prose | Medium; propagate typed failure across engine boundary |
| F7 | Build follows moving upstream without a zoder compatibility gate | Identical zoder revision produces different engine behavior | Low–medium; pin, record, and test the engine revision |
| F8 | Non-streaming reasoning-only reply passes answer validation | Empty answer accepted; fallback chain stops | Low; validate delivered answer independently of reasoning |
| F9 | Missing latency can improve Auto rank | Data loss promotes a model over a measured peer | Low code risk; requires an explicit ranking policy |

**F1 — CONFIRMED: EOF before completion can report success**

Evidence: the EOF branch calls the partial classifier, which returns `completed` for any nonempty text with no outstanding tool result. Success is then exactly that string.

`crates/acp-client/src/lib.rs:2513–2515`:

```rust
            Ok(Ok(false)) => {
                // Mid-turn disconnect (EOF before `turn_complete`). The engine
                // already streamed whatever it streamed — preserve the partial
```

`crates/acp-client/src/lib.rs:2550–2551`:

```rust
                let outcome = classify_partial_outcome(&pending_tool_results, &content, tool_calls);
                break outcome;
```

`crates/acp-client/src/lib.rs:2053–2059`:

```rust
    if pending_tool_results.values().any(|n| *n > 0) {
        "interrupted".to_string()
    } else if content.is_empty() && tool_call_count == 0 {
        "failed".to_string()
    } else {
        "completed".to_string()
    }
```

`crates/acp-client/src/lib.rs:941–944`:

```rust
impl AgentRun {
    pub fn succeeded(&self) -> bool {
        self.outcome == "completed"
    }
```

`crates/zoder-cli/src/main.rs:8772–8774`:

```rust
    if run.succeeded() && policy_violation.is_none() && model_mismatch.is_none() {
        health.record_success(&model_used, elapsed_ms);
    } else {
```

Scenario: the daemon emits “I will inspect the repository”, then crashes before issuing a tool call or `turn_complete`. The client has nonempty content and no pending tools, so it returns a successful run. With ordinary matching route/telemetry, zoder records model health success; the exec failure gate is bypassed (`crates/zoder-cli/src/main.rs:8910–8932`). A completed read tool followed by a disconnect before the planned edit has the same problem. Preserving partial text is correct; inferring completion from it is not.

Change: classify every pre-terminal EOF/read failure as interrupted (or failed if no work arrived), retaining text, session ID, and tool state. Only an explicit successful terminal event should yield completed. Validate with a mock daemon that emits one text chunk, closes the socket, and produces a nonzero CLI exit while preserving the partial transcript. Existing tests explicitly expect text-only EOF success (`crates/acp-client/src/lib.rs:12303`); revise the contract rather than adding a contradictory test.

**F2 — CONFIRMED: declared per-model quota windows do not constrain counting or exhaustion to that model**

`crates/zoder-core/src/config.rs:216–222`:

```rust
    /// Model-id glob patterns this window limits. `None` means "all models on
    /// the provider" (the legacy single-cap shape). Set to a list of globs
    /// (e.g. `["MiniMax-M3", "claude-opus-*"]`) to express a per-model cap —
    /// Anthropic publishes Sonnet / Opus / Haiku as separately-capped models
    /// on the same endpoint, and that's what this field is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
```

`crates/zoder-core/src/quota.rs:116–121`:

```rust
    for e in entries
        .iter()
        .filter(|e| e.provider == provider_id && e.ts_utc <= now && in_active_window(e.ts_utc))
    {
        used += unit_amount(e, w.unit);
        oldest = Some(oldest.map_or(e.ts_utc, |o| o.min(e.ts_utc)));
```

`crates/zoder-core/src/config.rs:1942–1951`:

```rust
    let usage = crate::quota::plan_usage_for_catalog_provider(
        entries,
        &p.id,
        plan,
        catalog,
        &catalog_provider,
    );
    if usage.is_empty() {
        return false;
    }
```

`crates/zoder-core/src/config.rs:1959`:

```rust
    usage.iter().any(|w| w.pct >= 1.0)
```

`crates/zoder-core/src/config.rs:1891–1895`:

```rust
enum BillingTier {
    Free = 0,
    SubscriptionLive = 1,
    Metered = 2,
    SubscriptionExhausted = 3,
```

`crates/zoder-core/src/config.rs:2744–2748`:

```rust
                Some(RankedProvider {
                    provider: p,
                    prefix_len: best_prefix_len,
                    billing_tier: billing_tier(p, entries, catalog),
                })
```

Scenario: one subscription serves Opus and Sonnet; its Opus-only window has `models = ["claude-opus-*"]`, cap 10 requests, and ten recent ledger calls are all Sonnet. `window_usage_at` counts all ten against Opus. The provider is now SubscriptionExhausted, ranked behind its Metered sibling, even though Opus usage is zero. Separately, ten actual Opus calls also demote Sonnet: exhaustion receives no requested model and applies any saturated window to the entire provider.

Impact is wrong route selection and avoidable paid confirmation or metered spend when paid use is authorized—not a demonstrated paid-policy bypass. The provider's paid gate remains relevant.

Change: filter ledger contributions by each window's model patterns, then consider only windows applicable to the requested model when ranking providers. Preserve provider-wide windows when `models` is absent. Test both directions above plus a shared provider-wide cap. Merely fixing the counter leaves the second failure intact.

**F3 — CONFIRMED: corpus freshness protection loses its baseline between clean CI jobs**

`.gitlab-ci.yml:28–32`:

```text
corpus-sync:
  stage: corpus
  image: python:3.12
  needs: []
  cache: {}
```

`.gitlab-ci.yml:44–49`:

```text
    - python3 scripts/ci/corpus-fetch-and-overlay.py --allow-lkg-fallback
    # Refuse to publish an overlay degraded past the freshness threshold.
    - python3 scripts/ci/corpus-freshness-check.py
    - git config user.name "zoder corpus sync"
    - git config user.email "jperlow@gmail.com"
    - git add corpus/model_corpus.json pricing/catalog.json bench/overlay.json corpus/freshness.json
```

`scripts/ci/corpus-freshness-check.py:98–105`:

```python
    if last_path.exists():
        try:
            last_overlay_count = 0
            # Best-effort: re-derive from the prior freshness report if
            # the prior overlay.json isn't checkable (the last-known-good
            # record is always `freshness-last.json` from the prior pass).
            last_meta = json.loads(last_path.read_text())
            last_overlay_count = int(last_meta.get("models_with_score", 0))
```

`scripts/ci/corpus-freshness-check.py:108–111`:

```python
        if last_overlay_count > 0:
            drop = (last_overlay_count - cur_overlay_count) / last_overlay_count
            print(f"corpus-freshness: coverage delta vs LKG = {drop:+.1%} (threshold {-COVERAGE_DROP_THRESHOLD:.0%})", file=sys.stderr)
            if drop > COVERAGE_DROP_THRESHOLD:
```

`scripts/ci/corpus-freshness-check.py:128–132`:

```python
    next_meta = dict(cur)
    next_meta["models_with_score"] = cur_overlay_count
    last_path.write_text(json.dumps(next_meta, indent=2, sort_keys=True) + "\n")
    print("corpus-freshness: OK (overlay publishable)", file=sys.stderr)
    return 0
```

The writer persists `freshness-last.json`, but the CI `git add` omits it and the job disables caches. `git ls-files corpus/freshness-last.json bench/raw bench/raw.lkg` returned no files at the reviewed revision. Thus a fresh checkout has no prior comparison baseline (nor the raw last-known-good snapshots).

Scenario: the preceding published overlay has 200 scored models; a clean scheduled job loses sources and produces 60, with at least one fetch marked successful. Sixty exceeds the absolute minimum of 50. Since `last_path.exists()` is false, the 70% regression bypasses the relative gate and is publishable.

Validation: an isolated temporary-directory run of the actual gate returned exit 0 for precisely that 60-row overlay without a baseline; supplying `models_with_score = 200` made the same gate return exit 1. The eight existing freshness tests pass; they do not establish CI persistence.

Change: commit the baseline alongside the corpus (or explicitly retrieve a durable versioned baseline), and establish durable per-source LKG inputs if fallback is promised. Add a two-clean-checkout publication test. On an established corpus, missing baseline should require explicit bootstrap/recovery, rather than silently disabling the drop check.

**F4 — CONFIRMED: zoder's catalog RPC describes a different protocol from the engine it builds**

`crates/zoder-core/src/catalog_models.rs:194–199`:

```rust
            &json!({
                "jsonrpc": "2.0",
                "id": "nvz-catalog",
                "method": "config/catalog-models",
                "params": {},
            }),
```

`crates/zoder-core/src/catalog_models.rs:119–123`:

```rust
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CatalogResponse {
    #[serde(default)]
    pub models: Vec<CatalogModel>,
}
```

`.zeroclaw-src/crates/zeroclaw-runtime/src/rpc/types.rs:1157–1161`:

```rust
    pub struct CatalogModelsParams {
        /// Accepts `model_provider` or aliased `provider` (gateway compat).
        #[serde(alias = "provider")]
        pub model_provider: String,
    }
```

`.zeroclaw-src/crates/zeroclaw-runtime/src/rpc/types.rs:1166–1168`:

```rust
    pub struct CatalogModelsResult {
        pub model_provider: String,
        pub models: Vec<String>,
```

`.zeroclaw-src/crates/zeroclaw-runtime/src/rpc/dispatch.rs:8144–8146`:

```rust
    async fn handle_config_catalog_models(&self, params: &Value) -> RpcResult {
        let req: CatalogModelsParams = parse_params(params)?;
        let local = crate::quickstart::model_provider_is_local(&req.model_provider);
```

`crates/zoder-cli/src/main.rs:3829–3838`:

```rust
        if cli.verbose > 0 {
            if let Some(out) = &enrichment {
                if out.has_live_data() {
                    eprintln!(
                        "[zoder] live catalog: +{} added, ~{} enriched ({} daemon rows)",
                        out.added, out.enriched, out.rows
                    );
                } else if let Some(err) = &out.error {
                    eprintln!("[zoder] live catalog enrichment skipped: {err}");
                }
```

Scenario: `zoder route` uses an available daemon from the reviewed checkout. The `{}` request cannot deserialize the required `model_provider: String`, so the daemon rejects it. Even adding that parameter alone would not fix enrichment: upstream returns `Vec<String>`, while zoder expects `Vec<CatalogModel>` objects. The merge error leaves the static corpus unchanged; the route command shows the diagnostic only with verbosity enabled. Models/consult use the same enrichment helper.

This is a present contract mismatch, not a hypothetical future ABI break. It does not mean all execution routing is broken: execution also uses the separate `config/get` registry/preflight path.

Change: query catalogs per configured provider, decode the actual string IDs and optional pricing map, and derive local billing classification from provider configuration rather than assuming upstream supplies `free` rows. Keep optional-daemon behavior, but distinguish unavailable enrichment from incompatible protocol in ordinary diagnostics. Add a contract test against the pinned daemon's real handler/schema; a mock returning zoder's invented object shape cannot detect this mismatch.

**F5 — CONFIRMED: explicit resume rejection is retried as a fresh session**

`crates/zoder-cli/src/main.rs:7931–7933`:

```rust
    if let Some(id) = cli.session.as_deref() {
        return Ok(Some(id.to_string()));
    }
```

`crates/acp-client/src/lib.rs:2226–2228`:

```rust
        Ok(v) => v,
        Err(msg) => {
            // Engine rejected the resume. The previous behavior
```

`crates/acp-client/src/lib.rs:2242–2251`:

```rust
            // Retry without session_id.
            let retry_params = zeroclaw_session_new_params(opts, None);
            write_frame(
                &mut write_half,
                &json!({
                    "jsonrpc": "2.0",
                    "id": "new",
                    "method": "session/new",
                    "params": Value::Object(retry_params),
                }),
```

`.zeroclaw-src/crates/zeroclaw-runtime/src/rpc/dispatch.rs:3581–3585`:

```rust
                    if data.agent_alias != req.agent_alias {
                        return Err(rpc_err(
                            INVALID_PARAMS,
                            "ACP session belongs to a different agent",
                        ));
```

Scenario: `--session S --agent B` targets a durable ACP session belonging to agent A. Zoder can successfully validate B's model/provider configuration before dispatch. Upstream rejects the resume with “ACP session belongs to a different agent”. The client treats every JSON-RPC error in this branch alike, retries with no session ID, and—if ordinary new-session creation succeeds—sends the prompt into a fresh conversation. There is no requirement that `--persist-session` was enabled and no requirement that the error meant an expired ID. Store-load errors and ownership denials can also enter this branch; the example does not depend on either.

Change: explicit `--session`/`--continue` must propagate resume failures. Limit optional persisted-session replacement to a typed stale/missing-session condition and clearly report the new ID. Preserve RPC error codes instead of collapsing all rejections to a string. Validate with an engine-shaped error followed by a mock that would accept a fresh session; assert that an explicit resume sends no second creation request or prompt.

**F6 — CONFIRMED: upstream converts repeated malformed tool calls into a successful text result**

The engine has a concrete fixture producing three malformed envelopes, then asserts a successful fallback with zero tool invocations:

`.zeroclaw-src/crates/zeroclaw-runtime/src/agent/loop_.rs:9984–9988`:

```rust
        let provider = ScriptedModelProvider::from_text_responses(vec![
            r#"{"toolcalls":[{"call_id":"call_1","arguments":{"value":"X"}}]}"#,
            r#"{"toolcalls":[{"call_id":"call_2","arguments":{"value":"Y"}}]}"#,
            r#"{"toolcalls":[{"call_id":"call_3","arguments":{"value":"Z"}}]}"#,
        ]);
```

`.zeroclaw-src/crates/zeroclaw-runtime/src/agent/loop_.rs:10058–10069`:

```rust
        .await
        .expect("malformed tool protocol should return a safe fallback");

        assert_eq!(
            result,
            crate::i18n::get_required_cli_string("channel-runtime-malformed-tool-output")
        );
        assert!(!result.contains("toolcalls"));
        assert_eq!(
            invocations.load(Ordering::SeqCst),
            0,
            "malformed protocol should never be executed as a tool call"
```

`.zeroclaw-src/crates/zeroclaw-runtime/src/agent/turn/mod.rs:2025–2027`:

```rust
            let msg = ChatMessage::assistant(fallback.to_string());
            turn_state.push_dual(msg);
            return Ok(accumulated_display_text);
```

`.zeroclaw-src/crates/zeroclaw-runtime/src/agent/agent.rs:4226–4231`:

```rust
            match loop_result {
                Ok(response) => {
                    // Commit-before-drain: this round's assistant output is in
                    // history/new_msgs (replay above) and committed_response
                    // before any steering continuation is folded in.
                    committed_response.push_str(&response);
```

`.zeroclaw-src/crates/zeroclaw-runtime/src/agent/agent.rs:4282–4288`:

```rust
                    return Ok(StreamedTurnSuccess {
                        response: committed_response,
                        new_messages: new_msgs,
                        provider_name: final_provider,
                        model: final_model,
                        final_context_limits: final_limits,
                        safeguard_fallback: turn_safeguard_fallback,
```

Scenario: a tool-enabled request asks the agent to perform work, and the provider emits the three malformed `toolcalls` envelopes above. After two retries the runtime returns fallback prose in `Ok(...)`; the streamed agent wraps the result as `StreamedTurnSuccess`, even though none of the requested tool operations executed. Logging the parser failure and suppressing raw protocol are useful, but the result type loses the failure. This is distinct from F1: even an intact transport can deliver a semantic failure as success.

Change: retain the user-facing diagnostic text, but return a typed failed/incomplete outcome after parser retry exhaustion and carry that through the terminal RPC event. Update the upstream regression to assert failure with zero tool invocations, then test zoder's exit/health treatment against that event. Do not patch prose matching into zoder; localized fallback text is not a reliable status protocol. The upstream fixture was read, not executed in this review.

**F7 — CONFIRMED: default engine builds are not reproducible or gated for compatibility with zoder**

`scripts/build.sh:16`:

```text
ZEROCLAW_REF="${ZEROCLAW_REF:-master}"
```

`scripts/build.sh:20–23`:

```text
  local tgt="$1" sfx="$2" zc=".zeroclaw-src"
  if [ ! -d "$zc/.git" ]; then git clone --depth 1 -b "$ZEROCLAW_REF" "$ZEROCLAW_REPO" "$zc"; fi
  ( cd "$zc" && git fetch -q origin "$ZEROCLAW_REF" && git checkout -q FETCH_HEAD )
  ( cd "$zc" && cargo build --release --bin zerocode --bin zeroclaw --target "$tgt" )
```

`.gitlab-ci.yml:199–201`:

```text
    - rm -rf upstream/src && git clone --depth 50 --branch "$UPSTREAM_REF" "$UPSTREAM_URL" upstream/src
    - cd upstream/src
    - "echo upstream-HEAD: $(git log --oneline -1)"
```

`.gitlab-ci.yml:208–210`:

```text
    - cargo check --locked --features ci-all --all-targets
    - cargo check --no-default-features
    - cargo test --locked --workspace --exclude zeroclaw-desktop
```

`.gitlab-ci.yml:225–232`:

```text
  rules:
    - if: "$CI_PIPELINE_SOURCE == \"schedule\""
    - if: "$CI_PIPELINE_SOURCE == \"web\""
  # Upstream breakage is INFORMATION, not a defect in this repo. This job
  # exists to notice when zeroclaw-labs/zeroclaw master stops building; making
  # it block turns someone else's commit into a red pipeline on our master
  # (and a buildwatch alert). It reports; it does not gate.
  allow_failure: true
```

Scenario: two macOS builds of the same zoder commit run on opposite sides of an upstream `master` change. Both fetch and check out their current `FETCH_HEAD`; they can produce different engines, including different RPC contracts. The scheduled advisory compiles/tests upstream in its own directory, not the zoder client against that engine, and allows failure. F4 demonstrates why an upstream build passing is insufficient. The `build_tui` path is used by the macOS branches; this finding does not claim that the Linux branch invokes it.

Change: store a reviewed engine commit in zoder, record that SHA in artifacts/runtime diagnostics, and gate its updates on small cross-repo tests: initialize, catalog, config/get, exact model/agent dispatch, resume, cancellation, terminal status. Keep a separate advisory job tracking upstream master. Support SHA pins correctly: a fresh checkout currently uses `git clone -b`, which expects a branch/tag rather than an arbitrary commit SHA; initialize/fetch/check out the pinned commit explicitly. This is a wire/process contract, not an in-process Rust ABI dependency.

**F8 — CONFIRMED: a reasoning-only non-streaming reply is accepted as an empty answer**

`crates/zoder-core/src/provider.rs:406–413`:

```rust
        pick_text(
            msg.content.clone(),
            msg.reasoning_content.clone(),
            msg.reasoning.clone(),
            /* show_reasoning = */ true,
        )
        .chars()
        .any(|c| !c.is_whitespace())
```

`crates/zoder-core/src/provider.rs:429–430`:

```rust
    } else {
        content.unwrap_or_default()
```

`crates/zoder-core/src/provider.rs:1472–1477`:

```rust
        let content = pick_text(
            msg.content,
            msg.reasoning_content,
            msg.reasoning,
            req.show_reasoning,
        );
```

`crates/zoder-core/src/provider.rs:1495–1498`:

```rust
        let tokens_out = completion_tokens.unwrap_or(0);
        Ok(ChatResult {
            content,
            tokens_out,
```

`crates/zoder-cli/src/main.rs:5272–5277`:

```rust
                used_model = model_id.clone();
                used_provider_id = pid.clone();
                used_latency_ms = model_started.elapsed().as_millis() as f64;
                outcome = Some(res);
                winning_reservation = Some(reservation);
                break;
```

Scenario: an OpenAI-compatible non-streaming provider returns `{"choices":[{"message":{"content":null,"reasoning_content":"thinking aloud"}}]}` and `show_reasoning` is false. `has_meaningful_message` validates using hardcoded `true`, so it accepts the reasoning. Actual rendering uses false and produces an empty string, returned in `Ok(ChatResult)`. The oneshot success arm selects that result and stops the fallback chain. With valid free telemetry, no policy failure rescues this answer-validation error.

Change: require a nonempty final answer for completion independently of whether reasoning may be displayed; classify a reasoning-only terminal reply as incomplete/decode failure and preserve reasoning separately. Test the actual non-streaming provider boundary with that JSON, including both display settings and ordinary nonempty answers. This is zoder's direct completion adapter; upstream's agentic reasoning handling has separate guards described below.

**F9 — CONFIRMED: absence of latency can improve Auto rank**

`crates/zoder-core/src/router.rs:109–113`:

```rust
            Tier::Auto => match (cap, m.latency_score) {
                (Some(c), Some(l)) => 1.0 + 0.6 * c + 0.4 * l,
                (Some(c), None) => 1.0 + c,
                (None, _) => m.agentic_score.or(m.w_swe).unwrap_or(0.0),
            },
```

`crates/zoder-core/src/router.rs:158–165`:

```rust
        keyed.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                // Deterministic secondary key: equal rank keys preserved
                // corpus insertion order, which is not stable across corpus
                // refreshes, so route.primary could flip run-to-run. Break
                // ties by model id (total order) — matches consult.
                .then_with(|| a.1.id.cmp(&b.1.id))
```

Scenario: two healthy, backed, free chat models each have code capability 80/100. Model A has no latency score; model B has latency score 0.7. Auto ranks A at 1.8 and B at 1.76. If A had the same measured latency as B, they would tie; deleting A's measurement improves its rank. More generally the absent-latency branch is equivalent to imputing latency equal to capability, despite those measuring different properties. Repeated equal defaults are then resolved by model ID.

Change: define and expose a missing-latency policy on the same scale as measured inputs, with provenance/confidence; do not change the capability coefficient merely because telemetry is absent. A conservative explicit prior or separate confidence band is preferable to an implicit capability-derived latency. Add a monotonicity fixture showing that removal of latency evidence alone cannot promote a candidate under the chosen policy. Include effective rank, metric provenance, and tie-break reason in route diagnostics. Arithmetic above was checked independently; no claim is made that these exact two models occur in today's corpus.

**Areas checked without a new substantiated defect**

The following paths appear sound for the specific cases checked; this is not a blanket certification of the modules.

- **`-m` / `--agent` selection:** current code rejects an explicit model without a configured agent route, checks the daemon's live route against the requested model, and rechecks the config snapshot before prompting. The repeatedly reported alias-as-model confusion should not be filed again against those guarded paths. `crates/zoder-cli/src/main.rs:5995–5999` says:

`crates/zoder-cli/src/main.rs:5995–5999`:

```rust
    if cli.model.is_some() {
        anyhow::bail!(
            "model {model:?} is not configured on any zeroclaw agent; add an \
             [agents.<alias>] model/model_provider route in {} or select an existing \
             agent with --agent <alias>",
```

`crates/acp-client/src/lib.rs:2133–2138`:

```rust
fn verify_zeroclaw_config_snapshot(expected: &Value, actual: &Value) -> anyhow::Result<()> {
    if actual != expected {
        bail!(
            "the running Zeroclaw configuration changed after model/agent routing was gated; \
             refusing to send session/prompt"
        );
```

- **Provider lookup and paid classification:** unmatched `serves` does not silently use the default provider; the policy considers the serving provider's paid status before accepting a free model flag. F2 affects quota selection, not this ordering.

`crates/zoder-core/src/config.rs:2719–2721`:

```rust
            return None;
        }
        candidates.into_iter().next().map(|c| c.provider)
```

`crates/zoder-core/src/policy.rs:93–100`:

```rust
        if provider_paid {
            return Decision::NeedConfirm(format!(
                "{PAID_WARNING}\n  model={} (routed to a paid/metered provider)",
                model.id
            ));
        }
        if model.free || provider_cost_neutral {
            Decision::Allow
```

- **The 900-second timeout is not silently successful in the ordinary exec timeout path:** the driver sends cancellation and drains; the CLI reports partial work and raises an error on unsuccessful outcomes. F1 concerns pre-terminal disconnects, not the configured deadline itself.

`crates/acp-client/src/lib.rs:2485–2489`:

```rust
                    &json!({
                        "jsonrpc": "2.0",
                        "id": "cancel",
                        "method": "session/cancel",
                        "params": { "session_id": session_id },
```

`crates/zoder-cli/src/main.rs:8923–8929`:

```rust
            let timed_out = t.run.outcome == "timeout";
            eprintln!(
                "[zoder] turn {} ({} chars captured). Resume with: zoder exec --session {} \"<continue>\"{}",
                if timed_out { "timed out — partial work kept" } else { "did not complete" },
                t.run.content.len(),
                t.run.session_id,
                if timed_out { "  (or raise --agent-timeout <secs>, default 900)" } else { "" },
```

`crates/zoder-cli/src/main.rs:8932`:

```rust
        anyhow::bail!("agentic turn ended: {}", t.run.outcome);
```

- **Inline reasoning in upstream agentic responses:** reading only the compatible provider adapter would produce a false finding. Downstream interpretation strips tagged thinking, and semantic-empty detection excludes a thinking-only response before acceptance. Arbitrary untagged prose cannot be reliably recognized as private reasoning from text alone; no defect is asserted for that case.

`.zeroclaw-src/crates/zeroclaw-runtime/src/agent/turn/parse_response.rs:206–209`:

```rust
    let response_text = strip_think_tags(resp.text_or_empty());
    // Strip trailing terminal markers (`<eom>`, `<|eom|>`) from non-streaming responses.
    // Handles stacked markers with arbitrary whitespace between them.
    let response_text = strip_trailing_terminal_markers(&response_text);
```

`.zeroclaw-src/crates/zeroclaw-api/src/model_provider.rs:227–229`:

```rust
    pub fn is_semantically_empty_terminal(&self) -> bool {
        strip_think_tags(self.text_or_empty()).is_empty() && self.tool_calls.is_empty()
    }
```

- **Two routers are not inherently duplicate authorities:** zoder ranks capability/health candidates; upstream resolves explicit `hint:` selectors to provider/model routes. Its chat path dispatches the resolved route (`.zeroclaw-src/crates/zeroclaw-providers/src/router.rs:373–382`). Their existence alone is not evidence of double-ranking. The checked upstream routing documentation explicitly describes unknown hints and provider pinning; no unsupported claim that zoder bypasses its paid gate through that router is made.
- **Subscription-tier resolution:** explicit-window override/provenance handling and unknown-tier fallback were inspected. Unknown tiers intentionally return explicit windows with a warning; this is not evidence of an automatic metered-charge bypass. The concrete per-model scope failure is F2. No separate tier-catalog finding is counted.

**Validation and limits**

- First file read occurred in tool call 1 (`scripts/build.sh`). The requested branch already existed at the same commit as local `master`, with a clean worktree; it was retained.
- Read-only tracing included production callers, wire types/handlers, adjacent tests, upstream route documentation, and both repository guidance files. Source excerpts in this document were extracted directly from the reviewed files with fixed line numbers.
- `python3 scripts/tests/corpus-freshness-tests.py`: **8 passed, 0 failed**.
- Isolated invocation of the actual freshness gate: **60 scored rows without baseline → exit 0; same rows with a 200-row baseline → exit 1**. Temporary files were outside the repository. Auto-rank arithmetic: **1.8 versus 1.76**.
- `cargo test -p zoder-core --lib --offline` did not reach tests: rustup's 1.96.1 component download failed. An installed-toolchain retry, `cargo +1.94.1 test -p zoder-core --lib --offline`, failed dependency resolution because locked `rustls 0.23.45` was unavailable in the offline cache (only 0.23.41 found). No Rust test-pass claim is made, and no lockfile/dependency changes were made to work around this.
- No live provider requests, billing, daemon sessions, fleet changes, upstream builds, or production mutations were performed. Rust control-flow findings are static confirmations; proposed mock-daemon/regression cases are follow-up validation, not tests claimed to have run.

Implement F1 first, then F2 and F3 as narrowly scoped changes. Correct and pin the seam with F4/F7 together before treating a newly built engine as tested with zoder. Preserve explicit failure states through F5/F6/F8, and make the rank policy in F9 explicit. None requires a rewrite.
