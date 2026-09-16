//! Clipboard copy with grok-build parity: one release fires every active
//! leg, and a backup file is always written.
//!
//! Legs (all best-effort, bounded waits — a wedged helper must never hang
//! the event loop):
//! 1. `native` — always attempted. macOS: `pbcopy` with stdin from a
//!    spooled 0600 temp file (never a pipe: a stalled helper must not
//!    block the writer) and a 2 s deadline. Other platforms: no native
//!    leg (grok uses arboard/CLI tools there; we add no dependency).
//! 2. `tmux_buffer` — only inside tmux: `tmux load-buffer -`, spooled
//!    stdin, 2 s deadline.
//! 3. `osc52` — toward the outer terminal on stderr. Always on Linux;
//!    on macOS only inside tmux, SSH, a display-less container, or when a
//!    wrap sink advertises (`POLYFORGE_OSC52_SINK` / `LC_POLYFORGE_OSC52_SINK`).
//!    Off everywhere with `POLYFORGE_CLIPBOARD_NO_OSC52`. The tmux DCS
//!    envelope applies only when tmux is the immediate terminal.
//!
//! Backup file: always attempted at `POLYFORGE_COPY_FILE` or
//! `$XDG_DATA_HOME/polyforge/last-copy.txt` (mode 0600, parents 0700), so
//! a copy survives every backend failing. The status flash names the path
//! when the clipboard legs all miss.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long one helper (`pbcopy`, `tmux load-buffer`) may run before it is
/// killed (mirrors grok's bounded clipboard waits).
const HELPER_DEADLINE: Duration = Duration::from_secs(2);

/// Ambient environment read once per copy (cheap env reads; no caching —
/// tests inject [`Env`] directly instead of mutating the process env).
#[derive(Debug, Clone, Copy)]
pub struct Env {
    pub linux: bool,
    pub tmux: bool,
    pub ssh: bool,
    pub container_no_display: bool,
    pub wrap_sink: bool,
    pub no_osc52: bool,
}

impl Env {
    pub fn ambient() -> Self {
        Self {
            linux: cfg!(target_os = "linux"),
            tmux: std::env::var_os("TMUX").is_some(),
            ssh: std::env::var_os("SSH_CONNECTION").is_some()
                || std::env::var_os("SSH_TTY").is_some()
                || std::env::var_os("SSH_CLIENT").is_some(),
            container_no_display: container_no_display(),
            wrap_sink: std::env::var_os("POLYFORGE_OSC52_SINK").is_some()
                || std::env::var_os("LC_POLYFORGE_OSC52_SINK").is_some(),
            no_osc52: std::env::var_os("POLYFORGE_CLIPBOARD_NO_OSC52").is_some(),
        }
    }
}

/// No display server plus a container sentinel: native clipboard cannot
/// work, OSC 52 is the only path.
fn container_no_display() -> bool {
    if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return false;
    }
    std::path::Path::new("/.dockerenv").exists()
        || std::path::Path::new("/run/.containerenv").exists()
        || std::env::var_os("container").is_some()
}

/// Which legs fire for one copy (pure: unit-testable without side effects).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub native: bool,
    pub tmux_buffer: bool,
    pub osc52: bool,
    pub osc52_tmux_passthrough: bool,
}

impl std::fmt::Display for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        for (flag, label) in [
            (self.native, "native"),
            (self.tmux_buffer, "tmux"),
            (self.osc52, "osc52"),
        ] {
            if flag {
                if !first {
                    f.write_str("+")?;
                }
                f.write_str(label)?;
                first = false;
            }
        }
        Ok(())
    }
}

pub fn resolve_route(env: &Env) -> Route {
    // Linux always emits OSC 52; macOS only in tmux/SSH/container or with
    // a wrap sink. The kill switch wins over every automatic path.
    let osc52 = !env.no_osc52
        && (env.linux || env.tmux || env.ssh || env.container_no_display || env.wrap_sink);
    Route {
        native: true,
        tmux_buffer: env.tmux,
        osc52,
        osc52_tmux_passthrough: osc52 && env.tmux,
    }
}

/// Where a copy landed (mirrors grok's delivery vocabulary).
#[derive(Debug)]
pub enum CopyDelivery {
    Clipboard { legs: String, file: Option<PathBuf> },
    File { path: PathBuf },
    Failed,
}

impl CopyDelivery {
    pub fn toast_message(&self) -> String {
        match self {
            Self::Clipboard { legs, file } => {
                let has = |leg: &str| legs.split('+').any(|l| l == leg);
                if has("native") {
                    "Copied!".to_string()
                } else if has("tmux") {
                    "Copied to tmux buffer, paste with prefix + ]".to_string()
                } else if let Some(path) = file {
                    // Unverified OSC-52-only landing names the backup path.
                    format!("Copied via OSC 52, saved to {}", abbreviate(path))
                } else {
                    "Copied via OSC 52.".to_string()
                }
            }
            Self::File { path } => {
                format!("Clipboard unreachable: wrote {}", abbreviate(path))
            }
            Self::Failed => "Copy failed (clipboard and backup file)".to_string(),
        }
    }
}

