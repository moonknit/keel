//! Host launcher for `spawn` / `wait` (REQ-3.27). The kernel stays a single ReAct loop.

use async_trait::async_trait;
use keel_core::agent::{Agent, CwdWriteLock, Inbox};
use keel_core::traits::{Approver, ContextSource, Model, Store, Tool};
use keel_core::types::{
    ApprovalRequest, Decision, InboxMessage, ItemKind, Role, RunStatus, ToolClass, ToolCx,
    ToolOutput, ToolSpec, WorkingSession,
};
use keel_tools::{
    CHILD_LIMIT_MSG, MAX_CHILDREN, child_summary, parse_spawn_args, parse_wait_args, spawn_spec,
    wait_spec,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct ChildResult {
    summary: String,
    is_error: bool,
}

struct ChildEntry {
    done: Option<ChildResult>,
    waiters: Vec<oneshot::Sender<ChildResult>>,
    cancel: CancellationToken,
}

#[derive(Clone, Default)]
pub struct ChildHub {
    inner: Arc<Mutex<HashMap<String, ChildEntry>>>,
}

impl ChildHub {
    pub fn new() -> Self {
        Self::default()
    }

    async fn running(&self) -> usize {
        self.inner
            .lock()
            .await
            .values()
            .filter(|e| e.done.is_none())
            .count()
    }

    async fn start(&self, id: &str, parent: &CancellationToken) -> Result<(), String> {
        let mut g = self.inner.lock().await;
        if g.values().filter(|e| e.done.is_none()).count() >= MAX_CHILDREN {
            return Err(CHILD_LIMIT_MSG.into());
        }
        g.insert(
            id.to_string(),
            ChildEntry {
                done: None,
                waiters: Vec::new(),
                cancel: parent.child_token(),
            },
        );
        Ok(())
    }

    pub async fn child_cancel(&self, id: &str) -> Option<CancellationToken> {
        self.inner.lock().await.get(id).map(|e| e.cancel.clone())
    }

    #[cfg(test)]
    async fn ids(&self) -> Vec<String> {
        self.inner.lock().await.keys().cloned().collect()
    }

    /// Returns true when the host should post Inbox `Subagent` (no waiter claimed it).
    async fn finish(&self, id: &str, result: ChildResult) -> bool {
        let mut g = self.inner.lock().await;
        let Some(e) = g.get_mut(id) else {
            return false;
        };
        e.done = Some(result.clone());
        let had_waiters = !e.waiters.is_empty();
        for tx in e.waiters.drain(..) {
            let _ = tx.send(result.clone());
        }
        !had_waiters
    }

    async fn wait(&self, ids: Option<Vec<String>>) -> Vec<ChildResult> {
        let mut rxs = Vec::new();
        let mut ready = Vec::new();
        {
            let mut g = self.inner.lock().await;
            let targets: Vec<String> = match ids {
                Some(v) => v,
                None => g
                    .iter()
                    .filter(|(_, e)| e.done.is_none())
                    .map(|(k, _)| k.clone())
                    .collect(),
            };
            if targets.is_empty() {
                return vec![ChildResult {
                    summary: "no unfinished children".into(),
                    is_error: false,
                }];
            }
            for id in targets {
                match g.get_mut(&id) {
                    Some(e) => {
                        if let Some(r) = &e.done {
                            ready.push(r.clone());
                        } else {
                            let (tx, rx) = oneshot::channel();
                            e.waiters.push(tx);
                            rxs.push(rx);
                        }
                    }
                    None => ready.push(ChildResult {
                        summary: format!("unknown child {id}"),
                        is_error: true,
                    }),
                }
            }
        }
        for rx in rxs {
            ready.push(rx.await.unwrap_or(ChildResult {
                summary: "interrupted".into(),
                is_error: true,
            }));
        }
        ready
    }
}

#[derive(Clone)]
pub struct Spawn {
    pub store: Arc<dyn Store>,
    pub model: Arc<dyn Model>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub approver: Arc<dyn Approver>,
    pub assembler: Arc<dyn ContextSource>,
    pub write_lock: CwdWriteLock,
    pub system: String,
    pub max_turns: u32,
    pub model_name: String,
    pub inbox: Inbox,
    pub hub: ChildHub,
}

/// Child Write/Exec prompts land on the parent Approver (REQ-3.27). No child TTY.
struct ParentRoutedApprover {
    inner: Arc<dyn Approver>,
    child_session: String,
}

#[async_trait]
impl Approver for ParentRoutedApprover {
    async fn approve(&self, req: &ApprovalRequest) -> Decision {
        let tagged = ApprovalRequest {
            name: req.name.clone(),
            class: req.class,
            args_summary: format!("child {}: {}", self.child_session, req.args_summary),
            dangerous: req.dangerous,
        };
        self.inner.approve(&tagged).await
    }
}

impl Spawn {
    fn child_agent(&self, child_id: &str) -> Agent {
        Agent {
            model: self.model.clone(),
            tools: self.tools.clone(),
            store: self.store.clone(),
            approver: Arc::new(ParentRoutedApprover {
                inner: self.approver.clone(),
                child_session: child_id.to_string(),
            }),
            assembler: self.assembler.clone(),
            max_turns: self.max_turns,
            system: self.system.clone(),
            write_lock: self.write_lock.clone(),
            inbox: Inbox::default(),
        }
    }

    async fn run_child(
        &self,
        child_id: &str,
        task: &str,
        cwd: &str,
        child_ws: WorkingSession,
        cancel: CancellationToken,
    ) -> ChildResult {
        let (tx, _rx) = mpsc::unbounded_channel();
        let status = match self
            .child_agent(child_id)
            .run(child_id, task, cwd, cancel, tx)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                let body =
                    assistant_text(&self.store.open_session(child_id).await.unwrap_or(child_ws));
                let _ = self.store.close(child_id).await;
                return ChildResult {
                    summary: child_summary(child_id, "error", &format!("{e}\n{body}")),
                    is_error: true,
                };
            }
        };
        let opened = self
            .store
            .open_session(child_id)
            .await
            .unwrap_or_else(|_| child_ws);
        let body = assistant_text(&opened);
        let _ = self.store.close(child_id).await;
        let status_s = match status {
            RunStatus::Done => "done",
            RunStatus::Interrupted => "interrupted",
            RunStatus::Error => "error",
            RunStatus::Limit => "limit",
        };
        ChildResult {
            summary: child_summary(child_id, status_s, &body),
            is_error: matches!(status, RunStatus::Error | RunStatus::Interrupted),
        }
    }
}

