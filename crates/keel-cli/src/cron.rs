//! Host cron scheduler and job policy (REQ-3.32). Core does not parse schedules.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use cron::Schedule;
use keel_core::agent::{Agent, Inbox};
use keel_core::traits::{Approver, Store};
use keel_core::types::{ApprovalRequest, Decision, InboxMessage, ToolClass};
use keel_store::{CronJob, SqliteStore};
use keel_tools::dangerous_bash;
use std::str::FromStr;

pub fn parse_schedule(expr: &str) -> Result<Schedule> {
    let expr = expr.trim();
    let n = expr.split_whitespace().count();
    let expr = if n == 5 {
        format!("0 {expr}")
    } else {
        expr.to_string()
    };
    Schedule::from_str(&expr).map_err(|e| anyhow::anyhow!(e))
}

/// First fire is after `created_at` (or last run), not from the epoch.
pub fn job_due(schedule: &str, last_run_at: Option<i64>, created_at: i64, now_ms: i64) -> bool {
    let Ok(sched) = parse_schedule(schedule) else {
        return false;
    };
    let Some(now) = Utc.timestamp_millis_opt(now_ms).single() else {
        return false;
    };
    let after_ms = last_run_at.unwrap_or(created_at);
    let Some(after) = Utc.timestamp_millis_opt(after_ms).single() else {
        return false;
    };
    sched
        .after(&after)
        .next()
        .map(|next| next <= now)
        .unwrap_or(false)
}

pub struct CronApprover {
    pub allow_edits: bool,
    pub allow_bash: Vec<String>,
}

impl CronApprover {
    pub fn from_job(job: &CronJob) -> Self {
        Self {
            allow_edits: job.allow_edits,
            allow_bash: job
                .allow_bash
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        }
    }
}

#[async_trait]
impl Approver for CronApprover {
    async fn approve(&self, req: &ApprovalRequest) -> Decision {
        if req.class == ToolClass::Read || req.name == "web_search" {
            return Decision::Once;
        }
        if req.class == ToolClass::Write && self.allow_edits {
            return Decision::Once;
        }
        if req.name == "bash" {
            if dangerous_bash(&req.args_summary) {
                return Decision::Deny;
            }
            if self
                .allow_bash
                .iter()
                .any(|allowed| req.args_summary.contains(allowed))
            {
                return Decision::Once;
            }
        }
        Decision::Deny
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FireKind {
    Inbox,
    Run,
    Skip,
}

pub fn resolve_session_id(job: &CronJob, latest_cwd: Option<&str>) -> Option<String> {
    job.session_id
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| latest_cwd.map(str::to_string))
}

/// Same process + current session + a live run uses Inbox. A live foreign lock skips.
pub fn fire_kind(
    target_session: &str,
    lock_pid: Option<i64>,
    my_pid: i64,
    current_session: Option<&str>,
    running: bool,
    lock_alive: bool,
) -> FireKind {
    if lock_pid == Some(my_pid) && current_session == Some(target_session) && running {
        return FireKind::Inbox;
    }
    if lock_pid.is_some() && lock_pid != Some(my_pid) && lock_alive {
        return FireKind::Skip;
    }
    FireKind::Run
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DueAction {
    Inbox {
        session_id: String,
        prompt: String,
        job_id: String,
    },
    Run {
        session_id: String,
        job_id: String,
        prompt: String,
    },
    Skip {
        job_id: String,
    },
}

pub fn plan_due(
    jobs: &[CronJob],
    now_ms: i64,
    my_pid: i64,
    current_session: Option<&str>,
    running: bool,
    session_lock: &dyn Fn(&str) -> (Option<i64>, bool),
    latest_for_cwd: &dyn Fn(&str) -> Option<String>,
) -> Vec<DueAction> {
    let mut out = Vec::new();
    for job in jobs {
        if !job.enabled {
            continue;
        }
        if !job_due(&job.schedule, job.last_run_at, job.created_at, now_ms) {
            continue;
        }
        let Some(session_id) = resolve_session_id(job, latest_for_cwd(&job.cwd).as_deref()) else {
            out.push(DueAction::Skip {
                job_id: job.id.clone(),
            });
            continue;
        };
        let (pid, alive) = session_lock(&session_id);
        match fire_kind(&session_id, pid, my_pid, current_session, running, alive) {
            FireKind::Inbox => out.push(DueAction::Inbox {
                session_id,
                prompt: job.prompt.clone(),
                job_id: job.id.clone(),
            }),
            FireKind::Run => out.push(DueAction::Run {
                session_id,
                job_id: job.id.clone(),
                prompt: job.prompt.clone(),
            }),
            FireKind::Skip => out.push(DueAction::Skip {
                job_id: job.id.clone(),
            }),
        }
    }
    out
}

pub async fn apply_due(
    store: &SqliteStore,
    action: &DueAction,
    now_ms: i64,
    inbox: Option<&Inbox>,
) {
    match action {
        DueAction::Inbox { prompt, job_id, .. } => {
            if let Some(inbox) = inbox {
                inbox
                    .push(InboxMessage::Cron {
                        prompt: prompt.clone(),
                    })
                    .await;
            }
            let _ = store.mark_cron_run(job_id, "inbox", now_ms);
        }
        DueAction::Run { job_id, .. } => {
            let _ = store.mark_cron_run(job_id, "run", now_ms);
        }
        DueAction::Skip { job_id } => {
            let _ = store.mark_cron_run(job_id, "cron_skip", now_ms);
        }
    }
}

pub async fn tick(
    store: &SqliteStore,
    now_ms: i64,
    my_pid: i64,
    current_session: Option<&str>,
    running: bool,
    inbox: Option<&Inbox>,
) -> Vec<DueAction> {
    let jobs = store.list_cron(None).unwrap_or_default();
    let sessions = store.list_sessions(None, false).await.unwrap_or_default();
    let actions = plan_due(
        &jobs,
        now_ms,
        my_pid,
        current_session,
        running,
        &|id| match store.session_lock_pid(id) {
            Ok(Some(pid)) => (Some(pid), pid_alive(pid)),
            Ok(None) => (None, false),
            Err(_) => (None, false),
        },
        &|cwd| sessions.iter().find(|s| s.cwd == cwd).map(|s| s.id.clone()),
    );
    for action in &actions {
        apply_due(store, action, now_ms, inbox).await;
    }
    actions
}

fn pid_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(unix)]
mod libc {
    unsafe extern "C" {
        pub fn kill(pid: i32, sig: i32) -> i32;
    }
}

pub async fn fire_run(agent: &Agent, session_id: &str, prompt: &str, cwd: &str) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let cancel = tokio_util::sync::CancellationToken::new();
    let _ = agent.run(session_id, prompt, cwd, cancel, tx).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_core::types::ApprovalRequest;

