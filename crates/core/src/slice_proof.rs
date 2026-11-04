//! Reviewed whole-slice proof records and pure pre-dispatch evaluation.
//!
//! This is a structural gate, not proof authentication or a reservation. Callers must
//! load reviewed records from trusted storage and atomically reserve the returned bounds.
use semver::{Op, Version, VersionReq};
use serde::{Deserialize, Serialize};

/// Nonnegative US dollars, represented by an exact decimal string with at most 12
/// fractional places. Monetary arithmetic uses picodollars, never floating point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Usd(String);
impl Usd {
    pub fn parse(s: &str) -> Option<Self> {
        let x = Self(s.to_string());
        x.picos()?;
        Some(x)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    fn picos(&self) -> Option<u128> {
        let (whole, fraction) = self.0.split_once('.').unwrap_or((&self.0, ""));
        if whole.is_empty()
            || whole.len() > 18
            || fraction.len() > 12
            || !whole.bytes().all(|b| b.is_ascii_digit())
            || !fraction.bytes().all(|b| b.is_ascii_digit())
            || (self.0.contains('.') && fraction.is_empty())
        {
            return None;
        }
        let w: u128 = whole.parse().ok()?;
        let f: u128 = if fraction.is_empty() {
            0
        } else {
            fraction.parse().ok()?
        };
        w.checked_mul(1_000_000_000_000)?
            .checked_add(f.checked_mul(10u128.checked_pow((12 - fraction.len()) as u32)?)?)
    }
    fn from_micros(v: u64) -> Self {
        Self(format!("{}.{:06}", v / 1_000_000, v % 1_000_000))
    }
}

/// Only actual USD or an explicitly proven zero-incremental subscription is admissible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AccountingUnit {
    ActualUsd {
        billing_contract: String,
    },
    SubscriptionIncrementalUsd {
        billing_contract: String,
        zero_cost_proven: bool,
    },
    VirtualApiEquivalent {
        billing_contract: String,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SliceBoundKind {
    /// max_total_tokens is a separately enforced whole-slice bound, before adding epsilon.
    VendorCap {
        knob: String,
        cap_usd: Usd,
        epsilon_tokens: u64,
        epsilon_usd: Usd,
        token_bound_knob: String,
        max_total_tokens: Option<u64>,
    },
    Arithmetic {
        steps_knob: String,
        max_steps: u64,
        input_knob: String,
        max_input_tokens_per_step: u64,
        max_out_knob: String,
        max_output_tokens_per_step: u64,
        price_per_input_token: Usd,
        price_per_output_token: Usd,
    },
    PerRequest {
        max_requests: u64,
        max_tokens_per_request: u64,
        max_usd_per_request: Usd,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SliceBoundProof {
    pub proof_record_id: String,
    pub adapter: String,
    pub mode: String,
    pub cli_version_range: String,
    pub binary_hash: String,
    pub model: String,
    pub auth_billing_mode: String,
    pub config_digest: String,
    pub kind: SliceBoundKind,
    pub accounting_unit: AccountingUnit,
    pub transcript_ref: String,
}
#[derive(Debug, Clone, Copy)]
pub struct SliceProofInput<'a> {
    pub adapter: &'a str,
    pub mode: &'a str,
    pub installed_cli_version: &'a str,
    pub binary_hash: &'a str,
    pub model: &'a str,
    pub auth_billing_mode: &'a str,
    pub config_digest: &'a str,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedSliceBound {
    pub max_tokens: u64,
    pub max_cost_usd: Usd,
    pub max_cost_micros: u64,
    pub proof_record_id: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceAdmissionRefusal {
    ManualOnly,
    Unproven,
}
fn valid_text(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= 1024 && !s.chars().any(char::is_control)
}
fn valid_version_range(s: &str) -> Option<VersionReq> {
    let range = VersionReq::parse(s).ok()?;
    if range.comparators.is_empty()
        || range
            .comparators
            .iter()
            .any(|c| c.minor.is_none() || c.patch.is_none())
    {
        return None;
    }
    let exact = range.comparators.len() == 1 && range.comparators[0].op == Op::Exact;
    let lower = range
        .comparators
        .iter()
        .any(|c| matches!(c.op, Op::Greater | Op::GreaterEq));
    let upper = range
        .comparators
        .iter()
        .any(|c| matches!(c.op, Op::Less | Op::LessEq));
    (exact || (lower && upper)).then_some(range)
}
impl SliceBoundProof {
    fn bound(&self) -> Option<(u64, u64)> {
        if [
            &self.proof_record_id,
            &self.adapter,
            &self.mode,
            &self.binary_hash,
            &self.model,
            &self.auth_billing_mode,
            &self.config_digest,
            &self.transcript_ref,
        ]
        .iter()
        .any(|s| !valid_text(s))
        {
            return None;
        }
        let zero_allowed = match &self.accounting_unit {
            AccountingUnit::ActualUsd { billing_contract } if valid_text(billing_contract) => false,
            AccountingUnit::SubscriptionIncrementalUsd {
                billing_contract,
                zero_cost_proven: true,
            } if valid_text(billing_contract) && self.auth_billing_mode == "subscription" => true,
            _ => return None,
        };
        let (tokens, picos) = match &self.kind {
            SliceBoundKind::VendorCap {
                knob,
                cap_usd,
                epsilon_tokens,
                epsilon_usd,
                token_bound_knob,
                max_total_tokens,
            } => {
                let t = (*max_total_tokens)?;
                if !valid_text(knob)
                    || !valid_text(token_bound_knob)
                    || t == 0
                    || *epsilon_tokens == 0
                {
                    return None;
                }
                let epsilon = epsilon_usd.picos()?;
                if epsilon == 0 && !zero_allowed {
                    return None;
                }
                (
                    t.checked_add(*epsilon_tokens)?,
                    cap_usd.picos()?.checked_add(epsilon)?,
                )
            }
            SliceBoundKind::Arithmetic {
                steps_knob,
                max_steps,
                input_knob,
                max_input_tokens_per_step,
                max_out_knob,
                max_output_tokens_per_step,
                price_per_input_token,
                price_per_output_token,
            } => {
                if !valid_text(steps_knob)
                    || !valid_text(input_knob)
                    || !valid_text(max_out_knob)
                    || *max_steps == 0
                    || *max_input_tokens_per_step == 0
                    || *max_output_tokens_per_step == 0
                {
                    return None;
                }
                let tokens = max_input_tokens_per_step
                    .checked_add(*max_output_tokens_per_step)?
                    .checked_mul(*max_steps)?;
                let picos = (u128::from(*max_input_tokens_per_step)
                    .checked_mul(price_per_input_token.picos()?)?
                    .checked_add(
                        u128::from(*max_output_tokens_per_step)
                            .checked_mul(price_per_output_token.picos()?)?,
                    )?)
                .checked_mul(u128::from(*max_steps))?;
                (tokens, picos)
            }
            SliceBoundKind::PerRequest {
                max_requests,
                max_tokens_per_request,
                max_usd_per_request,
            } => {
                if *max_requests == 0 || *max_tokens_per_request == 0 {
                    return None;
                }
                (
                    max_requests.checked_mul(*max_tokens_per_request)?,
                    u128::from(*max_requests).checked_mul(max_usd_per_request.picos()?)?,
                )
            }
        };
        if picos == 0 && !zero_allowed {
            return None;
        }
        let micros = u64::try_from(picos.checked_add(999_999)? / 1_000_000).ok()?;
        Some((tokens, micros))
    }
}
/// Structural matching is exact. A missing registry is ManualOnly; drift, malformed records,
/// or multiple matching records are Unproven. No fallback record is chosen implicitly.
pub fn evaluate_slice_bounds(
    proofs: &[SliceBoundProof],
    input: &SliceProofInput<'_>,
) -> Result<AdmittedSliceBound, SliceAdmissionRefusal> {
    if proofs.is_empty() {
        return Err(SliceAdmissionRefusal::ManualOnly);
    }
    let version =
        Version::parse(input.installed_cli_version).map_err(|_| SliceAdmissionRefusal::Unproven)?;
    if [
        input.adapter,
        input.mode,
        input.binary_hash,
        input.model,
        input.auth_billing_mode,
        input.config_digest,
    ]
    .iter()
    .any(|v| !valid_text(v))
    {
        return Err(SliceAdmissionRefusal::Unproven);
    }
    let mut matched = proofs.iter().filter(|p| {
        p.adapter == input.adapter
            && p.mode == input.mode
            && p.binary_hash == input.binary_hash
            && p.model == input.model
            && p.auth_billing_mode == input.auth_billing_mode
            && p.config_digest == input.config_digest
            && valid_version_range(&p.cli_version_range).is_some_and(|req| req.matches(&version))
    });
    let p = matched.next().ok_or(SliceAdmissionRefusal::Unproven)?;
    if matched.next().is_some() {
        return Err(SliceAdmissionRefusal::Unproven);
    }
    let (tokens, micros) = p.bound().ok_or(SliceAdmissionRefusal::Unproven)?;
    Ok(AdmittedSliceBound {
        max_tokens: tokens,
        max_cost_usd: Usd::from_micros(micros),
        max_cost_micros: micros,
        proof_record_id: p.proof_record_id.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn money(s: &str) -> Usd {
        Usd::parse(s).unwrap()
    }
    fn proof() -> SliceBoundProof {
        SliceBoundProof {
            proof_record_id: "reviewed".into(),
            adapter: "claude".into(),
            mode: "print".into(),
            cli_version_range: ">=1.2.0, <2.0.0".into(),
            binary_hash: "sha256:abc".into(),
            model: "model".into(),
            auth_billing_mode: "api".into(),
            config_digest: "sha256:config".into(),
            accounting_unit: AccountingUnit::ActualUsd {
                billing_contract: "contract".into(),
            },
            transcript_ref: "reviewed-transcript".into(),
            kind: SliceBoundKind::Arithmetic {
                steps_knob: "steps".into(),
                max_steps: 2,
                input_knob: "context".into(),
                max_input_tokens_per_step: 100,
                max_out_knob: "output".into(),
                max_output_tokens_per_step: 20,
                price_per_input_token: money("0.000001"),
                price_per_output_token: money("0.000002"),
            },
        }
    }
    fn installed<'a>(p: &'a SliceBoundProof) -> SliceProofInput<'a> {
        SliceProofInput {
            adapter: &p.adapter,
            mode: &p.mode,
            installed_cli_version: "1.5.0",
            binary_hash: &p.binary_hash,
            model: &p.model,
            auth_billing_mode: &p.auth_billing_mode,
            config_digest: &p.config_digest,
        }
    }
    #[test]
    fn arithmetic_proof_reserves_both_dimensions() {
        let p = proof();
        let got = evaluate_slice_bounds(std::slice::from_ref(&p), &installed(&p)).unwrap();
        assert_eq!(got.max_tokens, 240);
        assert_eq!(got.max_cost_micros, 280);
    }
    #[test]
    fn unknown_and_drift_refuse() {
        let p = proof();
        let mut i = installed(&p);
        assert_eq!(
            evaluate_slice_bounds(&[], &i),
            Err(SliceAdmissionRefusal::ManualOnly)
        );
        i.model = "other";
        assert_eq!(
            evaluate_slice_bounds(std::slice::from_ref(&p), &i),
            Err(SliceAdmissionRefusal::Unproven)
        );
    }
    #[test]
    fn dollar_cap_never_creates_token_bound() {
        let mut p = proof();
        p.kind = SliceBoundKind::VendorCap {
            knob: "budget".into(),
            cap_usd: money("1"),
            epsilon_tokens: 10,
            epsilon_usd: money("0.1"),
            token_bound_knob: String::new(),
            max_total_tokens: None,
        };
        assert_eq!(
            evaluate_slice_bounds(std::slice::from_ref(&p), &installed(&p)),
            Err(SliceAdmissionRefusal::Unproven)
        );
    }
    #[test]
    fn vendor_cap_requires_independent_bound_and_reserves_overshoot() {
        let mut p = proof();
        p.kind = SliceBoundKind::VendorCap {
            knob: "budget".into(),
            cap_usd: money("1"),
            epsilon_tokens: 7,
            epsilon_usd: money("0.000000000001"),
            token_bound_knob: "turns".into(),
            max_total_tokens: Some(100),
        };
        let result = evaluate_slice_bounds(std::slice::from_ref(&p), &installed(&p)).unwrap();
        assert_eq!(result.max_tokens, 107);
        assert_eq!(result.max_cost_micros, 1_000_001);
        assert_eq!(result.max_cost_usd.as_str(), "1.000001");
    }
    #[test]
    fn per_request_checked_and_rounded_up() {
        let mut p = proof();
        p.kind = SliceBoundKind::PerRequest {
            max_requests: 3,
            max_tokens_per_request: 2,
            max_usd_per_request: money("0.000000000001"),
        };
        let a = evaluate_slice_bounds(std::slice::from_ref(&p), &installed(&p)).unwrap();
        assert_eq!(a.max_tokens, 6);
        assert_eq!(a.max_cost_micros, 1);
        p.kind = SliceBoundKind::PerRequest {
            max_requests: u64::MAX,
            max_tokens_per_request: 2,
            max_usd_per_request: money("0.01"),
        };
        assert!(evaluate_slice_bounds(std::slice::from_ref(&p), &installed(&p)).is_err());
    }
    #[test]
    fn all_pin_drift_and_invalid_ranges_fail_closed() {
        let p = proof();
        let base = installed(&p);
        let inputs = [
            SliceProofInput {
                adapter: "codex",
                ..base
            },
            SliceProofInput {
                mode: "interactive",
                ..base
            },
            SliceProofInput {
                installed_cli_version: "2.0.0",
                ..base
            },
            SliceProofInput {
                installed_cli_version: "1.5.0-alpha",
                ..base
            },
            SliceProofInput {
                binary_hash: "other",
                ..base
            },
            SliceProofInput {
                model: "other",
                ..base
            },
            SliceProofInput {
                auth_billing_mode: "subscription",
                ..base
            },
            SliceProofInput {
                config_digest: "other",
                ..base
            },
        ];
        for input in inputs {
            assert_eq!(
                evaluate_slice_bounds(std::slice::from_ref(&p), &input),
                Err(SliceAdmissionRefusal::Unproven)
            );
        }
        for range in ["*", ">=1.0.0", "1.2", ">=1.0.0, <garbage"] {
            let mut q = p.clone();
            q.cli_version_range = range.into();
            assert!(evaluate_slice_bounds(&[q], &base).is_err(), "{range}");
        }
        assert!(evaluate_slice_bounds(&[p.clone(), p.clone()], &base).is_err());
    }
    #[test]
    fn cost_and_arithmetic_require_proven_values() {
        for bad in ["", "-1", "NaN", "inf", "1e3", "0.0000000000001", "1."] {
            assert!(Usd::parse(bad).is_none(), "{bad}");
        }
        let mut p = proof();
        if let SliceBoundKind::Arithmetic { max_steps, .. } = &mut p.kind {
            *max_steps = 0
        };
        assert!(evaluate_slice_bounds(std::slice::from_ref(&p), &installed(&p)).is_err());
        if let SliceBoundKind::Arithmetic { max_steps, .. } = &mut p.kind {
            *max_steps = u64::MAX
        };
        assert!(evaluate_slice_bounds(std::slice::from_ref(&p), &installed(&p)).is_err());
        let mut p = proof();
        p.kind = SliceBoundKind::PerRequest {
            max_requests: 1,
            max_tokens_per_request: 1,
            max_usd_per_request: money("0"),
        };
        assert!(evaluate_slice_bounds(std::slice::from_ref(&p), &installed(&p)).is_err());
        p.accounting_unit = AccountingUnit::VirtualApiEquivalent {
            billing_contract: "virtual".into(),
        };
        assert!(evaluate_slice_bounds(std::slice::from_ref(&p), &installed(&p)).is_err());
        p.accounting_unit = AccountingUnit::SubscriptionIncrementalUsd {
            billing_contract: "plan".into(),
            zero_cost_proven: true,
        };
        assert!(evaluate_slice_bounds(std::slice::from_ref(&p), &installed(&p)).is_err());
        p.auth_billing_mode = "subscription".into();
        assert_eq!(
            evaluate_slice_bounds(std::slice::from_ref(&p), &installed(&p))
                .unwrap()
                .max_cost_micros,
            0
        );
        p.accounting_unit = AccountingUnit::SubscriptionIncrementalUsd {
            billing_contract: "plan".into(),
            zero_cost_proven: false,
        };
        assert!(evaluate_slice_bounds(std::slice::from_ref(&p), &installed(&p)).is_err());
    }
    #[test]
    fn builtins_are_manual_only_and_legacy_deserialization_defaults_empty() {
        for vendor in crate::VendorRegistry::with_builtins().list() {
            assert!(vendor.slice_bounds.is_empty());
            let mut data = serde_json::to_value(&vendor).unwrap();
            data.as_object_mut().unwrap().remove("slice_bounds");
            let legacy: crate::VendorDescriptor = serde_json::from_value(data).unwrap();
            assert!(legacy.slice_bounds.is_empty());
        }
    }
    #[test]
    fn runtime_vendor_registration_cannot_create_reviewed_proofs() {
        let registry = crate::VendorRegistry::with_builtins();
        let mut d = registry.get("claude").unwrap();
        d.id = "custom-proof".into();
        d.slice_bounds.push(proof());
        assert!(registry.register(d.clone()).is_err());
        assert!(registry.ensure_registered(d).is_err());
        assert!(registry.get("custom-proof").is_none());
    }
    #[test]
    fn malformed_serialized_prices_and_unpriced_vendor_overshoot_refuse() {
        let p = proof();
        let mut raw = serde_json::to_value(&p).unwrap();
        raw["kind"]["price_per_input_token"] = serde_json::json!("NaN");
        let p: SliceBoundProof = serde_json::from_value(raw).unwrap();
        assert!(evaluate_slice_bounds(std::slice::from_ref(&p), &installed(&p)).is_err());
        let mut p = proof();
        p.kind = SliceBoundKind::VendorCap {
            knob: "cap".into(),
            cap_usd: money("1"),
            epsilon_tokens: 1,
            epsilon_usd: money("0"),
            token_bound_knob: "turns".into(),
            max_total_tokens: Some(1),
        };
        assert!(evaluate_slice_bounds(std::slice::from_ref(&p), &installed(&p)).is_err());
    }
}
