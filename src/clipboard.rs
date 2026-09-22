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
//!    OSC 52 never fires for payloads over [`MAX_OSC52_RAW_BYTES`] raw
//!    bytes: a huge selection still lands in the native/tmux legs and the
//!    backup file, but no multi-megabyte escape blob is sprayed at the
//!    outer terminal.
//!
//! Backup file: attempted at `POLYFORGE_COPY_FILE` or
//! `$XDG_DATA_HOME/polyforge/last-copy.txt` (mode 0600, parents 0700), so
//! a copy survives every backend failing. The status flash names the path
//! when the clipboard legs all miss. Skip it entirely with
//! `POLYFORGE_CLIPBOARD_NO_BACKUP`.
//!
//! Backup-file policy (Stage 5): single-file overwrite, no rotation
//! history — each copy truncates the same path, so at most one copy's
//! worth of text is ever retained. Wipe it by deleting the file
//! (`rm "$XDG_DATA_HOME/polyforge/last-copy.txt"`, or whatever
//! `POLYFORGE_COPY_FILE` points at); the spool files used for helper
//! stdin are unlinked at creation and leave nothing behind. A custom
//! `POLYFORGE_COPY_FILE` must resolve inside the `$XDG_DATA_HOME`
//! `polyforge/` subtree (relative paths are remapped there); anything
//! else is rejected. Symlinks at the subtree root (`polyforge`) or any
//! component below it are refused rather than followed.

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
    pub no_backup: bool,
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
            no_backup: std::env::var_os("POLYFORGE_CLIPBOARD_NO_BACKUP").is_some(),
        }
    }
}

/// Raw-byte cap for the OSC 52 leg: selections at or under this size emit
/// normally; anything larger skips the OSC 52 leg (native/tmux legs and the
/// backup file are unaffected, so nothing is lost — it just never becomes
/// a giant escape blob on stderr).
pub const MAX_OSC52_RAW_BYTES: usize = 102_400;

/// Pure gate for the OSC 52 leg (unit-testable without touching stderr).
pub fn osc52_allowed(text: &str) -> bool {
    text.len() <= MAX_OSC52_RAW_BYTES
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

/// Copy entry point: every active leg fires, the backup file is
/// attempted unless `POLYFORGE_CLIPBOARD_NO_BACKUP` is set, and the
/// returned delivery says where the text landed. Oversize selections skip
/// the OSC 52 leg (see [`MAX_OSC52_RAW_BYTES`]) but still reach the other
/// legs and the backup file.
pub fn copy_text_or_file(text: &str) -> CopyDelivery {
    let env = Env::ambient();
    let route = resolve_route(&env);
    let mut legs = Vec::new();
    if route.native && native_copy(text) {
        legs.push("native");
    }
    if route.tmux_buffer && tmux_copy(text) {
        legs.push("tmux");
    }
    if route.osc52 && osc52_allowed(text) {
        emit_osc52(text, route.osc52_tmux_passthrough);
        legs.push("osc52");
    }
    let file = if env.no_backup {
        None
    } else {
        write_copy_fallback(text).ok()
    };
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
    let path =
        std::env::temp_dir().join(format!("pf-clipboard-{}-{nanos}-{seq}", std::process::id()));
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

/// Data dir for the backup file: `XDG_DATA_HOME`, else
/// `~/.local/share`. `None` when no home resolves.
fn data_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
}

/// Lexically normalize an absolute path (collapse `.`, duplicate
/// separators, and `..` without touching the filesystem), so the
/// subtree check below cannot be dodged with `..` segments.
fn lexical_normalize_absolute(path: &std::path::Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// Resolve the backup-file path (pure over its inputs: unit-testable
/// without mutating the process env).
///
/// - `copy_file`: value of `POLYFORGE_COPY_FILE` (`None`/empty = default).
/// - `base`: the data dir (`None` = unresolvable home).
///
/// Rules (Stage 5): the default is `<base>/polyforge/last-copy.txt`. A
/// custom path must stay inside the `<base>/polyforge/` subtree —
/// relative paths are remapped there, `~` expands against `home`, and
/// anything escaping the subtree (absolute `/tmp/...`, `..` traversal,
/// `~` outside the data dir) is rejected.
fn resolve_copy_path(
    copy_file: Option<&str>,
    base: Option<PathBuf>,
    home: Option<&str>,
) -> std::io::Result<PathBuf> {
    let base = base.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no home directory resolves; set POLYFORGE_COPY_FILE to enable the copy backup file",
        )
    })?;
    let subtree = base.join("polyforge");
    let Some(raw) = copy_file.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(subtree.join("last-copy.txt"));
    };
    let expanded = match raw.strip_prefix('~') {
        Some(rest) => match home {
            Some(h) => format!("{h}{rest}"),
            None => raw.to_string(),
        },
        None => raw.to_string(),
    };
    let candidate = PathBuf::from(&expanded);
    // Relative paths remap under the subtree; absolute paths must already
    // be inside it. Never a world-visible temp fallback.
    let joined = if candidate.is_absolute() {
        candidate
    } else {
        subtree.join(candidate)
    };
    // Anchor relative remainders (e.g. `..` leading above CWD) before the
    // lexical check so the subtree comparison is meaningful.
    let absolute = if joined.is_absolute() {
        joined
    } else {
        std::env::current_dir()
            .unwrap_or(PathBuf::from("/"))
            .join(joined)
    };
    let normalized = lexical_normalize_absolute(&absolute);
    if normalized.starts_with(&subtree) {
        Ok(normalized)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "POLYFORGE_COPY_FILE must stay under {}",
                subtree.to_string_lossy()
            ),
        ))
    }
}