#[async_trait]
impl Tool for Spawn {
    fn spec(&self) -> ToolSpec {
        spawn_spec()
    }

    fn class(&self) -> ToolClass {
        ToolClass::External
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> ToolOutput {
        let parsed = match parse_spawn_args(&args) {
            Ok(p) => p,
            Err(e) => {
                return ToolOutput {
                    text: e,
                    is_error: true,
                };
            }
        };
        if self.hub.running().await >= MAX_CHILDREN {
            return ToolOutput {
                text: CHILD_LIMIT_MSG.into(),
                is_error: true,
            };
        }
        let cwd = cx.cwd.to_string_lossy().into_owned();
        let child_ws = match self.store.create_session(&cwd, &self.model_name).await {
            Ok(ws) => ws,
            Err(e) => {
                return ToolOutput {
                    text: e.to_string(),
                    is_error: true,
                };
            }
        };
        let child_id = child_ws.meta.id.clone();
        if let Err(e) = self.hub.start(&child_id, &cx.cancel).await {
            let _ = self.store.close(&child_id).await;
            return ToolOutput {
                text: e,
                is_error: true,
            };
        }
        let Some(cancel) = self.hub.child_cancel(&child_id).await else {
            let _ = self.store.close(&child_id).await;
            return ToolOutput {
                text: "child token missing".into(),
                is_error: true,
            };
        };
        if parsed.wait {
            let result = self
                .run_child(&child_id, &parsed.task, &cwd, child_ws, cancel)
                .await;
            let _ = self.hub.finish(&child_id, result.clone()).await;
            return ToolOutput {
                text: result.summary,
                is_error: result.is_error,
            };
        }
        let spawn = self.clone();
        let inbox = self.inbox.clone();
        let hub = self.hub.clone();
        let task = parsed.task;
        let child_id_bg = child_id.clone();
        tokio::spawn(async move {
            let result = spawn
                .run_child(&child_id_bg, &task, &cwd, child_ws, cancel)
                .await;
            let summary = result.summary.clone();
            let is_error = result.is_error;
            if hub.finish(&child_id_bg, result).await {
                inbox
                    .push(InboxMessage::Subagent {
                        call_id: child_id_bg,
                        text: summary,
                        is_error,
                    })
                    .await;
            }
        });
        ToolOutput {
            text: format!("child_session: {child_id}"),
            is_error: false,
        }
    }
}

pub struct Wait {
    pub hub: ChildHub,
}

#[async_trait]
impl Tool for Wait {
    fn spec(&self) -> ToolSpec {
        wait_spec()
    }

    fn class(&self) -> ToolClass {
        ToolClass::External
    }

