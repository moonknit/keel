use assert_cmd::cargo::cargo_bin;
use std::path::Path;
use std::process::Command;

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

/// REQ-3.23 print mode exit 0 with scripted model.
#[test]
fn req_3_23_print_mode_scripted() {
    let dir = tempfile::tempdir().unwrap();
    let out = scripted(dir.path())
        .args(["-p", "hello"])
        .output()
        .expect("run");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("ok"), "{stdout}");
}

/// REQ-3.21 session list / show / rm after a print run.
#[test]
fn req_3_21_session_list_show_rm() {
    let dir = tempfile::tempdir().unwrap();
    let printed = scripted(dir.path())
        .args(["-p", "hello"])
        .output()
        .expect("print");
    assert!(printed.status.success());

    let list = scripted(dir.path())
        .args(["session", "list"])
        .output()
        .expect("list");
    assert!(list.status.success());
    let listed = String::from_utf8_lossy(&list.stdout);
    let id = listed
        .split_whitespace()
        .next()
        .expect("session id")
        .to_string();
    assert!(!id.is_empty(), "{listed}");

    let show = scripted(dir.path())
        .args(["session", "show", &id])
        .output()
        .expect("show");
    assert!(show.status.success());
    let shown = String::from_utf8_lossy(&show.stdout);
    assert!(shown.contains("chat"), "{shown}");

    let rm = scripted(dir.path())
        .args(["session", "rm", &id])
        .output()
        .expect("rm");
    assert!(rm.status.success());
}

/// REQ-3.23 live print mode. Skips unless KEEL_LIVE=1 and .env is present.
#[test]
fn req_3_23_print_mode_live() {
    let repo = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let _ = dotenvy::from_path(repo.join(".env"));
    if std::env::var("KEEL_LIVE").ok().as_deref() != Some("1") {
        eprintln!("skip live print; set KEEL_LIVE=1");
        return;
    }
    let key = match std::env::var("OPENAI_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!("skip live print; OPENAI_API_KEY missing");
            return;
        }
    };
    let model = std::env::var("LLM_MODEL").unwrap_or_else(|_| "gpt-6-luna".into());
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(cargo_bin("keel"))
        .current_dir(dir.path())
        .env("KEEL_HOME", dir.path())
        .env_remove("KEEL_SCRIPTED")
        .env("OPENAI_API_KEY", key)
        .env("LLM_MODEL", model)
        .env(
            "LLM_BASE_URL",
            std::env::var("LLM_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into()),
        )
        .args([
            "-p",
            "Reply with exactly the word pong and nothing else. Do not call tools.",
        ])
        .output()
        .expect("live print");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).to_lowercase();
    assert!(stdout.contains("pong"), "{stdout}");
}