/// Backup-file path: `POLYFORGE_COPY_FILE` (restricted to the data-dir
/// `polyforge/` subtree), else the default. Never a world-visible temp
/// path: no home resolvable means no file (NotFound).
fn fallback_path() -> std::io::Result<PathBuf> {
    let copy_file = std::env::var("POLYFORGE_COPY_FILE").ok();
    let home = std::env::var_os("HOME")
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_default();
    let home_opt = if home.is_empty() { None } else { Some(home) };
    resolve_copy_path(copy_file.as_deref(), data_dir(), home_opt.as_deref())
}

/// Backup write inside `subtree` (`<anchor>/polyforge`). Single-file
/// overwrite, mode 0600, created directories 0700. Ancestors above the
/// subtree may be followed. From `polyforge` downward, symlinks are not.
fn write_copy_backup_to(
    subtree: &std::path::Path,
    path: &std::path::Path,
    text: &str,
) -> std::io::Result<()> {
    let anchor = backup_anchor(subtree, path)?;
    create_private_dir(anchor)?;
    let mut file = open_backup_file(anchor, path)?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

fn backup_anchor<'a>(
    subtree: &'a std::path::Path,
    path: &'a std::path::Path,
) -> std::io::Result<&'a std::path::Path> {
    let inside = path.strip_prefix(subtree).ok();
    if inside.map(|rel| rel.as_os_str().is_empty()).unwrap_or(true) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "copy backup path is outside the polyforge subtree",
        ));
    }
    subtree
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "copy backup subtree has no anchor",
            )
        })
}

/// Attempted backup write (unless `POLYFORGE_CLIPBOARD_NO_BACKUP` is set
/// by the caller), returning the path written.
fn write_copy_fallback(text: &str) -> std::io::Result<PathBuf> {
    let path = fallback_path()?;
    let subtree = data_dir()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no home directory resolves; set POLYFORGE_COPY_FILE to enable the copy backup file",
            )
        })?
        .join("polyforge");
    write_copy_backup_to(&subtree, &path, text)?;
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

fn open_backup_file(
    anchor: &std::path::Path,
    path: &std::path::Path,
) -> std::io::Result<std::fs::File> {
    #[cfg(any(
        target_os = "macos",
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64")
    ))]
    {
        backup_fd::open(anchor, path)
    }
    #[cfg(not(any(
        target_os = "macos",
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64")
    )))]
    {
        // OpenOptions only. This target has no verified O_NOFOLLOW flags.
        let _ = anchor;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                create_private_dir(parent)?;
            }
        }
        open_backup_fallback(path)
    }
}

#[cfg(not(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64")
)))]
fn open_backup_fallback(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

#[cfg(any(
    target_os = "macos",
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64")
))]
mod backup_fd {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::{AsRawFd, FromRawFd};
    use std::path::{Component, Path};

