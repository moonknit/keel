//! OS jail for bash. Same primitives Claude Code uses, not their Node wrapper.
//! macOS: Seatbelt (`sandbox-exec`). Linux: bubblewrap (`bwrap`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

#[cfg(test)]
use std::cell::Cell;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JailKind {
    Seatbelt,
    Bwrap,
}

#[derive(Debug, Clone)]
pub struct OsJail {
    pub kind: JailKind,
}

static DETECTED: OnceLock<Option<JailKind>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static TEST_WRAP: Cell<Option<(&'static str, JailKind)>> = const { Cell::new(None) };
}

/// Test-only: force a jail and a wrap program that cannot start.
/// Thread-local so parallel tests do not share the override.
#[cfg(test)]
pub struct JailTestOverride;

#[cfg(test)]
impl JailTestOverride {
    pub fn missing_bin() -> Self {
        TEST_WRAP.with(|c| c.set(Some(("/no/such/keel-jail", JailKind::Seatbelt))));
        Self
    }
}

#[cfg(test)]
impl Drop for JailTestOverride {
    fn drop(&mut self) {
        TEST_WRAP.with(|c| c.set(None));
    }
}

impl OsJail {
    pub fn detect() -> Option<Self> {
        #[cfg(test)]
        if let Some((_, kind)) = TEST_WRAP.with(|c| c.get()) {
            return Some(Self { kind });
        }
        DETECTED
            .get_or_init(probe)
            .map(|kind| Self { kind })
    }

    pub fn available() -> bool {
        Self::detect().is_some()
    }

    pub fn name(&self) -> &'static str {
        match self.kind {
            JailKind::Seatbelt => "seatbelt",
            JailKind::Bwrap => "bwrap",
        }
    }

    pub fn wrap(&self, cwd: &Path, command: &str) -> (PathBuf, Vec<String>) {
        let (prog, args) = match self.kind {
            JailKind::Seatbelt => wrap_seatbelt(cwd, command),
            JailKind::Bwrap => wrap_bwrap(cwd, command),
        };
        #[cfg(test)]
        if let Some((over, _)) = TEST_WRAP.with(|c| c.get()) {
            return (PathBuf::from(over), args);
        }
        (prog, args)
    }
}

fn probe() -> Option<JailKind> {
    if cfg!(target_os = "macos") {
        let ok = Command::new("sandbox-exec")
            .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        return ok.then_some(JailKind::Seatbelt);
    }
    if cfg!(target_os = "linux") {
        let ok = Command::new("bwrap")
            .args([
                "--ro-bind",
                "/usr",
                "/usr",
                "--ro-bind-try",
                "/bin",
                "/bin",
                "--dev",
                "/dev",
                "--unshare-net",
                "--die-with-parent",
                "/bin/true",
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        return ok.then_some(JailKind::Bwrap);
    }
    None
}

fn sbpl_path(p: &Path) -> String {
    p.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

fn wrap_seatbelt(cwd: &Path, command: &str) -> (PathBuf, Vec<String>) {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let cwd_s = sbpl_path(&cwd);
    let profile = format!(
        r#"(version 1)
(allow default)
(deny network*)
(deny file-write*)
(allow file-write*
  (subpath "{cwd_s}")
  (subpath "/private/tmp")
  (subpath "/tmp")
  (subpath "/var/folders")
)"#
    );
    (
        PathBuf::from("sandbox-exec"),
        vec![
            "-p".into(),
            profile,
            "/bin/sh".into(),
            "-c".into(),
            command.into(),
        ],
    )
}

fn wrap_bwrap(cwd: &Path, command: &str) -> (PathBuf, Vec<String>) {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let cwd_s = cwd.to_string_lossy().into_owned();
    let home = std::env::var("HOME").unwrap_or_default();
    let mut args = vec![
        "--die-with-parent".into(),
        "--unshare-net".into(),
        "--unshare-pid".into(),
        "--dev".into(),
        "/dev".into(),
        "--proc".into(),
        "/proc".into(),
        "--tmpfs".into(),
        "/tmp".into(),
        "--ro-bind".into(),
        "/usr".into(),
        "/usr".into(),
    ];
    for p in ["/bin", "/sbin", "/lib", "/lib64", "/etc"] {
        args.extend(["--ro-bind-try".into(), p.into(), p.into()]);
    }
    if !home.is_empty() {
        let rustup = format!("{home}/.rustup");
        let cargo = format!("{home}/.cargo");
        args.extend(["--ro-bind-try".into(), rustup.clone(), rustup]);
        args.extend(["--ro-bind-try".into(), cargo.clone(), cargo]);
    }
    args.extend([
        "--bind".into(),
        cwd_s.clone(),
        cwd_s.clone(),
        "--chdir".into(),
        cwd_s,
        "/bin/sh".into(),
        "-c".into(),
        command.into(),
    ]);
    (PathBuf::from("bwrap"), args)
}
