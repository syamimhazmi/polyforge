//! M1 mock provider (throwaway). M2 replaces this with the Muse Spark
//! CLI-wrap; the `start_job` / streaming shape is what the real provider
//! must slot into.

use crate::app::PendingDiff;
use crate::app::Session;

pub fn seed(s: &mut Session) {
    for i in 1..=60 {
        s.push_line(format!("{} seed line {i:02}: scroll me with j/k, C-u/d, g/G", s.name));
    }
    s.push_line(format!("{} try: i<type><Enter> for a mock streaming reply", s.name));
    s.push_line(format!("{} try: /seed<Enter> then n/N to walk matches", s.name));
}

pub fn start_job(s: &mut Session, prompt: &str) {
    s.busy = true;
    let mut q = Vec::new();
    q.push(format!("mock: working on {prompt:?} …"));
    for i in 1..=36 {
        let body = match i % 6 {
            0 => format!("mock:   consider edge case {i} (tabs stream in background)"),
            1 => format!("mock:   fn draft_{i}() {{ todo!() }} // line {i}"),
            2 => format!("mock:   test plan item {i}: scroll stays put while reading"),
            _ => format!("mock:   step {i:02}/36 reasoning…"),
        };
        q.push(body);
    }
    q.push("mock: proposing 1 file — approve with y/n/a/q".to_string());
    s.queue = q;
    s.diff_after = Some(PendingDiff {
        file: "src/draft.rs".to_string(),
        body: "--- a/src/draft.rs\n+++ b/src/draft.rs\n@@\n+pub fn draft() {\n+    todo!(\"from mock job\")\n+}\n".to_string(),
    });
}