    #[cfg(target_os = "macos")]
    mod flags {
        // MacOSX.sdk usr/include/sys/fcntl.h and errno.h (this host).
        pub const O_WRONLY: i32 = 0x0001;
        pub const O_CREAT: i32 = 0x0000_0200;
        pub const O_TRUNC: i32 = 0x0000_0400;
        pub const O_NOFOLLOW: i32 = 0x0000_0100;
        pub const O_DIRECTORY: i32 = 0x0010_0000;
        pub const O_CLOEXEC: i32 = 0x0100_0000;
        pub const ELOOP: i32 = 62;
        pub const ENOENT: i32 = 2;
        pub const EEXIST: i32 = 17;
        pub const ENOTDIR: i32 = 20;
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    mod flags {
        // glibc 2.40 bits/fcntl-linux.h (x86_64 does not override these)
        // and Linux v6.12 include/uapi/asm-generic/fcntl.h. Values are octal.
        // ELOOP is include/uapi/asm-generic/errno.h.
        pub const O_WRONLY: i32 = 0o1;
        pub const O_CREAT: i32 = 0o100;
        pub const O_TRUNC: i32 = 0o1000;
        pub const O_DIRECTORY: i32 = 0o200000;
        pub const O_NOFOLLOW: i32 = 0o400000;
        pub const O_CLOEXEC: i32 = 0o2000000;
        pub const ELOOP: i32 = 40;
        pub const ENOENT: i32 = 2;
        pub const EEXIST: i32 = 17;
        pub const ENOTDIR: i32 = 20;
    }

    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    mod flags {
        // glibc 2.40 sysdeps/unix/sysv/linux/aarch64/bits/fcntl.h overrides
        // O_DIRECTORY and O_NOFOLLOW (same values as arch/arm64 uapi fcntl.h,
        // which defines them before including asm-generic). The other O_*
        // values stay on the fcntl-linux.h defaults. ELOOP is asm-generic.
        pub const O_WRONLY: i32 = 0o1;
        pub const O_CREAT: i32 = 0o100;
        pub const O_TRUNC: i32 = 0o1000;
        pub const O_DIRECTORY: i32 = 0o40000;
        pub const O_NOFOLLOW: i32 = 0o100000;
        pub const O_CLOEXEC: i32 = 0o2000000;
        pub const ELOOP: i32 = 40;
        pub const ENOENT: i32 = 2;
        pub const EEXIST: i32 = 17;
        pub const ENOTDIR: i32 = 20;
    }

    const DIR_FLAGS: i32 = flags::O_NOFOLLOW | flags::O_DIRECTORY | flags::O_CLOEXEC;
    const FILE_FLAGS: i32 =
        flags::O_NOFOLLOW | flags::O_CREAT | flags::O_TRUNC | flags::O_WRONLY | flags::O_CLOEXEC;
    const PROBE_FLAGS: i32 = flags::O_NOFOLLOW | flags::O_CLOEXEC;

    unsafe extern "C" {
        fn openat(dirfd: i32, pathname: *const std::ffi::c_char, flags: i32, mode: u32) -> i32;
        fn mkdirat(dirfd: i32, pathname: *const std::ffi::c_char, mode: u32) -> i32;
        fn fchmod(fd: i32, mode: u32) -> i32;
        fn close(fd: i32) -> i32;
    }

    struct Opened(i32);

    impl Drop for Opened {
        fn drop(&mut self) {
            if self.0 >= 0 {
                unsafe { close(self.0) };
                self.0 = -1;
            }
        }
    }

    impl Opened {
        fn fd(&self) -> i32 {
            self.0
        }
    }

    pub fn open(anchor: &Path, path: &Path) -> std::io::Result<std::fs::File> {
        let anchor_dir = std::fs::File::open(anchor)?;
        let rel = path
            .strip_prefix(anchor)
            .map_err(|_| invalid("copy backup path is outside the anchor"))?;
        validate_relative(rel)?;
        let mut current: Option<Opened> = None;
        let mut components = rel.components().peekable();
        while let Some(component) = components.next() {
            let name = component_cstr(component)?;
            let parent = if let Some(dir) = &current {
                dir.fd()
            } else {
                anchor_dir.as_raw_fd()
            };
            if components.peek().is_some() {
                current = Some(open_dir_component(parent, &name)?);
            } else {
                return open_file_component(parent, &name);
            }
        }
        Err(invalid("copy backup path has no file component"))
    }

    fn validate_relative(rel: &Path) -> std::io::Result<()> {
        let mut saw = false;
        for component in rel.components() {
            let Component::Normal(name) = component else {
                return Err(invalid("refusing non-normal component in copy backup path"));
            };
            if name.as_bytes().contains(&0) {
                return Err(invalid("refusing nul in copy backup path"));
            }
            saw = true;
        }
        if saw {
            Ok(())
        } else {
            Err(invalid("copy backup path has no file component"))
        }
    }

    fn component_cstr(component: Component<'_>) -> std::io::Result<CString> {
        let Component::Normal(name) = component else {
            return Err(invalid("refusing non-normal component in copy backup path"));
        };
        CString::new(name.as_bytes()).map_err(|_| invalid("refusing nul in copy backup path"))
    }

