//! Key and mouse input handling plus the terminal side effects it triggers
//! (bell, mouse capture, clipboard).

use crate::app::{App, BackendKind, DecisionKind, Mode, OutboxDecide};
use crate::claude::{map_claude_decision, queue_claude_frame};
use crate::codex::map_codex_decision;
use crate::grok::map_grok_decision;
use crate::provider::map_decision;
use crate::{clipboard, grok};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture, KeyCode, KeyModifiers};
use crossterm::execute;
use std::io::{self, Write};

/// Wheel = 3 lines, Shift+wheel = page (M1 spec, unchanged).
pub(crate) fn wheel_step(m: &crossterm::event::MouseEvent, app: &App) -> i32 {
    if m.modifiers.contains(KeyModifiers::SHIFT) {
        app.viewport_height as i32
    } else {
        3
    }
}

pub(crate) fn apply_mouse_capture(app: &App) -> io::Result<()> {
    if app.mouse {
        execute!(io::stdout(), EnableMouseCapture)?;
    } else {
        execute!(io::stdout(), DisableMouseCapture)?;
    }
    Ok(())
}

pub(crate) fn ring_bell() {
    print!("\x07");
    let _ = io::stdout().flush();
}

/// Copy entry point: grok-parity legs (native + tmux + OSC 52, the last
/// capped at `clipboard::MAX_OSC52_RAW_BYTES`) with a backup file unless
/// `POLYFORGE_CLIPBOARD_NO_BACKUP` is set. Returns the status flash naming
/// where the text landed.
pub(crate) fn copy_to_clipboard(text: &str) -> String {
    clipboard::copy_text_or_file(text).toast_message()
}

fn clear_pending_g(app: &mut App) {
    app.pending_g = false;
}

