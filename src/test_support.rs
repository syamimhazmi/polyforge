//! Test fixtures shared by more than one module's tests.

use crate::app::{App, ApprovalChoice, BackendKind, PendingApproval, PendingDiff};

pub(crate) fn muse_modal_app() -> App {
    let mut app = App::new();
    app.active_mut().backend = BackendKind::Muse;
    app.active_mut().stage_diff(PendingDiff {
        file: "tool".into(),
        body: "body".into(),
    });
    app.active_mut().pending_approval = Some(PendingApproval {
        approval_id: "a1".into(),
        requirement_id: serde_json::Value::Null,
        choices: vec![ApprovalChoice {
            choice_id: "c-allow".into(),
            decision: "approved".into(),
            scope: "once".into(),
            label: "Allow".into(),
            accepts_feedback: false,
        }],
    });
    app
}

pub(crate) fn busy_tab(backend: BackendKind) -> App {
    let mut app = App::new();
    let s = app.active_mut();
    s.backend = backend;
    s.remote_id = Some("sess-1".into());
    s.busy = true;
    s.sync_activity();
    app
}
