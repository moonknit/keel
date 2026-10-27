//! File-only tracing for `$KEEL_HOME/logs/keel-YYYYMMDD.log` (REQ-4.2, INV-8).

use anyhow::{Context, Result};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::level_filters::LevelFilter;
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::Layer;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

static EXTRA_SECRETS: Mutex<Vec<String>> = Mutex::new(Vec::new());

const MIN_SECRET_LEN: usize = 8;

pub struct LogGuard {
    path: PathBuf,
    writer: FileWriter,
    _default: Option<DefaultGuard>,
}

impl LogGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn flush(&self) {
        if let Ok(mut file) = self.writer.file.lock() {
            let _ = file.flush();
        }
    }
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        self._default.take();
        self.flush();
    }
}

#[derive(Clone)]
struct FileWriter {
    file: Arc<Mutex<fs::File>>,
    secrets: Arc<Vec<String>>,
}

impl Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let redacted = redact(&String::from_utf8_lossy(buf), &self.secrets);
        self.file.lock().unwrap().write_all(redacted.as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.lock().unwrap().flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for FileWriter {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

pub fn mask_secret(value: &str) -> String {
    keel_openai::mask_secret(value)
}

pub fn register_secret(value: &str) {
    if value.len() < MIN_SECRET_LEN {
        return;
    }
    EXTRA_SECRETS.lock().unwrap().push(value.to_string());
}

pub fn log_file_path(home: &Path) -> PathBuf {
    home.join("logs")
        .join(format!("keel-{}.log", yyyymmdd_utc()))
}

pub fn init(home: &Path) -> Result<LogGuard> {
    let guard = install(home, level_from_env(), env_secrets(), true)?;
    tracing::debug!("subscriber ready");
    tracing::info!(file = %guard.path().display(), "file log ready");
    Ok(guard)
}

fn level_from_env() -> LevelFilter {
    match std::env::var("KEEL_LOG") {
        Ok(v) if v.eq_ignore_ascii_case("debug") => LevelFilter::DEBUG,
        _ => LevelFilter::INFO,
    }
}

fn env_secrets() -> Vec<String> {
    std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|k| k.len() >= MIN_SECRET_LEN)
        .into_iter()
        .collect()
}

fn yyyymmdd_utc() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day()
    )
}

fn install(
    home: &Path,
    level: LevelFilter,
    secrets: Vec<String>,
    global: bool,
) -> Result<LogGuard> {
    let path = log_file_path(home);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("logging: create {}", dir.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("logging: open {}", path.display()))?;
    let writer = FileWriter {
        file: Arc::new(Mutex::new(file)),
        secrets: Arc::new(secrets),
    };
    let layer = fmt::layer()
        .with_ansi(false)
        .with_target(true)
        .with_writer(writer.clone())
        .with_filter(level);
    let subscriber = tracing_subscriber::registry().with(layer);
    let default = if global {
        subscriber
            .try_init()
            .map_err(|e| anyhow::anyhow!("logging: {e}"))?;
        None
    } else {
        Some(tracing::subscriber::set_default(subscriber))
    };
    Ok(LogGuard {
        path,
        writer,
        _default: default,
    })
}

fn redact(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for secret in secrets.iter().chain(extra_secrets().iter()) {
        if secret.len() >= MIN_SECRET_LEN && out.contains(secret) {
            out = out.replace(secret, &mask_secret(secret));
        }
    }
    out
}

fn extra_secrets() -> Vec<String> {
    EXTRA_SECRETS.lock().map(|g| g.clone()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn init_local(home: &Path, level: LevelFilter, secrets: Vec<String>) -> Result<LogGuard> {
        install(home, level, secrets, false)
    }

    fn read_log(guard: &LogGuard) -> String {
        guard.flush();
        fs::read_to_string(guard.path()).unwrap()
    }

    /// REQ-4.2 INV-8 dated file name, fake key and MCP subst result are masked.
    #[test]
    fn req_4_2_inv_8_file_log_masks_key() {
        let home = tempfile::tempdir().unwrap();
        let key = "sk-fake-logmask-TESTONLY";
        let mcp = "mcp-subst-result-TESTVAL";
        EXTRA_SECRETS.lock().unwrap().clear();
        let guard = init_local(home.path(), LevelFilter::INFO, vec![key.into()]).unwrap();
        register_secret(mcp);
        tracing::info!("auth {key} header {mcp}");
        let name = guard.path().file_name().and_then(|n| n.to_str()).unwrap();
        assert_eq!(name, format!("keel-{}.log", yyyymmdd_utc()));
        assert_eq!(guard.path(), log_file_path(home.path()));
        let text = read_log(&guard);
        assert!(!text.contains(key), "raw key leaked");
        assert!(!text.contains(mcp), "mcp subst leaked");
        assert!(text.contains(&mask_secret(key)), "{text}");
        assert!(text.contains(&mask_secret(mcp)), "{text}");
    }

    /// REQ-4.2 default level keeps debug out of the file (terminal is file-only).
    #[test]
    fn req_4_2_file_log_debug_not_in_file_by_default() {
        let home = tempfile::tempdir().unwrap();
        let guard = init_local(home.path(), LevelFilter::INFO, Vec::new()).unwrap();
        tracing::debug!("debug-marker-stderr");
        tracing::info!("info-marker-file");
        let file = read_log(&guard);
        assert!(file.contains("info-marker-file"), "{file}");
        assert!(!file.contains("debug-marker-stderr"), "{file}");
    }

    /// REQ-4.2 KEEL_LOG=debug raises the file level.
    #[test]
    fn req_4_2_keel_log_debug_raises_level() {
        let home = tempfile::tempdir().unwrap();
        let guard = init_local(home.path(), LevelFilter::DEBUG, Vec::new()).unwrap();
        tracing::debug!("debug-marker-file");
        let file = read_log(&guard);
        assert!(file.contains("debug-marker-file"), "{file}");
    }
}
