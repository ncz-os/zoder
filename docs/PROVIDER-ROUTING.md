# Per-model provider routing, pinned primary, and free open-weight ingestion

By default zoder picks the best **free** model for a task and sends every model
in the fallback chain to the single `default_provider` endpoint. That is fine
when one provider serves the whole free pool, but it cannot express "use my
subscription model first, then fall back to a *different* provider's free
models." This document covers the three config knobs that make that work.

## 1. `serves` — per-model provider routing

Each `[[providers]]` entry may declare a `serves` list of **model-id prefixes**
it serves. A routed model id is sent to the provider with the **longest**
matching prefix (most specific wins; config order breaks exact-length ties).
A model that no provider claims falls back to `default_provider` — so this is
purely additive and never changes behavior for existing single-provider setups.

```toml
# A flat-rate subscription model on its own endpoint.
[[providers]]
id = "minimax"
base_url = "https://api.minimax.io/v1"
kind = "openai-chat"
auth = { type = "env", var = "MINIMAX_API_KEY" }
paid = false           # flat-rate subscription => $0 marginal => treated as free
billing = "free"
serves = ["MiniMax-"]  # MiniMax-M3, MiniMax-Text-01, ... -> this provider

# A free open-weight endpoint (any OpenAI-compatible host): open-weight
# model families only (NOT the metered azure/aws/oci/gcp/google catalog).
[[providers]]
id = "openweight"
base_url = "https://your-openai-compatible-host.example/v1"
kind = "openai-chat"
auth = { type = "env", var = "OPENWEIGHT_API_KEY" }
paid = false
billing = "free"
serves = ["deepseek-ai/", "meta/llama-", "mistralai/", "qwen/"]
```

With the above, a single `zoder exec` fallback chain can run `MiniMax-M3` on
`api.minimax.io` and then `meta/llama-3.3-70b-instruct` on the open-weight host
— each link hits the provider that actually serves it. The policy gate runs
**per link**: a fallback that resolves to a paid / metered provider is skipped
fail-closed (use `--allow-paid` to permit it), and the spend ledger records the
provider that actually served the winning model.

**Prefixes should be delimiter-bounded** (`deepseek-ai/`, `meta/llama-`,
`MiniMax-`) so a short prefix like `meta` cannot also capture `metamath/...`. An
empty prefix is rejected by config validation (it would capture every model).

## 2. `primary_model` — pin a model to lead the chain

A vendor overlay's `[profile]` may pin a `primary_model`. The router always
tries it **first**, then ranks the rest of the free pool (by capability ×
latency × live health, the usual `rank_key`) as the fallback chain behind it.
The pin is honored even if the model is not itself in the free pool (e.g. a
subscription model), so an empty free pool is not an error in this path.

```toml
# config.minimax.toml — pin MiniMax first; the free open-weight models fall back.
[profile]
name = "minimax"
primary_model = "MiniMax-M3"
```

`primary_model` is independent of `[profile].default` — an overlay can pin the
primary model without owning the `default_provider`. If several overlays set it,
the default-claiming overlay wins, otherwise the alphabetically-last one. It
applies to `zoder exec` (oneshot + agentic) and `zoder route`.

## 3. Free open-weight ingestion via `zoder refresh`

`zoder refresh` reconciles the model corpus against live `/models` catalogs. In
addition to the default provider, it now queries **every provider that declares
`serves` and is billed free**, filters that provider's returned ids to its own
`serves` allowlist, and folds the survivors into the routing pool as free,
routable chat candidates.

For the free open-weight provider above this means the open-weight models
(`deepseek-ai/* | meta/llama-* | mistralai/* | qwen/*`) become routable, while
the host's `azure/ aws/ oci/ gcp/ google`-prefixed (metered/hosted) catalog
entries are dropped because they do not match the `serves` allowlist. Safety
rails:

- A model the corpus already classifies as **paid** (or with nonzero per-token
  economics) is never silently flipped to free.
- A newly-ingested, unbenched model gets a neutral capability prior so it is
  selectable as a fallback until the corpus builder benches it; a real
  benchmark always overrides the prior.
- Reconciliation runs against the **union** of every provider's served ids, so a
  free provider's models are not retired by the default provider's narrower list;
  a model that genuinely leaves all served catalogs is retired (and its `free`
  flag cleared).

Run it after wiring a new free provider:

```sh
zoder refresh           # adds the free open-weight models; prints how many were promoted
zoder route "..."       # confirm MiniMax-M3 leads, the free models fall back
```
