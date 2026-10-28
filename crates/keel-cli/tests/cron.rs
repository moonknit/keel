use assert_cmd::cargo::cargo_bin;
use keel_store::SqliteStore;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

fn scripted(dir: &Path) -> Command {
    let mut cmd = Command::new(cargo_bin("keel"));
    cmd.current_dir(dir)
        .env("KEEL_HOME", dir)
        .env("KEEL_SCRIPTED", "1")
        .env("LLM_MODEL", "test")
        .env("OPENAI_API_KEY", "test")
        .env_remove("KEEL_LIVE");
    cmd
}

/// REQ-3.21 REQ-3.32 cron list/add/rm persist job policy on the row.
#[test]
fn req_3_32_cron_cli_crud() {
    let dir = tempfile::tempdir().unwrap();
    let add = scripted(dir.path())
        .args([
            "cron",
            "add",
            "--schedule",
            "0 0 * * *",
            "--prompt",
            "nightly",
            "--allow-edits",
        ])
        .output()
        .expect("add");
    assert!(
        add.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&add.stderr)
    );
    let id = String::from_utf8_lossy(&add.stdout).trim().to_string();
    assert!(!id.is_empty());

    let list = scripted(dir.path())
        .args(["cron", "list"])
        .output()
        .expect("list");
    assert!(list.status.success());
    let listed = String::from_utf8_lossy(&list.stdout);
    assert!(listed.contains(&id), "{listed}");
    assert!(listed.contains("nightly"), "{listed}");

    let rm = scripted(dir.path())
        .args(["cron", "rm", &id])
        .output()
        .expect("rm");
    assert!(rm.status.success());
    let empty = scripted(dir.path())
        .args(["cron", "list"])
        .output()
        .expect("list2");
    assert!(String::from_utf8_lossy(&empty.stdout).is_empty());
}

/// REQ-3.32 INV-20 `keel serve` holds the scheduler lock so another holder fails.
#[test]
fn req_3_32_inv20_serve_holds_scheduler_lock() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = scripted(dir.path())
        .arg("serve")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("serve");
    let db = dir.path().join("keel.db");
    let mut held = false;
    for _ in 0..40 {
        if let Ok(store) = SqliteStore::open(&db) {
            if store.scheduler_pid().ok().flatten().is_some() {
                let me = i64::from(std::process::id());
                assert!(
                    !store.acquire_scheduler(me).unwrap(),
                    "second holder must fail"
                );
                held = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(held, "serve never acquired scheduler lock");
}
