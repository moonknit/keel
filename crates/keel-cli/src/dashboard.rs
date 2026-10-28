//! TUI dashboard panel (REQ-3.33). Lists sessions and cron jobs. Does not start a chat.

use anyhow::Result;
use keel_core::SessionMeta;
use keel_store::{CronJob, SqliteStore};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DashCmd {
    Add {
        schedule: String,
        prompt: String,
        allow_edits: bool,
        allow_bash: String,
    },
    Rm(String),
    On(String),
    Off(String),
}

/// Dashboard input is job CRUD, never a new conversation prompt.
pub fn parse_dash(line: &str) -> Option<DashCmd> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("add ") {
        let (left, prompt) = rest.split_once(" -- ")?;
        let mut allow_edits = false;
        let mut allow_bash = String::new();
        let mut sched_parts = Vec::new();
        for tok in left.split_whitespace() {
            if tok == "+edits" {
                allow_edits = true;
            } else if let Some(b) = tok.strip_prefix("+bash=") {
                allow_bash = b.to_string();
            } else {
                sched_parts.push(tok);
            }
        }
        let schedule = sched_parts.join(" ");
        if schedule.is_empty() || prompt.trim().is_empty() {
            return None;
        }
        return Some(DashCmd::Add {
            schedule,
            prompt: prompt.trim().to_string(),
            allow_edits,
            allow_bash,
        });
    }
    if let Some(id) = line.strip_prefix("rm ") {
        let id = id.split_whitespace().next()?;
        return Some(DashCmd::Rm(id.into()));
    }
    if let Some(id) = line.strip_prefix("on ") {
        let id = id.split_whitespace().next()?;
        return Some(DashCmd::On(id.into()));
    }
    if let Some(id) = line.strip_prefix("off ") {
        let id = id.split_whitespace().next()?;
        return Some(DashCmd::Off(id.into()));
    }
    None
}

pub fn dashboard_text(
    sessions: &[SessionMeta],
    jobs: &[CronJob],
    running: bool,
    scheduler: bool,
) -> String {
    let mut out = String::from("dashboard — Esc back. HTTP is off.\n");
    out.push_str(&format!(
        "run: {}  scheduler: {}\n",
        if running { "yes" } else { "no" },
        if scheduler {
            "this process"
        } else {
            "other/none"
        }
    ));
    out.push_str("sessions:\n");
    if sessions.is_empty() {
        out.push_str("  (none)\n");
    }
    for s in sessions {
        out.push_str(&format!(
            "  {}  {}  {}\n",
            s.id,
            s.title.as_deref().unwrap_or("-"),
            s.cwd
        ));
    }
    out.push_str("jobs:\n");
    if jobs.is_empty() {
        out.push_str("  (none)\n");
    }
    for j in jobs {
        out.push_str(&format!(
            "  {}  {}  {}  {}\n",
            j.id,
            j.schedule,
            if j.enabled { "on" } else { "off" },
            j.prompt
        ));
    }
    out.push_str("add <cron> -- <prompt> | rm <id> | on <id> | off <id>");
    out
}

pub fn apply_dash(store: &SqliteStore, cwd: &str, cmd: DashCmd) -> Result<String> {
    match cmd {
        DashCmd::Add {
            schedule,
            prompt,
            allow_edits,
            allow_bash,
        } => {
            crate::cron::parse_schedule(&schedule)?;
            let job = store.add_cron(
                None,
                cwd,
                None,
                &schedule,
                &prompt,
                allow_edits,
                &allow_bash,
            )?;
            Ok(format!("added {}", job.id))
        }
        DashCmd::Rm(id) => {
            store.delete_cron(&id)?;
            Ok(format!("removed {id}"))
        }
        DashCmd::On(id) => {
            let mut job = store.get_cron(&id)?;
            job.enabled = true;
            store.update_cron(&job)?;
            Ok(format!("on {id}"))
        }
        DashCmd::Off(id) => {
            let mut job = store.get_cron(&id)?;
            job.enabled = false;
            store.update_cron(&job)?;
            Ok(format!("off {id}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_core::traits::Store;
    use keel_store::SqliteStore;

    #[test]
    fn req_3_33_parse_dash_is_job_crud_not_chat() {
        assert!(parse_dash("hello there").is_none());
        assert!(parse_dash("/new").is_none());
        let add = parse_dash("add * * * * * -- check mail").unwrap();
        assert!(matches!(add, DashCmd::Add { prompt, .. } if prompt == "check mail"));
        assert!(matches!(parse_dash("rm abc"), Some(DashCmd::Rm(id)) if id == "abc"));
    }

    #[test]
    fn req_3_33_inv21_apply_does_not_begin_chat() {
        let store = SqliteStore::open_memory().unwrap();
        let msg = apply_dash(
            &store,
            "/ws",
            parse_dash("add * * * * * -- nightly").unwrap(),
        )
        .unwrap();
        assert!(msg.starts_with("added "));
        assert_eq!(store.list_cron(None).unwrap().len(), 1);
        let sessions = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(store.list_sessions(None, true))
            .unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn req_3_33_inv21_panel_text_has_no_http() {
        let text = dashboard_text(&[], &[], false, false);
        assert!(text.contains("HTTP is off"));
        assert!(!text.contains("127.0.0.1"));
        assert!(text.contains("sessions:"));
        assert!(text.contains("jobs:"));
    }
}
