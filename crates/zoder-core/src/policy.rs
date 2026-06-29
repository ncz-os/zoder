//! Policy gate: free-first, default-deny paid, plus the anti-paid-fallback
//! guard that inspects LiteLLM telemetry after every "free" call.

use crate::config::Config;
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
        Self {
            allow_paid,
            free_hosts: cfg.free_api_hosts.clone(),
            strict_free,
        }
    }

    /// Pre-call decision. `provider_paid` is the serving provider's billing
    /// posture (paid or metered): a paid/metered PROVIDER requires confirmation
    /// even when the model id is classified free, because an org overlay can
    /// make a paid gateway the default route for a "free"-looking model — the
    /// spend would otherwise only be discovered after the call.
    pub fn check(&self, model: &ModelEntry, provider_paid: bool) -> Decision {
        if self.allow_paid {
            return Decision::Allow;
        }
        if provider_paid {
            return Decision::NeedConfirm(format!(
                "{PAID_WARNING}\n  model={} (routed to a paid/metered provider)",
                model.id
            ));
        }
        if model.free {
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
            gate(false, true).check(&free_model(), true),
            Decision::NeedConfirm(_)
        ));
        // free model + free provider -> allow.
        assert!(matches!(
            gate(false, true).check(&free_model(), false),
            Decision::Allow
        ));
        // --allow-paid overrides.
        assert!(matches!(
            gate(true, true).check(&free_model(), true),
            Decision::Allow
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
}