    fn open_dir_component(parent: i32, name: &CStr) -> std::io::Result<Opened> {
        match open_dir_nofollow(parent, name) {
            Ok(fd) => Ok(Opened(fd)),
            Err(err) if err.raw_os_error() == Some(flags::ENOENT) => {
                mkdir_component(parent, name)?;
                open_dir_nofollow(parent, name).map(Opened)
            }
            Err(err) => Err(err),
        }
    }

    fn open_dir_nofollow(parent: i32, name: &CStr) -> std::io::Result<i32> {
        match open_raw(parent, name, DIR_FLAGS, 0) {
            Ok(fd) => Ok(fd),
            Err(err) if err.raw_os_error() == Some(flags::ENOTDIR) => {
                probe_dir_symlink(parent, name, err)
            }
            Err(err) => Err(map_eloop(err)),
        }
    }

    /// macOS returns ENOTDIR, not ELOOP, for O_NOFOLLOW|O_DIRECTORY on a
    /// symlink. Probe with O_NOFOLLOW and no O_DIRECTORY: ELOOP means the
    /// component is a symlink and was not followed. A real non-directory
    /// stays ENOTDIR.
    fn probe_dir_symlink(
        parent: i32,
        name: &CStr,
        original: std::io::Error,
    ) -> std::io::Result<i32> {
        match open_raw(parent, name, PROBE_FLAGS, 0) {
            Err(err) if err.raw_os_error() == Some(flags::ELOOP) => Err(map_eloop(err)),
            Ok(fd) => {
                unsafe { close(fd) };
                Err(original)
            }
            Err(_) => Err(original),
        }
    }

    fn mkdir_component(parent: i32, name: &CStr) -> std::io::Result<()> {
        let rc = unsafe { mkdirat(parent, name.as_ptr(), 0o700) };
        if rc == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(flags::EEXIST) {
            Ok(())
        } else {
            Err(map_eloop(err))
        }
    }

    fn open_file_component(parent: i32, name: &CStr) -> std::io::Result<std::fs::File> {
        let fd = match open_raw(parent, name, FILE_FLAGS, 0o600) {
            Ok(fd) => fd,
            Err(err) => return Err(map_eloop(err)),
        };
        // fchmod the new fd. openat's mode argument is variadic, so umask
        // (and the Darwin arm64 variadic ABI) must not leave it world-readable.
        // Never chmod by path: that would follow a symlink.
        if unsafe { fchmod(fd, 0o600) } < 0 {
            let err = std::io::Error::last_os_error();
            unsafe { close(fd) };
            return Err(err);
        }
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }

    fn open_raw(dirfd: i32, name: &CStr, flags: i32, mode: u32) -> std::io::Result<i32> {
        let fd = unsafe { openat(dirfd, name.as_ptr(), flags, mode) };
        if fd < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(fd)
        }
    }