fn abbreviate(path: &PathBuf) -> String {
    let s = path.to_string_lossy();
    if let Some(home) = std::env::var_os("HOME") {
        let h = home.to_string_lossy();
        if let Some(rest) = s.strip_prefix(h.as_ref()) {
            return format!("~{rest}");
        }
    }
    s.into_owned()
}

/// Copy entry point: every active leg fires, the backup file is always
/// attempted, and the returned delivery says where the text landed.
pub fn copy_text_or_file(text: &str) -> CopyDelivery {
    let route = resolve_route(&Env::ambient());
    let mut legs = Vec::new();
    if route.native && native_copy(text) {
        legs.push("native");
    }
    if route.tmux_buffer && tmux_copy(text) {
        legs.push("tmux");
    }
    if route.osc52 {
        emit_osc52(text, route.osc52_tmux_passthrough);
        legs.push("osc52");
    }
    let file = write_copy_fallback(text).ok();
    if legs.is_empty() {
        return match file {
            Some(path) => CopyDelivery::File { path },
            None => CopyDelivery::Failed,
        };
    }
    CopyDelivery::Clipboard {
        legs: legs.join("+"),
        file,
    }
}

/// macOS `pbcopy`, stdin from a spooled 0600 temp file (a pipe write from
/// the event loop would block past ~64 KiB and a wedged helper needs stdin
/// already closed for the deadline wait). 2 s deadline, then kill.
#[cfg(target_os = "macos")]
fn native_copy(text: &str) -> bool {
    let Ok(spooled) = spool_stdin(text.as_bytes()) else {
        return false;
    };
    let Ok(stdin) = spooled.reopen() else {
        return false;
    };
    let mut cmd = Command::new("pbcopy");
    cmd.stdin(Stdio::from(stdin))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    // stdin is a complete file, not a pipe: nothing to close, and the
    // deadline wait below can't block on a half-open writer.
    let _ = spooled;
    match wait_with_deadline(&mut child, HELPER_DEADLINE) {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

/// No native leg off-macOS (no new dependency for arboard/CLI tools).
#[cfg(not(target_os = "macos"))]
fn native_copy(_text: &str) -> bool {
    false
}

/// `tmux load-buffer -`, spooled stdin, bounded wait.
fn tmux_copy(text: &str) -> bool {
    let Ok(spooled) = spool_stdin(text.as_bytes()) else {
        return false;
    };
    let Ok(stdin) = spooled.reopen() else {
        return false;
    };
    let mut cmd = Command::new("tmux");
    cmd.args(["load-buffer", "-"])
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    let _ = spooled;
    match wait_with_deadline(&mut child, HELPER_DEADLINE) {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

/// Wait with a deadline; kill on expiry. Callers must have closed stdin
/// first (a child still reading a held pipe would burn the deadline).
fn wait_with_deadline(
    child: &mut std::process::Child,
    deadline: Duration,
) -> std::io::Result<std::process::ExitStatus> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if start.elapsed() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "helper did not exit in time",
            ));
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// Spool bytes to a 0600 temp file and return it open for reading.
/// Unique per process (pid + nanos + counter): no tempfile crate needed.
fn spool_stdin(data: &[u8]) -> std::io::Result<Spooled> {
    use std::os::unix::fs::OpenOptionsExt;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "pf-clipboard-{}-{nanos}-{seq}",
        std::process::id()
    ));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    f.write_all(data)?;
    drop(f);
    let read = std::fs::File::open(&path)?;
    let _ = std::fs::remove_file(&path);
    Ok(Spooled { file: read })
}

struct Spooled {
    file: std::fs::File,
}

impl Spooled {
    fn reopen(&self) -> std::io::Result<std::fs::File> {
        self.file.try_clone()
    }
}

/// OSC 52 toward the outer terminal on stderr (the terminal output
/// stream), tmux-wrapped only when tmux is the immediate terminal.
fn emit_osc52(text: &str, tmux_passthrough: bool) {
    let seq = osc52_seq(text, tmux_passthrough);
    let _ = std::io::stderr().write_all(seq.as_bytes());
    let _ = std::io::stderr().flush();
}

/// OSC 52 set-clipboard sequence. Pure for testing.
pub fn osc52_seq(text: &str, tmux_passthrough: bool) -> String {
    let inner = format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()));
    if tmux_passthrough {
        format!("\x1bPtmux;{}\x1b\\", inner.replace('\x1b', "\x1b\x1b"))
    } else {
        inner
    }
}

/// Minimal base64 (hand-rolled: no new dependency for one escape sequence).
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut n = 0u32;
        for (i, &b) in chunk.iter().enumerate() {
            n |= (b as u32) << (16 - 8 * i);
        }
        let pad = 3 - chunk.len();
        let mut quad = [0u8; 4];
        for i in 0..4 {
            quad[i] = ALPHA[((n >> (18 - 6 * i)) & 0x3f) as usize];
        }
        for i in 0..pad {
            quad[3 - i] = b'=';
        }
        out.push_str(std::str::from_utf8(&quad).unwrap_or("===="));
    }
    out
}

