//! REPL slash commands: session switch and `/plan` (REQ-3.22, REQ-3.26).
//! Kept out of the rustyline loop so tests can drive Store/Policy without a TTY.

use keel_core::traits::Store;
use keel_core::{KeelError, Result, WorkingSession};
use keel_tools::Policy;

pub const HELP: &str = "/new /resume /name /session /skills /last /help /exit /plan";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionSwitch {
    New,
    Resume(Option<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanCommand {
    Enter,
    Approve,
    Deny,
}

pub fn parse_session_switch(line: &str) -> Option<SessionSwitch> {
    if line == "/new" {
        return Some(SessionSwitch::New);
    }
    if line == "/resume" {
        return Some(SessionSwitch::Resume(None));
    }
    let rest = line.strip_prefix("/resume ")?;
    let id = rest.split_whitespace().next().unwrap_or("");
    if id.is_empty() {
        Some(SessionSwitch::Resume(None))
    } else {
        Some(SessionSwitch::Resume(Some(id.to_string())))
    }
}

/// CLI `--resume` and REPL `/resume`: explicit id, else cwd latest, else create.
pub async fn resume_session(
    store: &dyn Store,
    cwd: &str,
    model: &str,
    id: Option<&str>,
) -> Result<WorkingSession> {
    if let Some(id) = id {
        return store.open_session(id).await;
    }
    let list = store.list_sessions(Some(cwd), false).await?;
    if let Some(meta) = list.into_iter().next() {
        store.open_session(&meta.id).await
    } else {
        store.create_session(cwd, model).await
    }
}

/// Close the current session, then create or open the target. On failure, reopen the previous id.
pub async fn apply_session_switch(
    store: &dyn Store,
    current_id: &str,
    cwd: &str,
    model: &str,
    switch: SessionSwitch,
) -> Result<String> {
    store.close(current_id).await?;
    let opened = match switch {
        SessionSwitch::New => store.create_session(cwd, model).await,
        SessionSwitch::Resume(id) => resume_session(store, cwd, model, id.as_deref()).await,
    };
    match opened {
        Ok(ws) => Ok(ws.meta.id),
        Err(e) => {
            if store.open_session(current_id).await.is_err() {
                return Err(KeelError::Msg(format!(
                    "{e}; also failed to restore session {current_id}"
                )));
            }
            Err(e)
        }
    }
}

pub fn parse_plan(line: &str) -> Option<PlanCommand> {
    if line == "/plan" {
        return Some(PlanCommand::Enter);
    }
    let rest = line.strip_prefix("/plan ")?;
    match rest.split_whitespace().next().unwrap_or("") {
        "approve" => Some(PlanCommand::Approve),
        "deny" => Some(PlanCommand::Deny),
        _ => None,
    }
}

/// Enter turns plan on. Approve restores the same Policy's REQ-3.16 defaults.
/// Deny leaves writes closed (plan stays on if it was on).
pub fn apply_plan(policy: &Policy, cmd: PlanCommand) -> &'static str {
    match cmd {
        PlanCommand::Enter => {
            policy.set_plan_mode(true);
            "plan"
        }
        PlanCommand::Approve => {
            policy.set_plan_mode(false);
            "approved"
        }
        PlanCommand::Deny => "denied",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_store::SqliteStore;

    fn pid() -> i64 {
        std::process::id() as i64
    }

    async fn meta(store: &SqliteStore, id: &str) -> keel_core::SessionMeta {
        store
            .list_sessions(None, false)
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.id == id)
            .expect("session")
    }

    /// REQ-3.22 `/new` and `/resume` parse as switch commands, not prompts.
    #[test]
    fn req_3_22_slash_parse_new_and_resume() {
        assert_eq!(parse_session_switch("/new"), Some(SessionSwitch::New));
        assert_eq!(
            parse_session_switch("/resume"),
            Some(SessionSwitch::Resume(None))
        );
        assert_eq!(
            parse_session_switch("/resume  abc  extra"),
            Some(SessionSwitch::Resume(Some("abc".into())))
        );
        assert!(parse_session_switch("/new extra").is_none());
        assert!(parse_session_switch("/session").is_none());
        assert!(parse_session_switch("hello").is_none());
    }

    /// REQ-3.22 `/new` closes the previous session and the next chat attaches to the new id.
    #[tokio::test]
    async fn req_3_22_slash_new_next_prompt_uses_new_session() {
        let store = SqliteStore::open_memory().unwrap();
        let a = store.create_session("/ws", "m").await.unwrap();
        let new_id = apply_session_switch(&store, &a.meta.id, "/ws", "m", SessionSwitch::New)
            .await
            .unwrap();
        assert_ne!(new_id, a.meta.id);
        assert!(meta(&store, &a.meta.id).await.lock_pid.is_none());
        assert_eq!(meta(&store, &new_id).await.lock_pid, Some(pid()));

        store.begin_chat(&new_id, Some("after-new")).await.unwrap();
        let ws = store.open_session(&new_id).await.unwrap();
        assert_eq!(ws.meta.id, new_id);
        assert_eq!(ws.chats.last().unwrap().session_id, new_id);
        assert_eq!(ws.chats.last().unwrap().title.as_deref(), Some("after-new"));
    }

    /// REQ-3.22 `/resume <id>` routes later chats to that session and releases the previous lock.
    #[tokio::test]
    async fn req_3_22_slash_resume_id_routes_later_chats() {
        let store = SqliteStore::open_memory().unwrap();
        let a = store.create_session("/ws", "m").await.unwrap();
        let a_id = a.meta.id.clone();
        let b_id = apply_session_switch(&store, &a_id, "/ws", "m", SessionSwitch::New)
            .await
            .unwrap();
        let back = apply_session_switch(
            &store,
            &b_id,
            "/ws",
            "m",
            SessionSwitch::Resume(Some(a_id.clone())),
        )
        .await
        .unwrap();
        assert_eq!(back, a_id);
        assert!(meta(&store, &b_id).await.lock_pid.is_none());
        assert_eq!(meta(&store, &a_id).await.lock_pid, Some(pid()));

        store.begin_chat(&back, Some("after-resume")).await.unwrap();
        let ws = store.open_session(&back).await.unwrap();
        assert_eq!(ws.chats.last().unwrap().session_id, a_id);
        assert_eq!(
            ws.chats.last().unwrap().title.as_deref(),
            Some("after-resume")
        );
    }

    /// REQ-3.22 `/resume` without id picks the cwd list head, same idea as CLI `--resume`.
    #[tokio::test]
    async fn req_3_22_slash_resume_without_id_picks_cwd_list() {
        let store = SqliteStore::open_memory().unwrap();
        let a = store.create_session("/ws-a", "m").await.unwrap();
        let b = store.create_session("/ws-b", "m").await.unwrap();
        let picked = apply_session_switch(
            &store,
            &a.meta.id,
            "/ws-b",
            "m",
            SessionSwitch::Resume(None),
        )
        .await
        .unwrap();
        assert_eq!(picked, b.meta.id);
        assert!(meta(&store, &a.meta.id).await.lock_pid.is_none());
        assert_eq!(meta(&store, &b.meta.id).await.lock_pid, Some(pid()));
    }

    /// REQ-3.22 a failed `/resume` must not leave the previous session unlocked.
    #[tokio::test]
    async fn req_3_22_slash_resume_unknown_restores_current() {
        let store = SqliteStore::open_memory().unwrap();
        let a = store.create_session("/ws", "m").await.unwrap();
        let err = apply_session_switch(
            &store,
            &a.meta.id,
            "/ws",
            "m",
            SessionSwitch::Resume(Some("nope".into())),
        )
        .await;
        assert!(err.is_err());
        assert_eq!(meta(&store, &a.meta.id).await.lock_pid, Some(pid()));
    }

    fn write_req() -> keel_core::types::ApprovalRequest {
        keel_core::types::ApprovalRequest {
            name: "write".into(),
            class: keel_core::types::ToolClass::Write,
            args_summary: "a".into(),
            dangerous: false,
        }
    }

    /// REQ-3.22 `/plan` enter / approve / deny parse as commands, not prompts.
    #[test]
    fn req_3_22_slash_parse_plan() {
        assert_eq!(parse_plan("/plan"), Some(PlanCommand::Enter));
        assert_eq!(parse_plan("/plan approve"), Some(PlanCommand::Approve));
        assert_eq!(parse_plan("/plan  deny  extra"), Some(PlanCommand::Deny));
        assert!(parse_plan("/plan foo").is_none());
        assert!(parse_plan("/plans").is_none());
        assert!(parse_plan("/compact").is_none());
        assert!(HELP.contains("/plan"));
    }

    /// REQ-3.26 REQ-3.22 `/plan` denies write; `/plan approve` restores ask on the same Policy.
    #[test]
    fn req_3_26_req_3_22_plan_enter_denies_write_approve_asks() {
        let policy = Policy::default();
        apply_plan(&policy, PlanCommand::Enter);
        assert_eq!(
            policy.decide(&write_req()),
            Some(keel_core::types::Decision::Deny)
        );
        apply_plan(&policy, PlanCommand::Approve);
        assert!(!policy.is_plan_mode());
        assert_eq!(policy.decide(&write_req()), None);
    }

    /// REQ-3.26 REQ-3.22 `/plan deny` does not open writes.
    #[test]
    fn req_3_26_req_3_22_plan_deny_keeps_write_denied() {
        let policy = Policy::default();
        apply_plan(&policy, PlanCommand::Enter);
        apply_plan(&policy, PlanCommand::Deny);
        assert!(policy.is_plan_mode());
        assert_eq!(
            policy.decide(&write_req()),
            Some(keel_core::types::Decision::Deny)
        );
    }
}
