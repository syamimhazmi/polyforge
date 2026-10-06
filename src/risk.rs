//! Approval-risk plumbing: TypeSafe scores spawned per pending card and
//! applied back to the app when they arrive.

use crate::app::App;
use crate::typesafe;
use tokio::sync::mpsc;

/// TypeSafe approval-risk result (tab + generation for staleness).
pub(crate) enum RiskMsg {
    Ready {
        tab: usize,
        token: u64,
        judgment: typesafe::ApprovalJudgment,
    },
    Failed {
        tab: usize,
        token: u64,
        err: String,
    },
}

/// Spawn one System One call per open DIFF that lacks a judgment yet.
pub(crate) fn spawn_pending_risk_scores(
    app: &mut App,
    client: &Option<typesafe::Client>,
    tx: &mpsc::Sender<RiskMsg>,
) {
    let Some(client) = client else {
        return;
    };
    for (tab, s) in app.sessions.iter_mut().enumerate() {
        let Some(diff) = s.pending_diff.as_ref() else {
            continue;
        };
        if s.approval_risk.is_some() {
            continue;
        }
        if s.risk_spawned_gen == Some(s.risk_gen) {
            continue;
        }
        let token = s.risk_gen;
        s.risk_spawned_gen = Some(token);
        let tool = diff.file.clone();
        let body = diff.body.clone();
        let client = client.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let msg = match client.judge_approval(&tool, &body).await {
                Ok(judgment) => RiskMsg::Ready {
                    tab,
                    token,
                    judgment,
                },
                Err(err) => RiskMsg::Failed { tab, token, err },
            };
            let _ = tx.send(msg).await;
        });
    }
}

pub(crate) fn apply_risk_msg(app: &mut App, msg: RiskMsg) {
    match msg {
        RiskMsg::Ready {
            tab,
            token,
            judgment,
        } => {
            let Some(s) = app.sessions.get_mut(tab) else {
                return;
            };
            if s.pending_diff.is_none() || s.risk_gen != token {
                return;
            }
            let line = judgment.summary_line();
            s.approval_risk = Some(judgment);
            if tab == app.active {
                app.flash = line;
            }
        }
        RiskMsg::Failed { tab, token, err } => {
            let Some(s) = app.sessions.get_mut(tab) else {
                return;
            };
            if s.pending_diff.is_none() || s.risk_gen != token {
                return;
            }
            // Leave approval_risk None; allow a later retry if token bumps.
            s.risk_spawned_gen = None;
            if tab == app.active {
                app.flash = format!("risk score failed: {err}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::muse_modal_app;
    use crate::typesafe;

    #[test]
    fn risk_ready_applies_only_for_matching_token() {
        let mut app = muse_modal_app();
        let token = app.active().risk_gen;
        let j = typesafe::ApprovalJudgment::compose(2.0, 1.0, 0.97, 0.96);
        apply_risk_msg(
            &mut app,
            RiskMsg::Ready {
                tab: 0,
                token,
                judgment: j.clone(),
            },
        );
        assert_eq!(app.active().approval_risk.as_ref(), Some(&j));
        assert!(app.flash.contains("HIGH"));

        // Stale token after clear: ignored.
        let _ = app.active_mut().clear_diff();
        apply_risk_msg(
            &mut app,
            RiskMsg::Ready {
                tab: 0,
                token,
                judgment: j,
            },
        );
        assert!(app.active().approval_risk.is_none());
    }

    #[test]
    fn risk_failed_clears_spawned_marker() {
        let mut app = muse_modal_app();
        let token = app.active().risk_gen;
        app.active_mut().risk_spawned_gen = Some(token);
        apply_risk_msg(
            &mut app,
            RiskMsg::Failed {
                tab: 0,
                token,
                err: "boom".into(),
            },
        );
        assert!(app.active().approval_risk.is_none());
        assert!(app.active().risk_spawned_gen.is_none());
        assert!(app.flash.contains("boom"));
    }
}