/// Queue a UI-side approval decision: transcript lines now, wire decide via
/// the outbox (live backends) or the immediate mock close-out.
pub(crate) fn decide_ui(app: &mut App, kind: DecisionKind, label: &str) {
    let tab = app.active;
    let backend = app.sessions[tab].backend;
    match backend {
        BackendKind::Mock => {
            if app.decide_diff(label) {
                ring_bell();
            }
        }
        BackendKind::Muse => {
            let approval = app.active().pending_approval.clone();
            match approval {
                Some(a) => match map_decision(&a, kind) {
                    Some((choice_id, feedback)) => {
                        app.outbox.decides.push(OutboxDecide {
                            tab,
                            backend,
                            approval_id: a.approval_id,
                            requirement_id: a.requirement_id,
                            choice_id,
                            feedback,
                        });
                        if app.muse_approved(label) {
                            ring_bell();
                        }
                    }
                    None => {
                        // S2-F2: a negative decision must always deny on
                        // the wire. With no deny choice, closing the card
                        // would fake a denial the server never received
                        // (it may treat silence as consent), so keep the
                        // card open and say so loudly. Positive decisions
                        // still close locally: nothing is approved
                        // server-side, so closing is fail-closed.
                        if matches!(kind, DecisionKind::Reject | DecisionKind::Later) {
                            app.flash = format!(
                                "muse: cannot send `{label}` — server offered no deny path (card kept open; nothing denied)"
                            );
                            ring_bell();
                        } else {
                            app.muse_approved(label);
                            app.flash = format!(
                                "muse: closed locally — server offered no way to send `{label}`"
                            );
                            ring_bell();
                        }
                    }
                },
                None => {
                    if app.muse_approved(label) {
                        ring_bell();
                    }
                }
            }
        }
        BackendKind::Codex => {
            let approval = app.active().pending_approval.clone();
            match approval {
                Some(a) => match map_codex_decision(&a, kind) {
                    Some((req_id, decision)) => {
                        app.outbox.decides.push(OutboxDecide {
                            tab,
                            backend,
                            approval_id: a.approval_id,
                            requirement_id: req_id,
                            choice_id: decision,
                            feedback: None,
                        });
                        if app.approved("codex", label) {
                            ring_bell();
                        }
                    }
                    None => {
                        app.approved("codex", label);
                        app.flash = format!(
                            "codex: closed locally — server offered no way to send `{label}`"
                        );
                        ring_bell();
                    }
                },
                None => {
                    if app.approved("codex", label) {
                        ring_bell();
                    }
                }
            }
        }
        BackendKind::Grok => {
            let approval = app.active().pending_approval.clone();
            match approval {
                Some(a) => match map_grok_decision(&a, kind) {
                    Some((req_id, payload)) => {
                        app.outbox.decides.push(OutboxDecide {
                            tab,
                            backend,
                            approval_id: a.approval_id,
                            requirement_id: req_id,
                            choice_id: payload.to_string(),
                            feedback: None,
                        });
                        if app.approved("grok", label) {
                            ring_bell();
                        }
                    }
                    None => {
                        grok::queue_grok_cancelled(app, tab, a.requirement_id);
                        app.approved("grok", "cancelled");
                        app.flash = format!(
                            "grok: cancelled — server offered no matching option for `{label}`"
                        );
                        ring_bell();
                    }
                },
                None => {
                    if app.approved("grok", label) {
                        ring_bell();
                    }
                }
            }
        }
        BackendKind::Claude => {
            if let Some(a) = app.active().pending_approval.clone() {
                let sid = a.requirement_id["session_id"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                let frame = map_claude_decision(&a, kind);
                let once = frame
                    .pointer("/response/response/updatedPermissions")
                    .is_none();
                queue_claude_frame(app, tab, &sid, frame);
                if matches!(kind, DecisionKind::ApproveAll) && once {
                    app.flash = "claude: no 'always' rule offered — allowed once".to_string();
                }
            }
            if app.approved("claude", label) {
                ring_bell();
            }
        }
        BackendKind::Agy => {
            // Agy has no interactive approvals (vendor policy decides); the
            // modal can never appear, so any key here just ensures closure.
            app.flash =
                "agy: approvals aren't interactive — vendor policy decides (see transcript)"
                    .to_string();
        }
    }
}

/// Picker Esc: back to Normal, or quit during onboarding (no backend yet).
fn cancel_picker(app: &mut App) {
    if app.onboarding {
        app.should_quit = true;
    } else {
        app.mode = Mode::Normal;
    }
}

/// Picker choice: onboarding starts the tab + saves the provider;
/// otherwise the tab respawns fresh on the new backend.
fn pick_backend(app: &mut App, backend: BackendKind) {
    if app.onboarding {
        app.choose_first_backend(backend);
    } else {
        app.respawn_active(backend);
    }
}

pub(crate) fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    // Crossterm reports uppercase letters with SHIFT held (e.g. `G` arrives
    // as Char('G')+SHIFT). A lone SHIFT must not break single-key bindings.
    let no_mods = mods.is_empty() || mods == KeyModifiers::SHIFT;

    // Diff modal steals y/n/a/q on the active tab.
    if app.active().pending_diff.is_some() {
        if let KeyCode::Char(c) = code {
            if no_mods {
                match c {
                    'y' => {
                        decide_ui(app, DecisionKind::Approve, "approved");
                        return;
                    }
                    'n' => {
                        decide_ui(app, DecisionKind::Reject, "rejected");
                        return;
                    }
                    'a' => {
                        decide_ui(app, DecisionKind::ApproveAll, "approved-all");
                        return;
                    }
                    'q' => {
                        decide_ui(app, DecisionKind::Later, "deferred");
                        return;
                    }
                    _ => {}
                }
            }
        }
        if code == KeyCode::Esc {
            decide_ui(app, DecisionKind::Later, "deferred");
            return;
        }
    }

    match app.mode {
        Mode::Normal => match code {
            KeyCode::Char('c') if ctrl => app.should_quit = true,
            KeyCode::Char('q') if no_mods => app.should_quit = true,
            KeyCode::Char('j') if no_mods && app.vim => {
                clear_pending_g(app);
                app.scroll_lines(1);
            }
            KeyCode::Char('k') if no_mods && app.vim => {
                clear_pending_g(app);
                app.scroll_lines(-1);
            }
            KeyCode::Down if no_mods => {
                clear_pending_g(app);
                app.scroll_lines(1);
            }
            KeyCode::Up if no_mods => {
                clear_pending_g(app);
                app.scroll_lines(-1);
            }
            KeyCode::Char('u') if ctrl => {
                clear_pending_g(app);
                let h = app.half_page();
                app.scroll_lines(-h);
            }
            KeyCode::Char('d') if ctrl => {
                clear_pending_g(app);
                let h = app.half_page();
                app.scroll_lines(h);
            }
            // Prototype deviation (documented): single `g` = top, `G` = bottom.
            KeyCode::Char('g') if no_mods && app.vim => {
                app.scroll_top();
                clear_pending_g(app);
            }
            KeyCode::Char('G') if no_mods && app.vim => {
                app.scroll_bottom();
                clear_pending_g(app);
            }
            // Universal (both keymaps): full-size keys for the same moves.
            KeyCode::PageDown if no_mods => {
                clear_pending_g(app);
                let h = app.half_page();
                app.scroll_lines(h);
            }
            KeyCode::PageUp if no_mods => {
                clear_pending_g(app);
                let h = app.half_page();
                app.scroll_lines(-h);
            }
            KeyCode::Home if no_mods => {
                clear_pending_g(app);
                app.scroll_top();
            }
            KeyCode::End if no_mods => {
                clear_pending_g(app);
                app.scroll_bottom();
            }
            KeyCode::Char('/') if no_mods => {
                clear_pending_g(app);
                app.mode = Mode::Search;
                app.search_input.clear();
                app.search_cursor = 0;
            }
            KeyCode::Char('n') if no_mods => {
                clear_pending_g(app);
                app.search_step(1);
            }
            KeyCode::Char('N') if no_mods => {
                clear_pending_g(app);
                app.search_step(-1);
            }
            KeyCode::Char('i') | KeyCode::Char('a') if no_mods && app.vim => {
                clear_pending_g(app);
                app.mode = Mode::Insert;
            }
            // Normal keymap: Enter types (vim users keep i/a).
            KeyCode::Enter if !app.vim => {
                clear_pending_g(app);
                app.mode = Mode::Insert;
            }
            // Space types in BOTH keymaps (dx shortcut: no Enter needed).
            KeyCode::Char(' ') => {
                clear_pending_g(app);
                app.mode = Mode::Insert;
            }
            // Stop the running turn (a pending card was handled above:
            // Esc defers it instead). Idle tabs: no-op.
            KeyCode::Esc if no_mods => {
                clear_pending_g(app);
                app.request_stop();
            }
            KeyCode::Char('m') if no_mods => {
                clear_pending_g(app);
                app.mouse = !app.mouse;
                app.flash = format!("mouse {}", if app.mouse { "on" } else { "off" });
            }
            KeyCode::Char('P') if no_mods => {
                clear_pending_g(app);
                // Preselect the tab's current backend in the picker.
                let cur = app.sessions[app.active].backend;
                app.picker_sel = BackendKind::ALL
                    .iter()
                    .position(|(b, _)| *b == cur)
                    .unwrap_or(0);
                app.mode = Mode::Picker;
            }
            KeyCode::Char('R') if no_mods => {
                clear_pending_g(app);
                // Same-backend fresh respawn (the restore banner's promise).
                let cur = app.sessions[app.active].backend;
                app.respawn_active(cur);
            }
            KeyCode::Char(c)
                if no_mods && c >= '1' && (c as usize - '1' as usize) < app.sessions.len() =>
            {
                clear_pending_g(app);
                app.active = c as usize - '1' as usize;
                app.stick_to_bottom();
            }
            KeyCode::Tab if no_mods => {
                clear_pending_g(app);
                app.active = (app.active + 1) % app.sessions.len();
                app.stick_to_bottom();
            }
            _ => clear_pending_g(app),
        },
        Mode::Insert => match code {
            KeyCode::Esc => {
                app.mode = Mode::Normal;
                app.cmd_sel = 0;
            }
            // Ctrl-[ sends the same bytes as Esc on most terminals; belt & braces.
            KeyCode::Char('[') if ctrl => {
                app.mode = Mode::Normal;
                app.cmd_sel = 0;
            }
            KeyCode::Enter => app.submit(),
            // Slash-command suggestions (input starts with `/`):
            // Up/Down moves the highlight, Tab accepts it.
            KeyCode::Tab if no_mods => {
                app.accept_slash_completion();
            }
            KeyCode::Up if !app.slash_matches().is_empty() => {
                app.cycle_cmd_sel(-1);
            }
            KeyCode::Down if !app.slash_matches().is_empty() => {
                app.cycle_cmd_sel(1);
            }
            KeyCode::Backspace => {
                let s = app.active_mut();
                if s.cursor > 0 {
                    s.cursor -= 1;
                    let bi = byte_index(&s.input, s.cursor);
                    s.input.remove(bi);
                }
                app.cmd_sel = 0;
            }
            KeyCode::Left => {
                let s = app.active_mut();
                s.cursor = s.cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                let s = app.active_mut();
                let max = s.input.chars().count();
                s.cursor = (s.cursor + 1).min(max);
            }
            KeyCode::Char(c) if !ctrl => {
                let s = app.active_mut();
                let bi = byte_index(&s.input, s.cursor);
                s.input.insert(bi, c);
                s.cursor += 1;
                app.cmd_sel = 0;
            }
            _ => {}
        },
        Mode::Search => match code {
            KeyCode::Esc => {
                app.mode = Mode::Normal;
                app.flash.clear();
            }
            KeyCode::Char('[') if ctrl => {
                app.mode = Mode::Normal;
                app.flash.clear();
            }
            KeyCode::Enter => {
                app.mode = Mode::Normal;
                app.run_search();
            }
            KeyCode::Backspace => {
                if app.search_cursor > 0 {
                    app.search_cursor -= 1;
                    let bi = byte_index(&app.search_input, app.search_cursor);
                    app.search_input.remove(bi);
                }
            }
            KeyCode::Char(c) if !ctrl => {
                let bi = byte_index(&app.search_input, app.search_cursor);
                app.search_input.insert(bi, c);
                app.search_cursor += 1;
            }
            _ => {}
        },
        // First-run picker (onboarding): nothing to cancel back to, so
        // the exits quit and a choice starts the tab instead of respawning.
        Mode::Picker => match code {
            KeyCode::Esc => cancel_picker(app),
            KeyCode::Char('[') if ctrl => cancel_picker(app),
            KeyCode::Char('c') if ctrl && app.onboarding => app.should_quit = true,
            KeyCode::Char('q') if no_mods && app.onboarding => app.should_quit = true,
            KeyCode::Char('j') | KeyCode::Down if no_mods => {
                app.picker_sel = (app.picker_sel + 1) % BackendKind::ALL.len();
            }
            KeyCode::Char('k') | KeyCode::Up if no_mods => {
                app.picker_sel =
                    (app.picker_sel + BackendKind::ALL.len() - 1) % BackendKind::ALL.len();
            }
            KeyCode::Enter => pick_backend(app, BackendKind::ALL[app.picker_sel].0),
            KeyCode::Char(c) if no_mods && ['1', '2', '3', '4', '5'].contains(&c) => {
                let i = (c as usize - '1' as usize).min(BackendKind::ALL.len() - 1);
                pick_backend(app, BackendKind::ALL[i].0);
            }
            _ => {}
        },
        Mode::Sessions => match code {
            KeyCode::Esc => app.mode = Mode::Normal,
            KeyCode::Char('[') if ctrl => app.mode = Mode::Normal,
            KeyCode::Char('j') | KeyCode::Down if no_mods => {
                if !app.sess_list.is_empty() {
                    app.sess_sel = (app.sess_sel + 1) % app.sess_list.len();
                }
            }
            KeyCode::Char('k') | KeyCode::Up if no_mods => {
                if !app.sess_list.is_empty() {
                    app.sess_sel = (app.sess_sel + app.sess_list.len() - 1) % app.sess_list.len();
                }
            }
            KeyCode::Enter => app.choose_session(app.sess_sel),
            KeyCode::Char('d') if no_mods => app.delete_selected_session(),
            KeyCode::Char(c) if no_mods && c >= '1' && c <= '9' => {
                let i = c as usize - '1' as usize;
                if i < app.sess_list.len() {
                    app.choose_session(i);
                }
            }
            _ => {}
        },
    }
}

