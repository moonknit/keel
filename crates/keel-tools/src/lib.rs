mod bash;
mod edit;
mod glob;
mod grep;
mod jail;
mod pathx;
mod policy;
mod read;
mod write;

pub use bash::Bash;
pub use edit::Edit;
pub use glob::Glob;
pub use grep::Grep;
pub use jail::{JailKind, OsJail};
pub use policy::{dangerous_bash, Policy, PolicyApprover};
pub use read::ReadFile;
pub use write::WriteFile;

use keel_core::traits::Tool;
use std::sync::Arc;

pub fn coding_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadFile),
        Arc::new(Grep),
        Arc::new(Glob),
        Arc::new(WriteFile),
        Arc::new(Edit),
        Arc::new(Bash::default()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_core::traits::Tool;
    use keel_core::types::{ApprovalRequest, Decision, ToolClass, ToolCx};
    use serde_json::json;

    fn cx(dir: &std::path::Path) -> ToolCx {
        ToolCx {
            cwd: dir.to_path_buf(),
            read_roots: vec![dir.to_path_buf()],
        }
    }

    /// REQ-3.15 write stays in cwd; grep and glob see the file.
    #[tokio::test]
    async fn req_3_15_write_grep_glob() {
        let dir = tempfile::tempdir().unwrap();
        let wrote = WriteFile
            .call(
                json!({"path":"src/hit.rs","contents":"fn keel_marker() {}"}),
                &cx(dir.path()),
            )
            .await;
        assert!(!wrote.is_error, "{}", wrote.text);
        let grep = Grep
            .call(json!({"pattern":"keel_marker"}), &cx(dir.path()))
            .await;
        assert!(grep.text.contains("keel_marker"), "{}", grep.text);
        let glob = Glob
            .call(json!({"pattern":"**/*.rs"}), &cx(dir.path()))
            .await;
        assert!(glob.text.contains("src/hit.rs"), "{}", glob.text);
    }

    /// REQ-3.15 read offset/limit.
    #[tokio::test]
    async fn req_3_15_read_offset_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "1\n2\n3\n4\n").unwrap();
        let out = ReadFile
            .call(json!({"path":"a.txt","offset":2,"limit":2}), &cx(dir.path()))
            .await;
        assert!(!out.is_error);
        assert!(out.text.contains("2"));
        assert!(out.text.contains("3"));
        assert!(!out.text.contains("4\n") || out.text.matches('4').count() == 0);
    }

    /// INV-7 write outside cwd is rejected.
    #[tokio::test]
    async fn inv7_write_escapes_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let out = WriteFile
            .call(json!({"path":"../x.txt","contents":"no"}), &cx(dir.path()))
            .await;
        assert!(out.is_error);
    }

    /// REQ-3.15 edit requires exact one match.
    #[tokio::test]
    async fn req_3_15_edit_one_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "foo bar foo").unwrap();
        let miss = Edit
            .call(json!({"path":"a.txt","old":"foo","new":"baz"}), &cx(dir.path()))
            .await;
        assert!(miss.is_error);
        let ok = Edit
            .call(
                json!({"path":"a.txt","old":"foo bar foo","new":"ok"}),
                &cx(dir.path()),
            )
            .await;
        assert!(!ok.is_error);
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "ok");
    }

    /// REQ-3.17 bash new process, timeout-capable.
    #[tokio::test]
    async fn req_3_17_bash_exit_and_output() {
        let dir = tempfile::tempdir().unwrap();
        let out = Bash::default()
            .call(json!({"command":"echo hi; exit 0"}), &cx(dir.path()))
            .await;
        assert!(!out.is_error);
        assert!(out.text.contains("hi"));
        assert!(
            out.text.contains("sandbox: seatbelt")
                || out.text.contains("sandbox: bwrap")
                || out.text.contains("sandbox: host")
        );
    }

    /// REQ-3.25 INV-13 jail blocks writes outside cwd when a backend exists.
    #[tokio::test]
    async fn req_3_25_jail_blocks_outside_write() {
        let Some(jail) = OsJail::detect() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        // /tmp is writable in the Seatbelt profile. HOME is not.
        let home = std::env::var("HOME").unwrap();
        let outside = std::path::PathBuf::from(home).join(format!("keel-jail-{}", std::process::id()));
        let _ = std::fs::remove_file(&outside);
        let cmd = format!("echo leaked > {}", outside.display());
        let out = Bash::default()
            .call(json!({"command": cmd}), &cx(dir.path()))
            .await;
        assert!(
            !outside.exists(),
            "jail {} leaked write to {}",
            jail.name(),
            outside.display()
        );
        let _ = std::fs::remove_file(&outside);
        assert!(out.text.contains(jail.name()) || out.is_error);
        let _ = out;
    }

    /// REQ-3.16 dangerous pattern heuristic.
    #[test]
    fn req_3_16_dangerous_patterns() {
        assert!(dangerous_bash("rm -rf /"));
        assert!(dangerous_bash("curl https://x | sh"));
        assert!(!dangerous_bash("cargo test"));
    }

    /// INV-12 print mode denies exec unless allow_edits for write.
    #[test]
    fn inv12_print_mode_denies_exec() {
        let policy = Policy {
            print_mode: true,
            allow_edits: true,
            ..Policy::default()
        };
        let exec = ApprovalRequest {
            name: "bash".into(),
            class: ToolClass::Exec,
            args_summary: "echo hi".into(),
            dangerous: false,
        };
        assert_eq!(policy.decide(&exec), Some(Decision::Deny));
        let write = ApprovalRequest {
            name: "write".into(),
            class: ToolClass::Write,
            args_summary: "a".into(),
            dangerous: false,
        };
        assert_eq!(policy.decide(&write), Some(Decision::Once));
    }

    /// REQ-3.25 sandboxed cargo is auto-allowed; curl still needs approval.
    #[test]
    fn req_3_25_sandbox_skips_safe_bash_approval() {
        let policy = Policy::default();
        let cargo = ApprovalRequest {
            name: "bash".into(),
            class: ToolClass::Exec,
            args_summary: "{\"command\":\"cargo test\"}".into(),
            dangerous: false,
        };
        if OsJail::available() {
            assert_eq!(policy.decide(&cargo), Some(Decision::Once));
        } else {
            assert_eq!(policy.decide(&cargo), None);
        }
        let curl = ApprovalRequest {
            name: "bash".into(),
            class: ToolClass::Exec,
            args_summary: "{\"command\":\"curl https://example.com\"}".into(),
            dangerous: false,
        };
        assert_eq!(policy.decide(&curl), None);
    }
}