    fn job(schedule: &str, created: i64, last: Option<i64>) -> CronJob {
        CronJob {
            id: "j1".into(),
            title: None,
            cwd: "/ws".into(),
            session_id: Some("s1".into()),
            schedule: schedule.into(),
            prompt: "do work".into(),
            allow_edits: false,
            allow_bash: String::new(),
            enabled: true,
            last_run_at: last,
            last_status: None,
            created_at: created,
            updated_at: created,
        }
    }

    #[test]
    fn req_3_32_due_uses_fake_clock_after_created() {
        let created = 0;
        let two_min = 120_000;
        assert!(job_due("* * * * *", None, created, two_min));
        assert!(!job_due("* * * * *", Some(two_min), created, two_min));
        assert!(!job_due("* * * * *", None, two_min, two_min));
    }

    #[test]
    fn req_3_32_job_policy_matches_print_and_denies_dangerous_bash() {
        let deny = CronApprover {
            allow_edits: false,
            allow_bash: vec![],
        };
        let read = ApprovalRequest {
            name: "read".into(),
            class: ToolClass::Read,
            args_summary: "a".into(),
            dangerous: false,
        };
        let write = ApprovalRequest {
            name: "write".into(),
            class: ToolClass::Write,
            args_summary: "a".into(),
            dangerous: false,
        };
        let bash = ApprovalRequest {
            name: "bash".into(),
            class: ToolClass::Exec,
            args_summary: "ls".into(),
            dangerous: false,
        };
        let danger = ApprovalRequest {
            name: "bash".into(),
            class: ToolClass::Exec,
            args_summary: "rm -rf /".into(),
            dangerous: true,
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            assert_eq!(deny.approve(&read).await, Decision::Once);
            assert_eq!(deny.approve(&write).await, Decision::Deny);
            assert_eq!(deny.approve(&bash).await, Decision::Deny);
            let edits = CronApprover {
                allow_edits: true,
                allow_bash: vec!["ls".into()],
            };
            assert_eq!(edits.approve(&write).await, Decision::Once);
            assert_eq!(edits.approve(&bash).await, Decision::Once);
            assert_eq!(edits.approve(&danger).await, Decision::Deny);
        });
    }

    #[test]
    fn req_3_32_inv20_skip_foreign_lock_inbox_same_run() {
        let me = 9;
        assert_eq!(
            fire_kind("s1", Some(1), me, Some("s1"), true, true),
            FireKind::Skip
        );
        assert_eq!(
            fire_kind("s1", Some(me), me, Some("s1"), true, true),
            FireKind::Inbox
        );
        assert_eq!(fire_kind("s1", None, me, None, false, false), FireKind::Run);
    }

    #[test]
    fn req_3_32_plan_due_marks_skip_without_session() {
        let jobs = vec![job("* * * * *", 0, None)];
        let actions = plan_due(&jobs, 120_000, 1, None, false, &|_| (None, false), &|_| {
            None
        });
        // session_id is set on job so this Runs
        assert!(matches!(actions[0], DueAction::Run { .. }));
        let mut orphan = job("* * * * *", 0, None);
        orphan.session_id = None;
        let skipped = plan_due(
            &[orphan],
            120_000,
            1,
            None,
            false,
            &|_| (None, false),
            &|_| None,
        );
        assert!(matches!(skipped[0], DueAction::Skip { .. }));
    }
}
