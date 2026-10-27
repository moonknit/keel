use assert_cmd::cargo::cargo_bin;
use std::fs;
use std::process::Command;

fn session_list(home: &std::path::Path, keel_log: Option<&str>) -> std::process::Output {
    let mut cmd = Command::new(cargo_bin("keel"));
    cmd.current_dir(home)
        .env("KEEL_HOME", home)
        .env("OPENAI_API_KEY", "sk-fake-logmask-TESTONLY")
        .env_remove("KEEL_LIVE")
        .args(["session", "list"]);
    match keel_log {
        Some(v) => {
            cmd.env("KEEL_LOG", v);
        }
        None => {
            cmd.env_remove("KEEL_LOG");
        }
    }
    cmd.output().expect("keel session list")
}

fn dated_log(home: &std::path::Path) -> (std::path::PathBuf, String) {
    let logs = home.join("logs");
    let entries: Vec<_> = fs::read_dir(&logs)
        .unwrap_or_else(|e| panic!("logs dir {}: {e}", logs.display()))
        .map(|e| e.unwrap().path())
        .collect();
    let path = entries
        .iter()
        .find(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.len() == "keel-YYYYMMDD.log".len()
                    && n.starts_with("keel-")
                    && n.ends_with(".log")
                    && n[5..13].bytes().all(|b| b.is_ascii_digit())
            })
        })
        .cloned()
        .unwrap_or_else(|| panic!("missing keel-YYYYMMDD.log in {entries:?}"));
    let body = fs::read_to_string(&path).unwrap();
    (path, body)
}

/// REQ-4.2 INV-8 tracing goes to keel-YYYYMMDD.log; debug stays off captured stderr.
#[test]
fn req_4_2_inv_8_file_log_debug_not_on_stderr() {
    let home = tempfile::tempdir().unwrap();
    let out = session_list(home.path(), None);
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("subscriber ready"), "{stderr}");
    assert!(!stderr.contains("file log ready"), "{stderr}");
    assert!(!stderr.contains("DEBUG"), "{stderr}");
    assert!(!stderr.contains("sk-fake-logmask-TESTONLY"), "{stderr}");

    let (path, file) = dated_log(home.path());
    assert!(
        path.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("keel-") && n.ends_with(".log")),
        "{}",
        path.display()
    );
    assert!(file.contains("file log ready"), "{file}");
    assert!(!file.contains("subscriber ready"), "{file}");
    assert!(!file.contains("sk-fake-logmask-TESTONLY"), "{file}");
}

/// REQ-4.2 KEEL_LOG=debug raises file level and still leaves stderr quiet.
#[test]
fn req_4_2_keel_log_debug_raises_level_off_stderr() {
    let home = tempfile::tempdir().unwrap();
    let out = session_list(home.path(), Some("debug"));
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("subscriber ready"), "{stderr}");
    assert!(!stderr.contains("DEBUG"), "{stderr}");
    let (_path, file) = dated_log(home.path());
    assert!(file.contains("subscriber ready"), "{file}");
    assert!(file.contains("file log ready"), "{file}");
}