/// Backup-file path: `POLYFORGE_COPY_FILE`, else the data dir. Never a
/// world-visible temp path: no home resolvable means no file (NotFound).
fn fallback_path() -> std::io::Result<PathBuf> {
    if let Ok(raw) = std::env::var("POLYFORGE_COPY_FILE") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let expanded = if let Some(home) = std::env::var_os("HOME") {
                let h = home.to_string_lossy();
                match trimmed.strip_prefix('~') {
                    Some(rest) => format!("{h}{rest}"),
                    None => trimmed.to_string(),
                }
            } else {
                trimmed.to_string()
            };
            return Ok(PathBuf::from(expanded));
        }
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share"))
        })
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no home directory resolves; set POLYFORGE_COPY_FILE to enable the copy backup file",
            )
        })?;
    Ok(base.join("polyforge").join("last-copy.txt"))
}

/// Always-attempted backup write (mode 0600, parents 0700: copied text can
/// be sensitive and the default path is predictable).
fn write_copy_fallback(text: &str) -> std::io::Result<PathBuf> {
    let path = fallback_path()?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            create_private_dir(parent)?;
        }
    }
    open_private_file(&path)?.write_all(text.as_bytes())?;
    // Tighten pre-existing perms too (create-time mode is not enough).
    tighten_private_file(&path)?;
    Ok(path)
}

#[cfg(unix)]
fn create_private_dir(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

#[cfg(not(unix))]
fn create_private_dir(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

#[cfg(unix)]
fn open_private_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_private_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

#[cfg(unix)]
fn tighten_private_file(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn tighten_private_file(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(
        linux: bool,
        tmux: bool,
        ssh: bool,
        container_no_display: bool,
        wrap_sink: bool,
        no_osc52: bool,
    ) -> Env {
        Env {
            linux,
            tmux,
            ssh,
            container_no_display,
            wrap_sink,
            no_osc52,
        }
    }

    #[test]
    fn route_matrix_matches_grok_contract() {
        // Plain macOS terminal: native only.
        let r = resolve_route(&env(false, false, false, false, false, false));
        assert_eq!(
            r,
            Route {
                native: true,
                tmux_buffer: false,
                osc52: false,
                osc52_tmux_passthrough: false,
            }
        );
        // macOS in tmux: all three legs + passthrough envelope.
        let r = resolve_route(&env(false, true, false, false, false, false));
        assert!(r.native && r.tmux_buffer && r.osc52 && r.osc52_tmux_passthrough);
        assert_eq!(r.to_string(), "native+tmux+osc52");
        // macOS over SSH: native + plain OSC 52 (no tmux to wrap or fill).
        let r = resolve_route(&env(false, false, true, false, false, false));
        assert!(r.native && !r.tmux_buffer && r.osc52 && !r.osc52_tmux_passthrough);
        // Display-less container: OSC 52 joins even without tmux/SSH.
        let r = resolve_route(&env(false, false, false, true, false, false));
        assert!(r.osc52);
        // Wrap sink advertises: OSC 52 joins on plain macOS too.
        let r = resolve_route(&env(false, false, false, false, true, false));
        assert!(r.osc52);
        // Kill switch beats every automatic path.
        let r = resolve_route(&env(true, true, true, true, true, true));
        assert!(!r.osc52 && !r.osc52_tmux_passthrough);
        assert!(r.native && r.tmux_buffer);
        // Linux always emits OSC 52.
        let r = resolve_route(&env(true, false, false, false, false, false));
        assert!(r.osc52 && !r.osc52_tmux_passthrough);
    }

    #[test]
    fn toast_names_the_landing() {
        let clip = |legs: &str| CopyDelivery::Clipboard {
            legs: legs.to_string(),
            file: Some(PathBuf::from("/tmp/x.txt")),
        };
        assert_eq!(clip("native").toast_message(), "Copied!");
        assert_eq!(clip("native+tmux+osc52").toast_message(), "Copied!");
        assert_eq!(
            clip("tmux").toast_message(),
            "Copied to tmux buffer, paste with prefix + ]"
        );
        assert!(clip("osc52")
            .toast_message()
            .starts_with("Copied via OSC 52, saved to "));
        assert_eq!(
            CopyDelivery::Clipboard {
                legs: "osc52".to_string(),
                file: None,
            }
            .toast_message(),
            "Copied via OSC 52."
        );
        assert!(CopyDelivery::File {
            path: PathBuf::from("/tmp/x.txt"),
        }
        .toast_message()
        .starts_with("Clipboard unreachable: wrote "));
    }

    #[test]
    fn base64_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode("hello ✓".as_bytes()), "aGVsbG8g4pyT");
    }

    #[test]
    fn osc52_framing_matches_grok_vectors() {
        // Byte-identical to grok-build's osc52_sequence test vectors.
        assert_eq!(osc52_seq("hi", false), "\x1b]52;c;aGk=\x07");
        assert_eq!(
            osc52_seq("hi", true),
            "\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\"
        );
    }
}
