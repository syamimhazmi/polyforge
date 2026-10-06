//! Test fixtures shared by more than one `app` submodule's tests.

use super::App;

/// Serializes the tests that mutate `XDG_CONFIG_HOME` (`/vim` and
/// `/theme` persistence): parallel mutation would redirect each
/// other's save/read mid-test.
pub(super) static CONFIG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(super) fn lock_config_env() -> std::sync::MutexGuard<'static, ()> {
    CONFIG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

pub(super) const VH: usize = 20; // fixed viewport

pub(super) fn bottom_app() -> App {
    let mut app = App::new();
    // Production tabs open empty; scroll tests seed their own filler.
    crate::mock::seed(&mut app.sessions[0]);
    app.viewport_height = VH;
    app.viewport_width = 100;
    app.active_mut().ensure_cache(100);
    // Place the viewport explicitly: must not depend on stick_to_bottom().
    let tail = app.active().total_rows.saturating_sub(VH);
    app.active_mut().scroll = tail;
    app
}

pub(super) fn test_store(name: &str) -> (crate::store::Store, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("pf-app-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = crate::store::Store::open_in(dir.clone()).expect("open_in");
    (store, dir)
}