fn byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .map(|(b, _)| b)
        .nth(char_idx)
        .unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{ApprovalChoice, PendingApproval, PendingDiff};
    use crate::test_support::busy_tab;
    use crate::test_support::muse_modal_app;

    #[test]
    fn grok_unmappable_decision_queues_cancelled_and_closes_card() {
        let mut app = muse_modal_app();
        app.active_mut().backend = BackendKind::Grok;
        app.active_mut()
            .pending_approval
            .as_mut()
            .unwrap()
            .requirement_id = serde_json::json!(42);
        decide_ui(&mut app, DecisionKind::ApproveAll, "approved-all");
        assert!(app.active().pending_approval.is_none());
        assert!(app.active().pending_diff.is_none());
        assert!(app.flash.contains("no matching option"));
        assert_eq!(app.outbox.decides.len(), 1);
        assert_eq!(app.outbox.decides[0].requirement_id, 42);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&app.outbox.decides[0].choice_id).unwrap()["outcome"]
                ["outcome"],
            "cancelled"
        );
    }

    const NONE: KeyModifiers = KeyModifiers::empty();

    const SHIFT: KeyModifiers = KeyModifiers::SHIFT;

    /// S2-F2: with no deny choice offered, q/Esc must NOT close the
    /// card — a silent close fakes a denial the server never received.
    /// The card stays open with a loud flash; y still escapes via the
    /// offered approve choice, so the user is never trapped.
    #[test]
    fn q_keeps_modal_open_without_deny_choice() {
        let mut app = muse_modal_app();
        handle_key(&mut app, KeyCode::Char('q'), NONE);
        assert!(
            app.active().pending_diff.is_some(),
            "q silently closed a deny-less card"
        );
        assert!(app.outbox.decides.is_empty(), "q must not send a decision");
        assert!(
            app.flash.contains("no deny path"),
            "flash must name the missing deny path"
        );
        let mut app = muse_modal_app();
        handle_key(&mut app, KeyCode::Esc, NONE);
        assert!(
            app.active().pending_diff.is_some(),
            "Esc silently closed a deny-less card"
        );
        assert!(
            app.outbox.decides.is_empty(),
            "Esc must not send a decision"
        );
        // The offered approve path still closes (fail-closed: y queues
        // the allow choice; n/q never fabricate a denial).
        let mut app = muse_modal_app();
        handle_key(&mut app, KeyCode::Char('n'), NONE);
        assert!(
            app.active().pending_diff.is_some(),
            "n silently closed a deny-less card"
        );
        assert!(app.outbox.decides.is_empty(), "n must not send a decision");
    }

    /// The happy path in muse mode: y maps to the allow choice and queues
    /// exactly one wire decision.
    #[test]
    fn y_queues_allow_decision_in_muse_mode() {
        let mut app = muse_modal_app();
        handle_key(&mut app, KeyCode::Char('y'), NONE);
        assert!(app.active().pending_diff.is_none());
        assert_eq!(app.outbox.decides.len(), 1);
        assert_eq!(app.outbox.decides[0].choice_id, "c-allow");
    }

    /// ApproveAll with only once-approved offered: local-close, no wire send.
    #[test]
    fn approve_all_without_session_choice_closes_locally() {
        let mut app = muse_modal_app();
        handle_key(&mut app, KeyCode::Char('a'), NONE);
        assert!(app.active().pending_diff.is_none());
        assert!(app.outbox.decides.is_empty());
        assert!(!app.flash.is_empty());
    }

    /// Codex q/Later queues wire `denied` when choices exist.
    #[test]
    fn codex_later_queues_denied() {
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Codex;
        app.active_mut().stage_diff(PendingDiff {
            file: "applyPatch".into(),
            body: "body".into(),
        });
        app.active_mut().pending_approval = Some(PendingApproval {
            approval_id: "applyPatchApproval".into(),
            requirement_id: serde_json::json!(7),
            choices: vec![
                ApprovalChoice {
                    choice_id: "approved".into(),
                    decision: "approved".into(),
                    scope: "once".into(),
                    label: "Allow".into(),
                    accepts_feedback: false,
                },
                ApprovalChoice {
                    choice_id: "approved_for_session".into(),
                    decision: "approved_for_session".into(),
                    scope: "once".into(),
                    label: "Allow for session".into(),
                    accepts_feedback: false,
                },
                ApprovalChoice {
                    choice_id: "denied".into(),
                    decision: "denied".into(),
                    scope: "once".into(),
                    label: "Deny".into(),
                    accepts_feedback: false,
                },
            ],
        });
        handle_key(&mut app, KeyCode::Char('q'), NONE);
        assert!(app.active().pending_diff.is_none());
        assert_eq!(app.outbox.decides.len(), 1);
        assert_eq!(app.outbox.decides[0].choice_id, "denied");
    }

    /// P opens the picker; j/k move; Enter respawns the tab fresh.
    #[test]
    fn picker_respawns_tab_fresh() {
        let mut app = App::new();
        handle_key(&mut app, KeyCode::Char('P'), SHIFT);
        assert_eq!(app.mode, Mode::Picker);
        handle_key(&mut app, KeyCode::Char('j'), NONE);
        handle_key(&mut app, KeyCode::Char('j'), NONE);
        assert_eq!(app.picker_sel, 2); // mock -> muse -> codex
        handle_key(&mut app, KeyCode::Enter, NONE);
        assert_eq!(app.mode, Mode::Normal);
        let s = app.active();
        assert_eq!(s.backend, BackendKind::Codex);
        assert!(s.remote_id.is_none());
        assert_eq!(s.lines.len(), 1); // marker only: history never carries over
        assert!(s.lines[0].contains("fresh"));
        assert_eq!(app.outbox.respawns.len(), 1);
        assert_eq!(app.outbox.respawns[0].backend, BackendKind::Codex);
    }

    /// R respawns the active tab fresh under its current backend.
    #[test]
    fn r_respawns_active_tab_same_backend() {
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Codex;
        app.active_mut().remote_id = Some("thread-1".into());
        app.active_mut().push_line("old history".to_string());
        handle_key(&mut app, KeyCode::Char('R'), SHIFT);
        assert_eq!(app.mode, Mode::Normal);
        let s = app.active();
        assert_eq!(s.backend, BackendKind::Codex);
        assert!(s.remote_id.is_none());
        assert_eq!(s.lines.len(), 1); // marker only: history never carries over
        assert!(s.lines[0].contains("fresh"));
        assert_eq!(app.outbox.respawns.len(), 1);
        assert_eq!(app.outbox.respawns[0].backend, BackendKind::Codex);
    }

    /// Picker Esc cancels without touching the tab.
    #[test]
    fn picker_esc_cancels() {
        let mut app = App::new();
        handle_key(&mut app, KeyCode::Char('P'), SHIFT);
        handle_key(&mut app, KeyCode::Esc, NONE);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.active().backend, BackendKind::Mock);
        assert!(app.outbox.respawns.is_empty());
    }

    /// Onboarding picker: j/k still move, but Esc (and q) quit — there
    /// is no backend to cancel back to.
    #[test]
    fn onboarding_picker_exits_quit() {
        for key in [KeyCode::Esc, KeyCode::Char('q')] {
            let mut app = App::new();
            app.onboarding = true;
            app.mode = Mode::Picker;
            handle_key(&mut app, KeyCode::Char('j'), NONE);
            assert_eq!(app.picker_sel, 1);
            handle_key(&mut app, key, NONE);
            assert!(app.should_quit, "{key:?} quits during onboarding");
            assert!(app.outbox.respawns.is_empty());
        }
    }

    /// Agy submit queues unconditionally (no session id needed up front;
    /// the drain writes into the tab child's stdin).
    #[test]
    fn agy_submit_queues_without_session_id() {
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Agy;
        app.active_mut().input = "hi".to_string();
        app.submit();
        assert_eq!(app.outbox.submits.len(), 1);
        assert_eq!(app.outbox.submits[0].backend, BackendKind::Agy);
        assert!(app.active().busy);
    }

    /// Sessions chooser: j/k move, out-of-range digits are a no-op,
    /// Esc cancels without touching the tab.
    #[test]
    fn sessions_keys_navigate_and_cancel() {
        fn stored(id: &str) -> crate::store::StoredSession {
            crate::store::StoredSession {
                id: id.to_string(),
                backend: "mock".to_string(),
                remote_id: None,
                created_at: 1000,
                updated_at: 1000,
                title: String::new(),
                preview: "preview".to_string(),
            }
        }
        let mut app = App::new();
        app.sess_list = vec![stored("a"), stored("b")];
        app.mode = Mode::Sessions;
        handle_key(&mut app, KeyCode::Char('j'), NONE);
        assert_eq!(app.sess_sel, 1);
        handle_key(&mut app, KeyCode::Char('k'), NONE);
        assert_eq!(app.sess_sel, 0);
        handle_key(&mut app, KeyCode::Char('9'), NONE);
        assert_eq!(app.mode, Mode::Sessions, "out-of-range digit is a no-op");
        assert!(app.outbox.respawns.is_empty());
        handle_key(&mut app, KeyCode::Esc, NONE);
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.outbox.respawns.is_empty());
    }

    /// Digit tabs follow the live tab count (boot = 1 tab).
    #[test]
    fn digit_tabs_are_dynamic() {
        let mut app = App::new();
        handle_key(&mut app, KeyCode::Char('2'), NONE);
        assert_eq!(app.active, 0, "no second tab yet: digit ignored");
        app.open_tab();
        handle_key(&mut app, KeyCode::Char('2'), NONE);
        assert_eq!(app.active, 1);
        handle_key(&mut app, KeyCode::Char('1'), NONE);
        assert_eq!(app.active, 0);
    }

    /// Picker reaches all five backends.
    #[test]
    fn picker_lists_five_backends() {
        let mut app = App::new();
        handle_key(&mut app, KeyCode::Char('P'), SHIFT);
        handle_key(&mut app, KeyCode::Char('4'), NONE);
        assert_eq!(app.active().backend, BackendKind::Agy);
        assert_eq!(app.outbox.respawns.len(), 1);
        handle_key(&mut app, KeyCode::Char('P'), SHIFT);
        handle_key(&mut app, KeyCode::Char('5'), NONE);
        assert_eq!(app.active().backend, BackendKind::Grok);
        // Same tab re-picked: the stale Agy respawn is dropped, one Grok
        // respawn queued.
        assert_eq!(app.outbox.respawns.len(), 1);
        assert_eq!(app.outbox.respawns[0].backend, BackendKind::Grok);
    }

    /// Submit on a grok tab queues a backend-tagged submit (the drain
    /// spawns session/prompt); submit on a dead tab reports, never hangs.
    #[test]
    fn submit_routes_grok_tab() {
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Grok;
        app.active_mut().remote_id = Some("acp-sess-1".into());
        app.active_mut().input = "hi".to_string();
        app.submit();
        assert_eq!(app.outbox.submits.len(), 1);
        assert_eq!(app.outbox.submits[0].backend, BackendKind::Grok);
        assert!(app.active().busy);

        let mut dead = App::new();
        dead.active_mut().backend = BackendKind::Grok;
        dead.active_mut().input = "hi".to_string();
        dead.submit();
        assert!(dead.outbox.submits.is_empty());
        assert!(!dead.active().busy);
        assert!(dead.active().lines.iter().any(|l| l.contains("grok")));
    }

    /// Submit on a codex tab queues a backend-tagged submit (async drain
    /// sends turn/start); submit on a dead tab reports instead of hanging.
    #[test]
    fn submit_routes_per_tab_backend() {
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Codex;
        app.active_mut().remote_id = Some("thread-1".into());
        app.active_mut().input = "hi".to_string();
        app.submit();
        assert_eq!(app.outbox.submits.len(), 1);
        assert_eq!(app.outbox.submits[0].backend, BackendKind::Codex);
        assert!(app.active().busy);

        let mut dead = App::new();
        dead.active_mut().backend = BackendKind::Codex;
        dead.active_mut().input = "hi".to_string();
        dead.submit();
        assert!(dead.outbox.submits.is_empty());
        assert!(!dead.active().busy);
        assert!(dead.active().lines.iter().any(|l| l.contains("codex")));
    }

    /// Scrollable app: production tabs open empty, so tests seed filler.
    fn scroll_app(vim: bool) -> App {
        let mut app = App::new();
        crate::mock::seed(&mut app.sessions[0]);
        app.vim = vim;
        app.viewport_height = 20;
        app.stick_to_bottom();
        app
    }

    /// Crossterm delivers `G` as Char('G')+SHIFT; it must jump to bottom.
    #[test]
    fn shift_g_goes_to_bottom() {
        let mut app = scroll_app(true);
        let max = app.active().lines.len().saturating_sub(20);
        handle_key(&mut app, KeyCode::Char('g'), NONE);
        assert_eq!(app.active().scroll, 0);
        handle_key(&mut app, KeyCode::Char('G'), SHIFT);
        assert_eq!(
            app.active().scroll,
            max,
            "Shift+G ignored: scroll={} max={}",
            app.active().scroll,
            max
        );
    }

    /// Plain keys keep working alongside the Shift tolerance.
    #[test]
    fn plain_keys_unaffected() {
        let mut app = scroll_app(true);
        let max = app.active().lines.len().saturating_sub(20);
        handle_key(&mut app, KeyCode::Char('k'), NONE);
        assert_eq!(app.active().scroll, max - 1);
        handle_key(&mut app, KeyCode::Char('n'), NONE); // no search: flash, no panic
        assert!(!app.flash.is_empty());
        handle_key(&mut app, KeyCode::Char('q'), NONE);
        assert!(app.should_quit);
    }

    /// Default keymap is NOT vim: j/k/g/G/i/a do nothing, Enter types.
    #[test]
    fn normal_keymap_ignores_vim_keys() {
        let mut app = App::new();
        assert!(!app.vim);
        app.viewport_height = 20;
        app.stick_to_bottom();
        let scroll = app.active().scroll;
        for c in ['j', 'k', 'g', 'G', 'i', 'a'] {
            handle_key(&mut app, KeyCode::Char(c), NONE);
        }
        assert_eq!(app.active().scroll, scroll);
        assert_eq!(app.mode, Mode::Normal);
        handle_key(&mut app, KeyCode::Enter, NONE);
        assert_eq!(app.mode, Mode::Insert, "Enter types in the normal keymap");
    }

    /// Space enters insert in BOTH keymaps (dx shortcut).
    #[test]
    fn space_types_in_both_keymaps() {
        for vim in [false, true] {
            let mut app = App::new();
            app.vim = vim;
            handle_key(&mut app, KeyCode::Char(' '), NONE);
            assert_eq!(app.mode, Mode::Insert, "vim={vim}: Space must type");
        }
    }

    /// Vim keymap: i/a type, Enter does NOT enter insert.
    #[test]
    fn vim_keymap_types_with_i_a_only() {
        let mut app = App::new();
        app.vim = true;
        handle_key(&mut app, KeyCode::Enter, NONE);
        assert_eq!(app.mode, Mode::Normal, "Enter must not type in vim mode");
        handle_key(&mut app, KeyCode::Char('i'), NONE);
        assert_eq!(app.mode, Mode::Insert);
    }

    /// A command typed in Normal mode lands in Search; a miss that looks
    /// like a command must redirect to Insert mode instead of silence.
    #[test]
    fn failed_search_matching_a_command_redirects_to_insert() {
        let mut app = App::new();
        app.mode = Mode::Search;
        app.search_input = "tab close".to_string();
        app.run_search();
        assert!(app.flash.contains("INSERT mode"), "flash: {}", app.flash);
        assert!(app.flash.contains("/tab close"), "flash: {}", app.flash);
        // Ordinary misses keep the plain message (no false redirect).
        app.search_input = "zzz-no-such-line".to_string();
        app.run_search();
        assert_eq!(app.flash, "no match: zzz-no-such-line");
    }

    /// Insert mode: typing `/` narrows suggestions, Up/Down moves the
    /// highlight, Tab accepts it, Enter still sends.
    #[test]
    fn slash_suggestions_complete_via_tab() {
        let mut app = App::new();
        app.mode = Mode::Insert;
        handle_key(&mut app, KeyCode::Char('/'), NONE);
        handle_key(&mut app, KeyCode::Char('t'), NONE);
        assert_eq!(app.active().input, "/t");
        assert_eq!(app.slash_matches().len(), 3);
        handle_key(&mut app, KeyCode::Down, NONE);
        assert_eq!(app.cmd_sel, 1);
        handle_key(&mut app, KeyCode::Up, NONE);
        assert_eq!(app.cmd_sel, 0);
        handle_key(&mut app, KeyCode::Tab, NONE);
        assert_eq!(app.active().input, "/tab new");
        assert_eq!(app.mode, Mode::Insert, "Tab completes, it does not send");
        // Plain text: Up/Down/Tab leave the input alone.
        app.active_mut().input = "hi".to_string();
        app.active_mut().cursor = 2;
        handle_key(&mut app, KeyCode::Up, NONE);
        handle_key(&mut app, KeyCode::Down, NONE);
        handle_key(&mut app, KeyCode::Tab, NONE);
        assert_eq!(app.active().input, "hi");
    }

    /// Home/End/PageUp/PageDown scroll in BOTH keymaps.
    #[test]
    fn fullsize_nav_keys_are_universal() {
        for vim in [false, true] {
            let mut app = scroll_app(vim);
            let max = app.active().lines.len().saturating_sub(20);
            assert!(max > 0, "precondition: scrollable seed");
            handle_key(&mut app, KeyCode::Home, NONE);
            assert_eq!(app.active().scroll, 0);
            handle_key(&mut app, KeyCode::End, NONE);
            assert_eq!(app.active().scroll, max);
            handle_key(&mut app, KeyCode::PageUp, NONE);
            assert!(app.active().scroll < max);
            handle_key(&mut app, KeyCode::PageDown, NONE);
            assert_eq!(app.active().scroll, max);
        }
    }

    /// Esc in Normal mode stops a busy tab (one queued stop) and does
    /// nothing on an idle one.
    #[test]
    fn esc_in_normal_mode_queues_a_stop_on_a_busy_tab() {
        let mut app = busy_tab(BackendKind::Grok);
        handle_key(&mut app, KeyCode::Esc, NONE);
        assert_eq!(app.outbox.stops.len(), 1);
        assert_eq!(app.outbox.stops[0].backend, BackendKind::Grok);
        assert!(app.active().stopping.is_some());
        let mut idle = App::new();
        handle_key(&mut idle, KeyCode::Esc, NONE);
        assert!(idle.outbox.stops.is_empty() && idle.active().stopping.is_none());
    }

    /// With a card open, Esc keeps deferring it and never stops the turn.
    #[test]
    fn esc_with_pending_card_still_defers_not_stops() {
        let mut app = muse_modal_app();
        app.active_mut().busy = true;
        handle_key(&mut app, KeyCode::Esc, NONE);
        assert!(app.outbox.stops.is_empty());
        assert!(app.active().stopping.is_none());
        assert!(app.flash.contains("no deny path"), "{}", app.flash);
    }
}
