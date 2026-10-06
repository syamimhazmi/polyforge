//! Outbox: async provider work queued by the synchronous `App` and performed
//! by the main loop.

use super::BackendKind;

/// Async provider work queued by the sync App, performed by the main loop.
/// `requirement_id`/`choice_id` are repurposed per backend: muse uses them
/// as the MSP requirement guard + choice id; codex uses `requirement_id`
/// for the server request id and `choice_id` for the decision word.
#[derive(Debug, Default)]
pub struct OutboxSubmit {
    pub tab: usize,
    pub backend: BackendKind,
    pub prompt: String,
}

#[derive(Debug, Default)]
pub struct OutboxDecide {
    pub tab: usize,
    pub backend: BackendKind,
    pub approval_id: String,
    pub requirement_id: serde_json::Value,
    pub choice_id: String,
    pub feedback: Option<String>,
}

#[derive(Debug, Default)]
pub struct OutboxRespawn {
    pub tab: usize,
    pub backend: BackendKind,
}

/// Stop request, snapshotted at press time so a later tab close / respawn
/// cannot retarget it (the drain uses these ids, not the live session).
#[derive(Debug, Default)]
pub struct OutboxStop {
    pub tab: usize,
    pub backend: BackendKind,
    pub remote_id: Option<String>,
    pub turn_id: Option<String>,
}

#[derive(Debug, Default)]
pub struct Outbox {
    pub submits: Vec<OutboxSubmit>,
    pub decides: Vec<OutboxDecide>,
    pub respawns: Vec<OutboxRespawn>,
    pub stops: Vec<OutboxStop>,
}

/// Transcript line when a stop lands (turn ended after a stop request).
pub const STOPPED_LINE: &str = "■ stopped";

/// Second stop press: the backend never ended the turn, so give up locally.
pub const FORCED_STOP_LINE: &str = "■ stopped (forced; late output may still arrive)";
