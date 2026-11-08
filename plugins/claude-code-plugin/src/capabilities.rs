//! Conditional native capabilities. Version strings alone never prove state-marker behavior.
use bcode_claude_code::SettlementSupport;
use bcode_harness_models::{HarnessCapabilities, HarnessCapability, HarnessSupportLevel};

const fn supported() -> HarnessCapability {
    HarnessCapability {
        support: HarnessSupportLevel::Supported,
        constraints: Vec::new(),
    }
}
fn constrained(message: &str) -> HarnessCapability {
    HarnessCapability {
        support: HarnessSupportLevel::Constrained,
        constraints: vec![message.into()],
    }
}
fn unsupported(message: &str) -> HarnessCapability {
    HarnessCapability {
        support: HarnessSupportLevel::Unsupported,
        constraints: vec![message.into()],
    }
}

/// API custody provides local credential readiness, not native login or remote verification.
pub fn api_auth_capability() -> HarnessCapability {
    constrained(
        "Exact host-admitted vault profile required; no native login, ambient key, subscription fallback or implicit remote verification",
    )
}

pub fn capabilities(settlement: SettlementSupport) -> HarnessCapabilities {
    let authoritative = settlement == SettlementSupport::VerifiedSessionState;
    HarnessCapabilities {
        model_switch: HarnessCapability {
            support: HarnessSupportLevel::Supported,
            constraints: vec![
                "Between foreground turns only; init-advertised native model keys only; acknowledgement is the native control response, not proof a later turn used it".into(),
            ],
        },
        permission_mode: HarnessCapability {
            support: HarnessSupportLevel::Supported,
            constraints: vec![
                "Native default, plan and acceptEdits modes; host approval policy still governs every surfaced callback".into(),
            ],
        },
        resume: constrained(
            "Exact native session identity and matching executable/config provenance required; no most-recent-session adoption",
        ),
        steering: unsupported(
            "Claude stream input has no expected-native-turn compare-and-set; active steering is rejected",
        ),
        approval: supported(),
        questions: constrained(
            "Single-question, single-select AskUserQuestion forms only; unsupported forms fail closed",
        ),
        assistance: constrained(
            "Host-admitted child/parent generation grant required at open and reauthorized for every response; bounded text-only SDK MCP assistance, no parent tool dispatch",
        ),
        // Support describes correlated native interrupt delivery, not a terminal
        // execution outcome. Live ownership/control readiness are enforced by control().
        interrupt: supported(),
        authoritative_settlement: if authoritative {
            // The exact verified SDK lifecycle enforces idle + result + completed
            // background work before emitting Settled; this is not a host constraint.
            supported()
        } else {
            unsupported(
                "This exact executable has no behaviorally verified session_state_changed contract",
            )
        },
        background_continuation: if authoritative {
            supported()
        } else {
            constrained("Result events remain nonterminal; authoritative settlement unavailable")
        },
        authentication: constrained(
            "Native account namespace only; explicit supported CLI login handoff, no invented auth RPC or API-key extraction",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unverified_mode_does_not_advertise_settlement_or_steering() {
        let value = capabilities(SettlementSupport::Unknown);
        assert_eq!(
            value.authoritative_settlement.support,
            HarnessSupportLevel::Unsupported
        );
        assert_eq!(value.steering.support, HarnessSupportLevel::Unsupported);
        assert!(value.validate().is_ok());
    }
    #[test]
    fn assistance_declares_host_admission_constraint() {
        let value = capabilities(SettlementSupport::VerifiedSessionState);
        assert_eq!(value.assistance.support, HarnessSupportLevel::Constrained);
        assert_ne!(value.assistance.constraints, Vec::<String>::new());
    }
    #[test]
    fn interrupt_delivery_is_supported_independently_of_settlement_evidence() {
        for mode in [
            SettlementSupport::Unknown,
            SettlementSupport::VerifiedSessionState,
        ] {
            let value = capabilities(mode);
            assert_eq!(value.interrupt.support, HarnessSupportLevel::Supported);
            assert_eq!(value.interrupt.constraints, Vec::<String>::new());
            assert!(value.validate().is_ok());
        }
        // Supporting native control delivery does not upgrade unknown settlement.
        assert_eq!(
            capabilities(SettlementSupport::Unknown)
                .authoritative_settlement
                .support,
            HarnessSupportLevel::Unsupported
        );
    }
    #[test]
    fn verified_sdk_lifecycle_supports_authoritative_settlement() {
        let value = capabilities(SettlementSupport::VerifiedSessionState);
        assert_eq!(
            value.authoritative_settlement.support,
            HarnessSupportLevel::Supported
        );
        assert!(value.validate().is_ok());
    }
}
