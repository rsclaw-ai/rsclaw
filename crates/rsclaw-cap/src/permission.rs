//! Auto-approve handler for cap PermissionRequest events.

use cap_rs::core::{ClientFrame, PermissionDecision, RiskLevel};

/// Always returns AllowOnce. Logs at info! so an operator can audit
/// approvals by tail-grepping the gateway log. P2 (conversation mode)
/// will replace this with a real user-in-the-loop handler.
pub fn auto_approve(req_id: &str, tool: &str, risk: RiskLevel) -> ClientFrame {
    tracing::info!(
        target: "cap.permission",
        req_id,
        tool,
        ?risk,
        "auto-approve"
    );
    ClientFrame::PermissionResponse {
        req_id: req_id.to_owned(),
        decision: PermissionDecision::AllowOnce,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_allow_regardless_of_risk() {
        for risk in [RiskLevel::Low, RiskLevel::Medium, RiskLevel::High] {
            let frame = auto_approve("req-1", "shell", risk);
            match frame {
                ClientFrame::PermissionResponse { req_id, decision } => {
                    assert_eq!(req_id, "req-1");
                    assert!(matches!(decision, PermissionDecision::AllowOnce));
                }
                other => panic!("expected PermissionResponse, got {other:?}"),
            }
        }
    }
}
