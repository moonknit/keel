use async_trait::async_trait;
use keel_core::traits::Approver;
use keel_core::types::{ApprovalRequest, Decision, ToolClass};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

pub fn dangerous_bash(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    let needles = [
        "rm -rf", "rm -fr", "sudo ", "git push", "curl ", "wget ", "ssh ", "| sh", "|bash", "| sh",
        "mkfs", "dd if=",
    ];
    needles.iter().any(|n| lower.contains(n))
}

#[derive(Default)]
pub struct Policy {
    pub allow: Mutex<Vec<String>>,
    pub allow_edits: bool,
    pub print_mode: bool,
    pub plan_mode: AtomicBool,
}

impl Policy {
    pub fn set_plan_mode(&self, on: bool) {
        self.plan_mode.store(on, Ordering::Release);
    }

    pub fn is_plan_mode(&self) -> bool {
        self.plan_mode.load(Ordering::Acquire)
    }

    pub fn decide(&self, req: &ApprovalRequest) -> Option<Decision> {
        if req.class == ToolClass::Read {
            return Some(Decision::Once);
        }
        if self.is_plan_mode() {
            return Some(Decision::Deny);
        }
        if self.print_mode {
            if self.allow_edits && req.class == ToolClass::Write {
                return Some(Decision::Once);
            }
            return Some(Decision::Deny);
        }
        if req.name == "spawn" || req.name == "wait" {
            return Some(Decision::Once);
        }
        if self.allow_edits && req.class == ToolClass::Write {
            return Some(Decision::Once);
        }
        if req.name == "bash"
            && crate::jail::OsJail::available()
            && !dangerous_bash(&req.args_summary)
        {
            return Some(Decision::Once);
        }
        if req.name == "bash" && dangerous_bash(&req.args_summary) {
            return None;
        }
        let allow = self.allow.lock().ok()?;
        if allow.iter().any(|n| n == &req.name) {
            return Some(Decision::Once);
        }
        None
    }

    pub fn remember(&self, name: &str) {
        if let Ok(mut a) = self.allow.lock() {
            if !a.iter().any(|n| n == name) {
                a.push(name.into());
            }
        }
    }
}

pub struct PolicyApprover {
    pub policy: Policy,
    pub inner: Box<dyn Approver>,
}

#[async_trait]
impl Approver for PolicyApprover {
    async fn approve(&self, req: &ApprovalRequest) -> Decision {
        if let Some(d) = self.policy.decide(req) {
            return d;
        }
        let d = self.inner.approve(req).await;
        if d == Decision::Always {
            self.policy.remember(&req.name);
        }
        d
    }
}
