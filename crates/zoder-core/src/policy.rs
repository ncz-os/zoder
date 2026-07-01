//! Policy gate: free-first, default-deny paid, plus the anti-paid-fallback
//! guard that inspects LiteLLM telemetry after every "free" call.

use crate::config::{BillingMode, Config};
use crate::corpus::ModelEntry;
use crate::provider::CallTelemetry;
use url::Url;

pub const PAID_WARNING: &str = "ARE YOU SURE YOU WANT TO USE A PAID MODEL? \
YOU WILL INCUR REAL COSTS THAT ARE TRACKED IN YOUR LOCAL LEDGER";

/// Extract the lowercased host from an `api_base` value. Handles userinfo,
/// port, and path correctly (no spoofable substring matching). Accepts bare
/// `host[:port]` values by retrying with an `https://` scheme.
fn host_of(api_base: &str) -> Option<String> {
    let parse = |s: &str| {
        Url::parse(s)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
    };
    parse(api_base).or_else(|| parse(&format!("https://{api_base}")))
}

/// True when `host` equals a free host exactly or is a subdomain of one.
fn host_is_free(host: &str, free_hosts: &[String]) -> bool {
    free_hosts.iter().any(|h| {
        let h = h.trim().to_ascii_lowercase();
        !h.is_empty() && (host == h || host.ends_with(&format!(".{h}")))
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Free model: proceed.
    Allow,
    /// Paid model: requires interactive confirmation (or --allow-paid).
    NeedConfirm(String),
}

pub struct PolicyGate {
    allow_paid: bool,
    free_hosts: Vec<String>,
    /// Fail closed when free-call telemetry is missing entirely.
    strict_free: bool,
}

impl PolicyGate {
    /// `strict_free` should typically be `cfg.strict_free && !lenient_flag`.
    pub fn new(cfg: &Config, allow_paid: bool, strict_free: bool) -> Self {
        let mut free_hosts = cfg.free_api_hosts.clone();
        // Operator-declared-cost-neutral providers (Free OR Subscription
        // billing, and not paid) are trusted $0-marginal backends: fold each
        // one's base_url host into the free-host set. This is what lets a
        // flat-rate subscription provider (e.g. MiniMax) pass the strict free
        // guard — its calls report no LiteLLM api_base header, so api_base falls
        // back to the provider base_url, and the host is verified here. Without
        // this an overlay could never extend the free-host set (it lives on
        // config.json).
        for p in &cfg.providers {
            if !p.paid && p.billing != BillingMode::Metered {
                if let Some(host) = host_of(&p.base_url) {
                    free_hosts.push(host);
                }
            }
        }
        Self {
            allow_paid,
            free_hosts,
            strict_free,
        }
    }

    /// Pre-call decision. `provider_paid` is the serving provider's billing
    /// posture (paid or metered): a paid/metered PROVIDER requires confirmation
    /// even when the model id is classified free, because an org overlay can
    /// make a paid gateway the default route for a "free"-looking model — the
    /// spend would otherwise only be discovered after the call.
    ///
    /// `provider_cost_neutral` is true when the SERVING provider is declared
    /// Free or Subscription (and not `paid`). A cost-neutral provider means
    /// the marginal cost of the call is $0 even if the model id isn't
    /// classified free in the corpus, so the gate must Allow — otherwise a
    /// subscription provider would be wrongly gated as paid.
    pub fn check(
        &self,
        model: &ModelEntry,
        provider_paid: bool,
        provider_cost_neutral: bool,
    ) -> Decision {
        if self.allow_paid {
            return Decision::Allow;
        }
        if provider_paid {
            return Decision::NeedConfirm(format!(
                "{PAID_WARNING}\n  model={} (routed to a paid/metered provider)",
                model.id
            ));
        }
        if model.free || provider_cost_neutral {
            Decision::Allow
        } else {
            let why = model
                .gated_reason
                .clone()
                .unwrap_or_else(|| "paid model".into());
            Decision::NeedConfirm(format!("{PAID_WARNING}\n  model={} ({why})", model.id))
        }
    }

    /// Post-call guard: a model we treated as FREE must have actually been
    /// served from a free/internal backend at $0 with no silent fallback.
    /// Catches the free->paid (azure/baseten) fallback cost trap.
    pub fn verify_free(&self, model: &ModelEntry, t: &CallTelemetry) -> Result<(), String> {
        if !model.free {
            return Ok(());
        }
        if let Some(cost) = t.cost_usd {
            if cost > 0.0 {
                return Err(format!(
                    "free model {} billed ${cost} (paid fallback)",
                    model.id
                ));
            }
        }
        if let Some(fb) = t.attempted_fallbacks {
            if fb > 0 {
                return Err(format!("free model {} used {fb} fallback(s)", model.id));
            }
        }
        if let Some(base) = &t.api_base {
            let served_free = host_of(base)
                .map(|host| host_is_free(&host, &self.free_hosts))
                .unwrap_or(false);
            if !served_free {
                return Err(format!(
                    "free model {} served from non-free backend {base}",
                    model.id
                ));
            }
        }
        // Strict mode: require POSITIVE proof the call was served free — a
        // free-host `api_base` (verified above). cost==0 alone is NOT proof: a
        // paid backend can report $0 / omit cost telemetry, so cost-only or
        // empty telemetry is rejected. (`--lenient-telemetry` clears strict.)
        if self.strict_free && t.api_base.is_none() {
            return Err(format!(
                "free model {} returned no served-backend (api_base) telemetry; cannot \
                 prove it was served free (strict mode; pass --lenient-telemetry to allow). \
                 Cost-only telemetry is insufficient proof.",
                model.id
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::CallTelemetry;

    fn gate(allow_paid: bool, strict: bool) -> PolicyGate {
        PolicyGate {
            allow_paid,
            free_hosts: vec!["free.example.com".into()],
            strict_free: strict,
        }
    }
    fn free_model() -> ModelEntry {
        ModelEntry {
            id: "free-m".into(),
            free: true,
            ..Default::default()
        }
    }

    #[test]
    fn paid_provider_blocks_even_a_free_model() {
        // free model id, but routed to a paid/metered provider -> confirm.
        assert!(matches!(
            gate(false, true).check(&free_model(), true, false),
            Decision::NeedConfirm(_)
        ));
        // free model + free provider -> allow.
        assert!(matches!(
            gate(false, true).check(&free_model(), false, false),
            Decision::Allow
        ));
        // --allow-paid overrides.
        assert!(matches!(
            gate(true, true).check(&free_model(), true, false),
            Decision::Allow
        ));
    }

    fn non_free_model() -> ModelEntry {
        ModelEntry {
            id: "gated-m".into(),
            free: false,
            gated_reason: Some("not in free corpus".into()),
            ..Default::default()
        }
    }

    #[test]
    fn cost_neutral_provider_allows_non_free_model() {
        // (a) provider_cost_neutral = true + a NON-free model -> Allow.
        // The serving provider is $0-marginal (Free or Subscription), so the
        // gate must not require confirmation even though the corpus has the
        // model marked non-free (e.g. a subscription provider serves a model
        // the free corpus does not classify).
        assert!(matches!(
            gate(false, true).check(&non_free_model(), false, true),
            Decision::Allow
        ));
    }

    #[test]
    fn paid_provider_dominates_even_if_cost_neutral() {
        // (b) provider_paid = true still NeedConfirm even if
        // provider_cost_neutral is true. paid dominates — a gateway that
        // is explicitly paid never gets bypassed by a coincidental
        // cost-neutral flag.
        assert!(matches!(
            gate(false, true).check(&non_free_model(), true, true),
            Decision::NeedConfirm(_)
        ));
    }

    #[test]
    fn non_free_model_with_no_provider_signal_still_needs_confirm() {
        // (c) non-free model + both flags false -> NeedConfirm (the
        // historical behavior: unknown model on an unverified provider
        // stays gated).
        assert!(matches!(
            gate(false, true).check(&non_free_model(), false, false),
            Decision::NeedConfirm(_)
        ));
    }

    #[test]
    fn strict_free_rejects_cost_only_telemetry() {
        let g = gate(false, true);
        // cost=0 but NO api_base -> cannot prove served-free -> Err in strict mode.
        let t = CallTelemetry {
            cost_usd: Some(0.0),
            ..Default::default()
        };
        assert!(
            g.verify_free(&free_model(), &t).is_err(),
            "cost-only telemetry must be rejected in strict mode"
        );
        // free-host api_base -> proven free -> Ok.
        let t_ok = CallTelemetry {
            cost_usd: Some(0.0),
            api_base: Some("https://free.example.com/v1".into()),
            ..Default::default()
        };
        assert!(g.verify_free(&free_model(), &t_ok).is_ok());
        // non-free api_base -> Err.
        let t_bad = CallTelemetry {
            api_base: Some("https://paid.azure.com/v1".into()),
            ..Default::default()
        };
        assert!(g.verify_free(&free_model(), &t_bad).is_err());
        // any real cost -> Err regardless of host.
        let t_paid = CallTelemetry {
            cost_usd: Some(0.5),
            api_base: Some("https://free.example.com/v1".into()),
            ..Default::default()
        };
        assert!(g.verify_free(&free_model(), &t_paid).is_err());
    }

    #[test]
    fn declared_free_provider_host_passes_strict_guard() {
        // A flat-rate subscription provider declared billing=free contributes
        // its base_url host to the free-host set, so a call whose api_base is
        // that host (the base_url fallback) verifies free under strict mode.
        let mut cfg = Config::default_provider(std::path::Path::new("/tmp/zoder-test"));
        cfg.providers.push(crate::config::Provider {
            id: "minimax".into(),
            base_url: "https://api.minimax.io/v1".into(),
            kind: "openai-chat".into(),
            auth: crate::config::Auth::None,
            paid: false,
            billing: BillingMode::Free,
            subscription: None,
            serves: vec!["MiniMax-".into()],
        });
        let g = PolicyGate::new(&cfg, false, true);
        let t = CallTelemetry {
            api_base: Some("https://api.minimax.io/v1".into()),
            ..Default::default()
        };
        assert!(
            g.verify_free(&free_model(), &t).is_ok(),
            "declared-free provider host must pass the strict free guard"
        );
        // A host NOT declared free still fails strict mode.
        let t_bad = CallTelemetry {
            api_base: Some("https://api.openai.com/v1".into()),
            ..Default::default()
        };
        assert!(g.verify_free(&free_model(), &t_bad).is_err());
    }
}