    fn map_eloop(err: std::io::Error) -> std::io::Error {
        if err.raw_os_error() == Some(flags::ELOOP) {
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, err)
        } else {
            err
        }
    }

    fn invalid(message: &str) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
    }
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
            no_backup: false,
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
        assert!(
            clip("osc52")
                .toast_message()
                .starts_with("Copied via OSC 52, saved to ")
        );
        assert_eq!(
            CopyDelivery::Clipboard {
                legs: "osc52".to_string(),
                file: None,
            }
            .toast_message(),
            "Copied via OSC 52."
        );
        assert!(
            CopyDelivery::File {
                path: PathBuf::from("/tmp/x.txt"),
            }
            .toast_message()
            .starts_with("Clipboard unreachable: wrote ")
        );
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

    #[test]
    fn osc52_large_copy_is_capped() {
        // S5-F1: small selections emit; anything over the cap skips the
        // OSC 52 leg (native/tmux legs and the backup file still fire).
        assert!(osc52_allowed("hi"));
        assert!(osc52_allowed(&"x".repeat(MAX_OSC52_RAW_BYTES)));
        assert!(!osc52_allowed(&"x".repeat(MAX_OSC52_RAW_BYTES + 1)));
        assert!(!osc52_allowed(&"x".repeat(MAX_OSC52_RAW_BYTES * 4)));
    }

    #[test]
    fn resolve_copy_path_defaults_and_restricts() {
        // S5-F2: default, in-tree custom, relative remap, and rejections.
        let base = PathBuf::from("/data");
        let home = Some("/home/u");
        assert_eq!(
            resolve_copy_path(None, Some(base.clone()), home).unwrap(),
            PathBuf::from("/data/polyforge/last-copy.txt")
        );
        assert_eq!(
            resolve_copy_path(Some("  "), Some(base.clone()), home).unwrap(),
            PathBuf::from("/data/polyforge/last-copy.txt")
        );
        // Absolute in-tree custom path is honored.
        assert_eq!(
            resolve_copy_path(Some("/data/polyforge/custom.txt"), Some(base.clone()), home)
                .unwrap(),
            PathBuf::from("/data/polyforge/custom.txt")
        );
        // Relative paths remap under the subtree.
        assert_eq!(
            resolve_copy_path(Some("notes/copy.txt"), Some(base.clone()), home).unwrap(),
            PathBuf::from("/data/polyforge/notes/copy.txt")
        );
        // Out-of-tree absolute paths are rejected.
        assert!(resolve_copy_path(Some("/tmp/evil.txt"), Some(base.clone()), home).is_err());
        assert!(resolve_copy_path(Some("/data/other.txt"), Some(base.clone()), home).is_err());
        // `..` traversal escaping the subtree is rejected.
        assert!(
            resolve_copy_path(
                Some("/data/polyforge/../../etc/x"),
                Some(base.clone()),
                home
            )
            .is_err()
        );
        assert!(resolve_copy_path(Some("../evil.txt"), Some(base.clone()), home).is_err());
        // `~` expanding outside the subtree is rejected.
        assert!(resolve_copy_path(Some("~/copy.txt"), Some(base.clone()), home).is_err());
        // No resolvable home means no file.
        assert!(resolve_copy_path(None, None, home).is_err());
    }

    /// Unique scratch dir per test (parallel-safe: pid + nanos + counter).
    fn scratch_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "pf-clipboard-test-{}-{}-{nanos}-{seq}",
            std::process::id(),
            tag
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    #[cfg(unix)]
    fn backup_refuses_symlink() {
        // S5-F2: a symlinked backup path is refused, and the link target
        // is never written through.
        let dir = scratch_dir("symlink");
        let target = dir.join("target.txt");
        let link = dir.join("polyforge").join("last-copy.txt");
        std::fs::create_dir_all(link.parent().unwrap()).expect("parents");
        std::fs::write(&target, "original").expect("target");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let subtree = dir.join("polyforge");
        let err = write_copy_backup_to(&subtree, &link, "sensitive copy").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            std::fs::read_to_string(&target).expect("target intact"),
            "original"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn backup_refuses_polyforge_dir_symlink() {
        // S5-GAP-1: a symlink at the subtree root is not followed.
        let dir = scratch_dir("pf-symlink");
        let outside = dir.join("outside");
        std::fs::create_dir(&outside).expect("outside");
        std::fs::write(outside.join("marker"), "keep").expect("marker");
        let subtree = dir.join("polyforge");
        std::os::unix::fs::symlink(&outside, &subtree).expect("symlink");
        let path = subtree.join("last-copy.txt");
        let err = write_copy_backup_to(&subtree, &path, "sensitive copy").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            std::fs::read_to_string(outside.join("marker")).expect("marker intact"),
            "keep"
        );
        assert!(!outside.join("last-copy.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn backup_refuses_subdir_symlink() {
        // S5-GAP-1: a symlink under polyforge is not followed.
        let dir = scratch_dir("sub-symlink");
        let outside = dir.join("outside");
        std::fs::create_dir(&outside).expect("outside");
        std::fs::write(outside.join("marker"), "keep").expect("marker");
        let subtree = dir.join("polyforge");
        std::fs::create_dir(&subtree).expect("polyforge");
        let notes = subtree.join("notes");
        std::os::unix::fs::symlink(&outside, &notes).expect("symlink");
        let path = notes.join("copy.txt");
        let err = write_copy_backup_to(&subtree, &path, "sensitive copy").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            std::fs::read_to_string(outside.join("marker")).expect("marker intact"),
            "keep"
        );
        assert!(!outside.join("copy.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_overwrites_without_rotation() {
        // S5-F1 rotation policy: one path, truncated each copy — no
        // numbered history, no siblings left behind.
        let dir = scratch_dir("rotate");
        let subtree = dir.join("polyforge");
        let path = subtree.join("last-copy.txt");
        write_copy_backup_to(&subtree, &path, "first copy").expect("first write");
        write_copy_backup_to(&subtree, &path, "second copy").expect("second write");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            "second copy"
        );
        let siblings: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .expect("list dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(siblings, vec![std::ffi::OsString::from("last-copy.txt")]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