    async fn call(&self, args: Value, _cx: &ToolCx) -> ToolOutput {
        let parsed = parse_wait_args(&args);
        let results = self.hub.wait(parsed.ids).await;
        let is_error = results.iter().any(|r| r.is_error);
        ToolOutput {
            text: results
                .into_iter()
                .map(|r| r.summary)
                .collect::<Vec<_>>()
                .join("\n\n"),
            is_error,
        }
    }
}

fn assistant_text(ws: &WorkingSession) -> String {
    let Some(chat) = ws.chats.last() else {
        return String::new();
    };
    chat.items
        .iter()
        .filter_map(|item| match &item.kind {
            ItemKind::Text { text } if item.role == Role::Assistant => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::last::last_tool_output;
    use keel_core::WorkingSession;
    use keel_core::assembler::ContextAssembler;
    use keel_core::scripted::{FakeTool, ScriptedModel};
    use keel_core::traits::AlwaysAllow;
    use keel_core::types::{
        ApprovalRequest, ChatStatus, Decision, Event, Item, ItemKind, ModelEvent, Role, ToolSpec,
        Usage,
    };
    use keel_core::{CwdWriteLock, Inbox, MemoryStore};
    use keel_tools::{Policy, PolicyApprover};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;

    fn item_call(name: &str, id: &str, args: Value) -> Item {
        Item {
            id: id.into(),
            role: Role::Assistant,
            kind: ItemKind::ToolCall {
                call_id: id.into(),
                name: name.into(),
                args,
            },
            raw: None,
        }
    }

    fn item_text(text: &str) -> Item {
        Item::text(Role::Assistant, text)
    }

    fn turn_call(name: &str, id: &str, args: Value) -> Vec<ModelEvent> {
        vec![
            ModelEvent::ItemDone(item_call(name, id, args)),
            ModelEvent::Completed {
                usage: Usage::default(),
            },
        ]
    }

    fn turn_calls(calls: Vec<(&str, &str, Value)>) -> Vec<ModelEvent> {
        let mut ev: Vec<ModelEvent> = calls
            .into_iter()
            .map(|(name, id, args)| ModelEvent::ItemDone(item_call(name, id, args)))
            .collect();
        ev.push(ModelEvent::Completed {
            usage: Usage::default(),
        });
        ev
    }

    fn turn_text(text: &str) -> Vec<ModelEvent> {
        vec![
            ModelEvent::TextDelta(text.into()),
            ModelEvent::ItemDone(item_text(text)),
            ModelEvent::Completed {
                usage: Usage::default(),
            },
        ]
    }

    fn tool_outputs(ws: &keel_core::WorkingSession) -> Vec<String> {
        ws.chats
            .iter()
            .flat_map(|c| c.items.iter())
            .filter_map(|item| match &item.kind {
                ItemKind::ToolResult { output, .. } => Some(output.clone()),
                _ => None,
            })
            .collect()
    }

    fn assert_inv4_paired(ws: &WorkingSession, call_id: &str) {
        let items: Vec<_> = ws.chats.iter().flat_map(|c| c.items.iter()).collect();
        let calls = items
            .iter()
            .filter(|i| matches!(&i.kind, ItemKind::ToolCall { call_id: id, .. } if id == call_id))
            .count();
        let results = items
            .iter()
            .filter(
                |i| matches!(&i.kind, ItemKind::ToolResult { call_id: id, .. } if id == call_id),
            )
            .count();
        assert_eq!(calls, 1, "tool_call {call_id} missing: {items:?}");
        assert_eq!(
            results, 1,
            "tool_result {call_id} must pair exactly once: {items:?}"
        );
    }

    async fn wait_flag(flag: &AtomicBool) {
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(2);
        while !flag.load(Ordering::SeqCst) {
            if tokio::time::Instant::now() > deadline {
                panic!("flag was not set");
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
        }
    }

    struct HoldTool {
        release: Arc<AtomicBool>,
        kick: Arc<Notify>,
        started: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Tool for HoldTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "hold".into(),
                description: "hold".into(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            }
        }
        fn class(&self) -> ToolClass {
            ToolClass::Read
        }
        async fn call(&self, _args: Value, _cx: &ToolCx) -> ToolOutput {
            self.started.store(true, Ordering::SeqCst);
            while !self.release.load(Ordering::SeqCst) {
                self.kick.notified().await;
            }
            ToolOutput {
                text: "held".into(),
                is_error: false,
            }
        }
    }

    fn make_spawn(
        store: Arc<MemoryStore>,
        child_model: ScriptedModel,
        child_tools: Vec<Arc<dyn Tool>>,
    ) -> Spawn {
        let write_lock = CwdWriteLock::new();
        Spawn {
            store: store.clone(),
            model: Arc::new(child_model),
            tools: child_tools,
            approver: Arc::new(AlwaysAllow),
            assembler: Arc::new(ContextAssembler::default()),
            write_lock,
            system: "child".into(),
            max_turns: 4,
            model_name: "test-model".into(),
            inbox: Inbox::default(),
            hub: ChildHub::new(),
        }
    }

    /// REQ-3.27 INV-15 INV-16 wait=true opens a new child session and parents only the summary.
    #[tokio::test]
    async fn req_3_27_inv_15_inv_16_spawn_wait_true_new_session() {
        let store = Arc::new(MemoryStore::default());
        let parent_id = store
            .create_session("/tmp/ws", "test-model")
            .await
            .unwrap()
            .meta
            .id;
        let shared: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec![
            turn_call(
                "spawn",
                "sp1",
                serde_json::json!({"task":"child work","wait":true}),
            ),
            turn_call("probe", "ch1", serde_json::json!({})),
            turn_text("child-summary-text"),
            turn_text("parent-done"),
        ]));
        let spawn = Spawn {
            store: store.clone(),
            model: shared.clone(),
            tools: vec![Arc::new(FakeTool::new("probe", "CHILD_TOOL_SECRET"))],
            approver: Arc::new(AlwaysAllow),
            assembler: Arc::new(ContextAssembler::default()),
            write_lock: CwdWriteLock::new(),
            system: "child".into(),
            max_turns: 4,
            model_name: "test-model".into(),
            inbox: Inbox::default(),
            hub: ChildHub::new(),
        };
        assert!(
            spawn
                .tools
                .iter()
                .all(|t| t.spec().name != "spawn" && t.spec().name != "wait"),
            "child must not have spawn/wait"
        );
        let agent = Agent {
            model: shared,
            tools: vec![
                Arc::new(spawn.clone()),
                Arc::new(Wait {
                    hub: spawn.hub.clone(),
                }),
            ],
            store: store.clone(),
            approver: Arc::new(AlwaysAllow),
            assembler: Arc::new(ContextAssembler::default()),
            max_turns: 4,
            system: "parent".into(),
            write_lock: spawn.write_lock.clone(),
            inbox: spawn.inbox.clone(),
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        agent
            .run(&parent_id, "hello", "/tmp/ws", CancellationToken::new(), tx)
            .await
            .unwrap();
        let parent = store.open_session(&parent_id).await.unwrap();

        let spawn_result = parent
            .chats
            .iter()
            .flat_map(|c| c.items.iter())
            .find_map(|item| match &item.kind {
                ItemKind::ToolResult { output, .. } => Some(output.as_str()),
                _ => None,
            })
            .expect("parent spawn tool_result");
        assert!(spawn_result.contains("child_session: "), "{spawn_result}");
        assert!(spawn_result.contains("status: done"), "{spawn_result}");
        assert!(
            spawn_result.contains("child-summary-text"),
            "{spawn_result}"
        );
        assert!(
            !spawn_result.contains("CHILD_TOOL_SECRET"),
            "child tool log leaked into parent summary: {spawn_result}"
        );

        let child_id = spawn_result
            .lines()
            .find_map(|l| l.strip_prefix("child_session: "))
            .expect("child id");
        assert_ne!(child_id, parent_id);
        assert!(
            parent
                .chats
                .iter()
                .flat_map(|c| c.items.iter())
                .all(|item| match &item.kind {
                    ItemKind::ToolCall { name, .. } => name == "spawn",
                    ItemKind::ToolResult { output, .. } => !output.contains("CHILD_TOOL_SECRET"),
                    _ => true,
                }),
            "parent transcript must not unfold child tool logs"
        );
        assert_eq!(last_tool_output(&parent), Some(spawn_result));

        let child = store.open_session(child_id).await.unwrap();
        assert_eq!(child.meta.id, child_id);
        assert_ne!(child.meta.id, parent_id);
        assert_eq!(child.meta.lock_pid, Some(std::process::id() as i64));
        let child_blob = format!("{:?}", child.chats);
        assert!(
            child_blob.contains("CHILD_TOOL_SECRET"),
            "child session missing tool body: {child_blob}"
        );
        assert!(child_blob.contains("child-summary-text"), "{child_blob}");
        assert!(
            child.chats.iter().flat_map(|c| c.items.iter()).any(
                |item| matches!(&item.kind, ItemKind::ToolCall { name, .. } if name == "probe")
            ),
            "child tool_call stayed in child session"
        );
        assert!(
            !child.chats.iter().flat_map(|c| c.items.iter()).any(
                |item| matches!(&item.kind, ItemKind::ToolCall { name, .. } if name == "spawn")
            ),
            "child must not reopen the parent spawn call"
        );
    }

    /// REQ-3.27 wait=false returns the child id immediately; parent turn continues.
    #[tokio::test]
    async fn req_3_27_spawn_wait_false_returns_id() {
        let store = Arc::new(MemoryStore::default());
        let listed_before = store.list_sessions(None, true).await.unwrap().len();
        let spawn = make_spawn(
            store.clone(),
            ScriptedModel::new(vec![turn_text("async-child")]),
            vec![],
        );
        let parent_model = ScriptedModel::new(vec![
            turn_call(
                "spawn",
                "sp1",
                serde_json::json!({"task":"later","wait":false}),
            ),
            turn_text("parent-done"),
        ]);
        let parent = store.create_session("/tmp/ws", "test-model").await.unwrap();
        let agent = Agent {
            model: Arc::new(parent_model),
            tools: vec![Arc::new(spawn.clone())],
            store: store.clone(),
            approver: Arc::new(AlwaysAllow),
            assembler: Arc::new(ContextAssembler::default()),
            max_turns: 3,
            system: "parent".into(),
            write_lock: spawn.write_lock.clone(),
            inbox: spawn.inbox.clone(),
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        agent
            .run(
                &parent.meta.id,
                "hello",
                "/tmp/ws",
                CancellationToken::new(),
                tx,
            )
            .await
            .unwrap();
        let ws = store.open_session(&parent.meta.id).await.unwrap();
        let result = tool_outputs(&ws)
            .into_iter()
            .find(|o| o.starts_with("child_session: ") && !o.contains("status:"))
            .expect("immediate child id");
        let child_id = result
            .lines()
            .find_map(|l| l.strip_prefix("child_session: "))
            .expect("child id")
            .to_string();
        spawn.hub.wait(Some(vec![child_id.clone()])).await;
        let listed = store.list_sessions(None, true).await.unwrap();
        assert_eq!(
            listed.len(),
            listed_before + 2,
            "wait=false opens a child session"
        );
        let child = store.open_session(&child_id).await.unwrap();
        assert_ne!(child.meta.id, parent.meta.id);
    }

    /// REQ-3.4 REQ-3.27 wait collects the async child summary as its tool_result.
    #[tokio::test]
    async fn req_3_4_req_3_27_wait_collects_async_child_summary() {
        let store = Arc::new(MemoryStore::default());
        let spawn = make_spawn(
            store.clone(),
            ScriptedModel::new(vec![turn_text("from-async-child")]),
            vec![],
        );
        let parent_model = ScriptedModel::new(vec![
            turn_call(
                "spawn",
                "sp1",
                serde_json::json!({"task":"later","wait":false}),
            ),
            turn_call("wait", "w1", serde_json::json!({})),
            turn_text("parent-done"),
        ]);
        let parent = store.create_session("/tmp/ws", "test-model").await.unwrap();
        let agent = Agent {
            model: Arc::new(parent_model),
            tools: vec![
                Arc::new(spawn.clone()),
                Arc::new(Wait {
                    hub: spawn.hub.clone(),
                }),
            ],
            store: store.clone(),
            approver: Arc::new(AlwaysAllow),
            assembler: Arc::new(ContextAssembler::default()),
            max_turns: 4,
            system: "parent".into(),
            write_lock: spawn.write_lock.clone(),
            inbox: spawn.inbox.clone(),
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        agent
            .run(
                &parent.meta.id,
                "hello",
                "/tmp/ws",
                CancellationToken::new(),
                tx,
            )
            .await
            .unwrap();
        let ws = store.open_session(&parent.meta.id).await.unwrap();
        let outs = tool_outputs(&ws);
        assert!(
            outs.iter()
                .any(|o| o.starts_with("child_session: ") && !o.contains("status:")),
            "spawn id first: {outs:?}"
        );
        let wait_out = outs
            .iter()
            .find(|o| o.contains("from-async-child") && o.contains("status: done"))
            .expect("wait summary");
        assert!(
            !wait_out.contains("CHILD_TOOL_SECRET"),
            "child body leaked: {wait_out}"
        );
        assert_eq!(last_tool_output(&ws), Some(wait_out.as_str()));
    }

    /// REQ-3.4 REQ-3.27 async completion between parent runs is Inbox Subagent → tool_result.
    #[tokio::test]
    async fn req_3_4_req_3_27_inbox_subagent_between_parent_runs() {
        let store = Arc::new(MemoryStore::default());
        let spawn = make_spawn(
            store.clone(),
            ScriptedModel::new(vec![turn_text("inbox-child-summary")]),
            vec![],
        );
        let parent = store.create_session("/tmp/ws", "test-model").await.unwrap();
        let agent = Agent {
            model: Arc::new(ScriptedModel::new(vec![
                turn_call(
                    "spawn",
                    "sp1",
                    serde_json::json!({"task":"later","wait":false}),
                ),
                turn_text("parent-1"),
            ])),
            tools: vec![Arc::new(spawn.clone())],
            store: store.clone(),
            approver: Arc::new(AlwaysAllow),
            assembler: Arc::new(ContextAssembler::default()),
            max_turns: 3,
            system: "parent".into(),
            write_lock: spawn.write_lock.clone(),
            inbox: spawn.inbox.clone(),
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        agent
            .run(
                &parent.meta.id,
                "first",
                "/tmp/ws",
                CancellationToken::new(),
                tx,
            )
            .await
            .unwrap();
        let ws1 = store.open_session(&parent.meta.id).await.unwrap();
        let id_out = last_tool_output(&ws1).expect("id");
        let child_id = id_out
            .lines()
            .find_map(|l| l.strip_prefix("child_session: "))
            .expect("child id")
            .to_string();
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(2);
        while spawn.inbox.is_empty().await {
            if tokio::time::Instant::now() > deadline {
                panic!("Inbox Subagent was not posted");
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
        }

        let agent = Agent {
            model: Arc::new(ScriptedModel::new(vec![turn_text("parent-2")])),
            tools: vec![Arc::new(spawn.clone())],
            store: store.clone(),
            approver: Arc::new(AlwaysAllow),
            assembler: Arc::new(ContextAssembler::default()),
            max_turns: 2,
            system: "parent".into(),
            write_lock: spawn.write_lock.clone(),
            inbox: spawn.inbox.clone(),
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        agent
            .run(
                &parent.meta.id,
                "second",
                "/tmp/ws",
                CancellationToken::new(),
                tx,
            )
            .await
            .unwrap();
        let ws2 = store.open_session(&parent.meta.id).await.unwrap();
        let outs = tool_outputs(&ws2);
        assert!(
            outs.iter()
                .any(|o| o.contains("inbox-child-summary") && o.contains(&child_id)),
            "Inbox Subagent summary missing: {outs:?}"
        );
        assert!(
            outs.iter().all(|o| !o.contains("CHILD_TOOL_SECRET")),
            "child tool log leaked: {outs:?}"
        );
    }

    /// REQ-3.27 a fifth concurrent spawn is a limit observation; the loop stays up.
    #[tokio::test]
    async fn req_3_27_fifth_spawn_is_limit_observation() {
        let store = Arc::new(MemoryStore::default());
        let release = Arc::new(AtomicBool::new(false));
        let kick = Arc::new(Notify::new());
        let hold = HoldTool {
            release: release.clone(),
            kick: kick.clone(),
            started: Arc::new(AtomicBool::new(false)),
        };
        let mut child_turns = Vec::new();
        for i in 0..MAX_CHILDREN {
            child_turns.push(turn_call("hold", &format!("h{i}"), serde_json::json!({})));
        }
        for _ in 0..MAX_CHILDREN {
            child_turns.push(turn_text("held-child"));
        }
        let spawn = make_spawn(
            store.clone(),
            ScriptedModel::new(child_turns),
            vec![Arc::new(hold)],
        );
        let mut calls = Vec::new();
        for i in 0..5 {
            calls.push((
                "spawn",
                format!("sp{i}"),
                serde_json::json!({"task": format!("c{i}"), "wait": false}),
            ));
        }
        let parent_model = ScriptedModel::new(vec![
            turn_calls(
                calls
                    .iter()
                    .map(|(n, id, a)| (*n, id.as_str(), a.clone()))
                    .collect(),
            ),
            turn_text("parent-after-limit"),
        ]);
        let parent = store.create_session("/tmp/ws", "test-model").await.unwrap();
        let agent = Agent {
            model: Arc::new(parent_model),
            tools: vec![Arc::new(spawn.clone())],
            store: store.clone(),
            approver: Arc::new(AlwaysAllow),
            assembler: Arc::new(ContextAssembler::default()),
            max_turns: 4,
            system: "parent".into(),
            write_lock: spawn.write_lock.clone(),
            inbox: spawn.inbox.clone(),
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let status = agent
            .run(
                &parent.meta.id,
                "hello",
                "/tmp/ws",
                CancellationToken::new(),
                tx,
            )
            .await
            .unwrap();
        assert_eq!(status, RunStatus::Done);
        let ws = store.open_session(&parent.meta.id).await.unwrap();
        let outs = tool_outputs(&ws);
        let ids: Vec<_> = outs
            .iter()
            .filter(|o| o.starts_with("child_session: ") && !o.contains("status:"))
            .cloned()
            .collect();
        assert_eq!(ids.len(), 4, "four children started: {outs:?}");
        assert!(
            outs.iter().any(|o| o.contains(CHILD_LIMIT_MSG)),
            "fifth spawn must be limit: {outs:?}"
        );
        release.store(true, Ordering::SeqCst);
        kick.notify_waiters();
        let child_ids: Vec<String> = ids
            .iter()
            .filter_map(|o| {
                o.lines()
                    .find_map(|l| l.strip_prefix("child_session: "))
                    .map(str::to_string)
            })
            .collect();
        spawn.hub.wait(Some(child_ids)).await;
    }

    struct RecordingApprover {
        hits: Arc<Mutex<Vec<ApprovalRequest>>>,
        decision: Decision,
    }

    #[async_trait]
    impl Approver for RecordingApprover {
        async fn approve(&self, req: &ApprovalRequest) -> Decision {
            self.hits.lock().expect("hits").push(req.clone());
            self.decision
        }
    }

    struct ProbeApprover {
        hits: Arc<Mutex<u32>>,
    }

    #[async_trait]
    impl Approver for ProbeApprover {
        async fn approve(&self, _req: &ApprovalRequest) -> Decision {
            *self.hits.lock().expect("hits") += 1;
            Decision::Once
        }
    }

    fn parent_agent(store: Arc<MemoryStore>, spawn: &Spawn, model: ScriptedModel) -> Agent {
        Agent {
            model: Arc::new(model),
            tools: vec![Arc::new(spawn.clone())],
            store,
            approver: Arc::new(AlwaysAllow),
            assembler: Arc::new(ContextAssembler::default()),
            max_turns: 4,
            system: "parent".into(),
            write_lock: spawn.write_lock.clone(),
            inbox: spawn.inbox.clone(),
        }
    }

    /// REQ-3.27 REQ-3.16 child Write/Exec approval is the parent Approver, tagged with the child id.
    #[tokio::test]
    async fn req_3_27_req_3_16_child_approval_routes_to_parent() {
        let store = Arc::new(MemoryStore::default());
        let hits = Arc::new(Mutex::new(Vec::new()));
        let rec = Arc::new(RecordingApprover {
            hits: hits.clone(),
            decision: Decision::Once,
        });
        let spawn = Spawn {
            store: store.clone(),
            model: Arc::new(ScriptedModel::new(vec![
                turn_call("write", "cw1", serde_json::json!({"p": "a"})),
                turn_text("child-wrote"),
            ])),
            tools: vec![Arc::new(FakeTool::write("write", "WROTE"))],
            approver: rec,
            assembler: Arc::new(ContextAssembler::default()),
            write_lock: CwdWriteLock::new(),
            system: "child".into(),
            max_turns: 4,
            model_name: "test-model".into(),
            inbox: Inbox::default(),
            hub: ChildHub::new(),
        };
        assert!(
            spawn
                .tools
                .iter()
                .all(|t| t.spec().name != "spawn" && t.spec().name != "wait"),
            "child must not have spawn/wait"
        );
        let parent = store.create_session("/tmp/ws", "test-model").await.unwrap();
        let agent = parent_agent(
            store.clone(),
            &spawn,
            ScriptedModel::new(vec![
                turn_call(
                    "spawn",
                    "sp1",
                    serde_json::json!({"task": "write a", "wait": true}),
                ),
                turn_text("parent-done"),
            ]),
        );
        let (tx, _rx) = mpsc::unbounded_channel();
        agent
            .run(
                &parent.meta.id,
                "hello",
                "/tmp/ws",
                CancellationToken::new(),
                tx,
            )
            .await
            .unwrap();
        let ws = store.open_session(&parent.meta.id).await.unwrap();
        let spawn_out = tool_outputs(&ws)
            .into_iter()
            .find(|o| o.contains("child_session: "))
            .expect("spawn summary");
        let child_id = spawn_out
            .lines()
            .find_map(|l| l.strip_prefix("child_session: "))
            .expect("child id")
            .to_string();
        let seen = hits.lock().expect("hits");
        assert_eq!(seen.len(), 1, "parent Approver must see the child write");
        assert_eq!(seen[0].name, "write");
        assert_eq!(seen[0].class, ToolClass::Write);
        assert!(
            seen[0]
                .args_summary
                .starts_with(&format!("child {child_id}: ")),
            "parent prompt is tagged with child session: {:?}",
            seen[0].args_summary
        );
        let child = store.open_session(&child_id).await.unwrap();
        let child_blob = format!("{:?}", child.chats);
        assert!(child_blob.contains("WROTE"), "{child_blob}");
        assert!(
            !spawn_out.contains("WROTE"),
            "child write body must not land on the parent summary"
        );
    }

    /// REQ-3.27 REQ-3.16 parent Deny is a child observation; the child run continues.
    #[tokio::test]
    async fn req_3_27_req_3_16_child_deny_is_child_observation() {
        let store = Arc::new(MemoryStore::default());
        let hits = Arc::new(Mutex::new(Vec::new()));
        let rec = Arc::new(RecordingApprover {
            hits: hits.clone(),
            decision: Decision::Deny,
        });
        let spawn = Spawn {
            store: store.clone(),
            model: Arc::new(ScriptedModel::new(vec![
                turn_call("write", "cw1", serde_json::json!({"p": "a"})),
                turn_text("child-after-deny"),
            ])),
            tools: vec![Arc::new(FakeTool::write("write", "WROTE"))],
            approver: rec,
            assembler: Arc::new(ContextAssembler::default()),
            write_lock: CwdWriteLock::new(),
            system: "child".into(),
            max_turns: 4,
            model_name: "test-model".into(),
            inbox: Inbox::default(),
            hub: ChildHub::new(),
        };
        let parent = store.create_session("/tmp/ws", "test-model").await.unwrap();
        let agent = parent_agent(
            store.clone(),
            &spawn,
            ScriptedModel::new(vec![
                turn_call(
                    "spawn",
                    "sp1",
                    serde_json::json!({"task": "write a", "wait": true}),
                ),
                turn_text("parent-done"),
            ]),
        );
        let (tx, _rx) = mpsc::unbounded_channel();
        agent
            .run(
                &parent.meta.id,
                "hello",
                "/tmp/ws",
                CancellationToken::new(),
                tx,
            )
            .await
            .unwrap();
        let ws = store.open_session(&parent.meta.id).await.unwrap();
        let spawn_out = tool_outputs(&ws)
            .into_iter()
            .find(|o| o.contains("child_session: "))
            .expect("spawn summary");
        assert!(
            spawn_out.contains("child-after-deny"),
            "child continues after deny: {spawn_out}"
        );
        assert!(
            !spawn_out.contains("WROTE"),
            "denied write must not run: {spawn_out}"
        );
        let child_id = spawn_out
            .lines()
            .find_map(|l| l.strip_prefix("child_session: "))
            .expect("child id");
        let child = store.open_session(child_id).await.unwrap();
        let denied = child
            .chats
            .iter()
            .flat_map(|c| c.items.iter())
            .any(|item| matches!(&item.kind, ItemKind::ToolResult { output, is_error: true, .. } if output == "denied"));
        assert!(
            denied,
            "deny observation stays on the child: {:?}",
            child.chats
        );
        assert!(
            ws.chats.iter().flat_map(|c| c.items.iter()).all(|item| {
                !matches!(&item.kind, ItemKind::ToolCall { name, .. } if name == "write")
            }),
            "parent transcript must not carry the child write call"
        );
        assert_eq!(hits.lock().expect("hits").len(), 1);
    }

    /// REQ-3.27 REQ-3.23 print mode refuses spawn; no child session, no approval UI.
    #[tokio::test]
    async fn req_3_27_req_3_23_print_refuses_spawn() {
        let store = Arc::new(MemoryStore::default());
        let inner_hits = Arc::new(Mutex::new(0u32));
        let listed_before = store.list_sessions(None, true).await.unwrap().len();
        let approver = Arc::new(PolicyApprover {
            policy: Policy {
                print_mode: true,
                ..Policy::default()
            },
            inner: Box::new(ProbeApprover {
                hits: inner_hits.clone(),
            }),
        });
        let spawn = Spawn {
            store: store.clone(),
            model: Arc::new(ScriptedModel::new(vec![turn_text("should-not-run")])),
            tools: vec![],
            approver: approver.clone(),
            assembler: Arc::new(ContextAssembler::default()),
            write_lock: CwdWriteLock::new(),
            system: "child".into(),
            max_turns: 4,
            model_name: "test-model".into(),
            inbox: Inbox::default(),
            hub: ChildHub::new(),
        };
        let parent = store.create_session("/tmp/ws", "test-model").await.unwrap();
        let agent = Agent {
            model: Arc::new(ScriptedModel::new(vec![
                turn_call(
                    "spawn",
                    "sp1",
                    serde_json::json!({"task": "nope", "wait": true}),
                ),
                turn_text("parent-after-print-deny"),
            ])),
            tools: vec![Arc::new(spawn)],
            store: store.clone(),
            approver,
            assembler: Arc::new(ContextAssembler::default()),
            max_turns: 4,
            system: "parent".into(),
            write_lock: CwdWriteLock::new(),
            inbox: Inbox::default(),
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let status = agent
            .run(
                &parent.meta.id,
                "hello",
                "/tmp/ws",
                CancellationToken::new(),
                tx,
            )
            .await
            .unwrap();
        assert_eq!(status, RunStatus::Done);
        let ws = store.open_session(&parent.meta.id).await.unwrap();
        let outs = tool_outputs(&ws);
        assert!(
            outs.iter().any(|o| o == "denied"),
            "print spawn observation: {outs:?}"
        );
        assert!(
            outs.iter().all(|o| !o.contains("child_session:")),
            "print must not open a child: {outs:?}"
        );
        assert_eq!(
            *inner_hits.lock().expect("hits"),
            0,
            "print denies without UI"
        );
        let listed = store.list_sessions(None, true).await.unwrap();
        assert_eq!(
            listed.len(),
            listed_before + 1,
            "only the parent session exists"
        );
    }

    /// REQ-3.27 REQ-3.16 a child in plan inherits Policy: Write is Deny without UI.
    #[tokio::test]
    async fn req_3_27_req_3_16_plan_child_write_denied_without_ui() {
        let store = Arc::new(MemoryStore::default());
        let inner_hits = Arc::new(Mutex::new(0u32));
        let policy = Policy::default();
        policy.set_plan_mode(true);
        let approver = Arc::new(PolicyApprover {
            policy,
            inner: Box::new(ProbeApprover {
                hits: inner_hits.clone(),
            }),
        });
        let spawn = Spawn {
            store: store.clone(),
            model: Arc::new(ScriptedModel::new(vec![
                turn_call("write", "cw1", serde_json::json!({"p": "a"})),
                turn_text("child-after-plan-deny"),
            ])),
            tools: vec![Arc::new(FakeTool::write("write", "WROTE"))],
            approver,
            assembler: Arc::new(ContextAssembler::default()),
            write_lock: CwdWriteLock::new(),
            system: "child".into(),
            max_turns: 4,
            model_name: "test-model".into(),
            inbox: Inbox::default(),
            hub: ChildHub::new(),
        };
        let parent = store.create_session("/tmp/ws", "test-model").await.unwrap();
        let agent = parent_agent(
            store.clone(),
            &spawn,
            ScriptedModel::new(vec![
                turn_call(
                    "spawn",
                    "sp1",
                    serde_json::json!({"task": "write a", "wait": true}),
                ),
                turn_text("parent-done"),
            ]),
        );
        let (tx, _rx) = mpsc::unbounded_channel();
        agent
            .run(
                &parent.meta.id,
                "hello",
                "/tmp/ws",
                CancellationToken::new(),
                tx,
            )
            .await
            .unwrap();
        let ws = store.open_session(&parent.meta.id).await.unwrap();
        let spawn_out = tool_outputs(&ws)
            .into_iter()
            .find(|o| o.contains("child-after-plan-deny"))
            .expect("child continued");
        let child_id = spawn_out
            .lines()
            .find_map(|l| l.strip_prefix("child_session: "))
            .expect("child id");
        let child = store.open_session(child_id).await.unwrap();
        let denied = child
            .chats
            .iter()
            .flat_map(|c| c.items.iter())
            .any(|item| matches!(&item.kind, ItemKind::ToolResult { output, is_error: true, .. } if output == "denied"));
        assert!(denied, "plan deny stays on the child");
        assert_eq!(*inner_hits.lock().expect("hits"), 0);
        let child_blob = format!("{:?}", child.chats);
        assert!(!child_blob.contains("WROTE"), "{child_blob}");
    }

    /// REQ-3.3 REQ-3.27 INV-4 parent cancel interrupts wait=true child and pairs spawn.
    #[tokio::test]
    async fn req_3_3_req_3_27_inv_4_parent_cancel_interrupts_wait_true_child() {
        let store = Arc::new(MemoryStore::default());
        let started = Arc::new(AtomicBool::new(false));
        let hold = HoldTool {
            release: Arc::new(AtomicBool::new(false)),
            kick: Arc::new(Notify::new()),
            started: started.clone(),
        };
        let spawn = make_spawn(
            store.clone(),
            ScriptedModel::new(vec![
                turn_call("hold", "ch1", serde_json::json!({})),
                turn_text("child-should-not-finish"),
            ]),
            vec![Arc::new(hold)],
        );
        let parent = store.create_session("/tmp/ws", "test-model").await.unwrap();
        let agent = Agent {
            model: Arc::new(ScriptedModel::new(vec![
                turn_call(
                    "spawn",
                    "sp1",
                    serde_json::json!({"task": "hold", "wait": true}),
                ),
                turn_text("parent-should-not-finish"),
            ])),
            tools: vec![Arc::new(spawn.clone())],
            store: store.clone(),
            approver: Arc::new(AlwaysAllow),
            assembler: Arc::new(ContextAssembler::default()),
            max_turns: 4,
            system: "parent".into(),
            write_lock: spawn.write_lock.clone(),
            inbox: spawn.inbox.clone(),
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let cancel_run = cancel.clone();
        let sid = parent.meta.id.clone();
        let run =
            tokio::spawn(async move { agent.run(&sid, "hello", "/tmp/ws", cancel, tx).await });
        wait_flag(&started).await;
        let child_id = spawn.hub.ids().await.into_iter().next().expect("child id");
        let child_tok = spawn
            .hub
            .child_cancel(&child_id)
            .await
            .expect("bound child token");
        assert!(
            !child_tok.is_cancelled(),
            "child should still be running before parent cancel"
        );
        cancel_run.cancel();
        assert!(
            child_tok.is_cancelled(),
            "parent cancel must bind and fire the child token"
        );
        let status = tokio::time::timeout(std::time::Duration::from_secs(2), run)
            .await
            .expect("wait=true cancel must finish")
            .unwrap()
            .unwrap();
        assert_eq!(status, RunStatus::Interrupted);
        let ws = store.open_session(&parent.meta.id).await.unwrap();
        assert_eq!(ws.chats[0].status, ChatStatus::Interrupted);
        assert_inv4_paired(&ws, "sp1");
        let spawn_out = tool_outputs(&ws)
            .into_iter()
            .find(|o| o.contains("child_session: ") && o.contains("status: interrupted"))
            .expect("interrupted child summary on spawn");
        let child_id = spawn_out
            .lines()
            .find_map(|l| l.strip_prefix("child_session: "))
            .expect("child id")
            .to_string();
        assert!(
            spawn.inbox.is_empty().await,
            "wait=true summary is the spawn tool_result, not Inbox"
        );
        let child = store.open_session(&child_id).await.unwrap();
        assert_eq!(child.chats[0].status, ChatStatus::Interrupted);
        assert_inv4_paired(&child, "ch1");
    }

    /// REQ-3.3 REQ-3.27 INV-4 parent cancel interrupts wait=false child; summary via Inbox.
    #[tokio::test]
    async fn req_3_3_req_3_27_inv_4_parent_cancel_interrupts_wait_false_child() {
        let store = Arc::new(MemoryStore::default());
        let child_started = Arc::new(AtomicBool::new(false));
        let parent_started = Arc::new(AtomicBool::new(false));
        let child_hold = HoldTool {
            release: Arc::new(AtomicBool::new(false)),
            kick: Arc::new(Notify::new()),
            started: child_started.clone(),
        };
        let parent_hold = HoldTool {
            release: Arc::new(AtomicBool::new(false)),
            kick: Arc::new(Notify::new()),
            started: parent_started.clone(),
        };
        let spawn = make_spawn(
            store.clone(),
            ScriptedModel::new(vec![
                turn_call("hold", "ch1", serde_json::json!({})),
                turn_text("child-should-not-finish"),
            ]),
            vec![Arc::new(child_hold)],
        );
        let parent = store.create_session("/tmp/ws", "test-model").await.unwrap();
        let agent = Agent {
            model: Arc::new(ScriptedModel::new(vec![
                turn_call(
                    "spawn",
                    "sp1",
                    serde_json::json!({"task": "hold", "wait": false}),
                ),
                turn_call("hold", "ph1", serde_json::json!({})),
                turn_text("parent-should-not-finish"),
            ])),
            tools: vec![Arc::new(spawn.clone()), Arc::new(parent_hold)],
            store: store.clone(),
            approver: Arc::new(AlwaysAllow),
            assembler: Arc::new(ContextAssembler::default()),
            max_turns: 4,
            system: "parent".into(),
            write_lock: spawn.write_lock.clone(),
            inbox: spawn.inbox.clone(),
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let cancel_run = cancel.clone();
        let sid = parent.meta.id.clone();
        let run =
            tokio::spawn(async move { agent.run(&sid, "hello", "/tmp/ws", cancel, tx).await });
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(2);
        let mut saw_parent_hold = false;
        while tokio::time::Instant::now() < deadline && !saw_parent_hold {
            match tokio::time::timeout(tokio::time::Duration::from_millis(50), rx.recv()).await {
                Ok(Some(Event::ToolStart { name, .. })) if name == "hold" => {
                    saw_parent_hold = true;
                }
                Ok(None) => break,
                Ok(Some(_)) | Err(_) => {}
            }
        }
        assert!(
            saw_parent_hold,
            "parent hold must start after wait=false spawn"
        );
        wait_flag(&child_started).await;
        cancel_run.cancel();
        let status = tokio::time::timeout(std::time::Duration::from_secs(2), run)
            .await
            .expect("wait=false cancel must finish")
            .unwrap()
            .unwrap();
        assert_eq!(status, RunStatus::Interrupted);
        let ws = store.open_session(&parent.meta.id).await.unwrap();
        assert_eq!(ws.chats[0].status, ChatStatus::Interrupted);
        assert_inv4_paired(&ws, "sp1");
        assert_inv4_paired(&ws, "ph1");
        let spawn_out = tool_outputs(&ws)
            .into_iter()
            .find(|o| o.starts_with("child_session: ") && !o.contains("status:"))
            .expect("wait=false already returned id");
        let child_id = spawn_out
            .lines()
            .find_map(|l| l.strip_prefix("child_session: "))
            .expect("child id")
            .to_string();
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(2);
        while spawn.inbox.is_empty().await {
            if tokio::time::Instant::now() > deadline {
                panic!("Inbox Subagent interrupted summary was not posted");
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
        }
        let inbox = spawn.inbox.drain().await;
        let InboxMessage::Subagent {
            call_id,
            text,
            is_error,
        } = &inbox[0]
        else {
            panic!("expected Subagent, got {inbox:?}");
        };
        assert_eq!(call_id, &child_id);
        assert!(
            is_error,
            "interrupted child summary is an error observation"
        );
        assert!(text.contains("child_session: "), "{text}");
        assert!(text.contains("status: interrupted"), "{text}");
        let child = store.open_session(&child_id).await.unwrap();
        assert_eq!(child.chats[0].status, ChatStatus::Interrupted);
        assert_inv4_paired(&child, "ch1");
    }
}
