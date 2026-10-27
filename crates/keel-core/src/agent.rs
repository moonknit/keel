use crate::error::{KeelError, Result};
use crate::traits::{Approver, ContextSource, Hooks, Model, Store, Tool};
use crate::types::{
    ApprovalRequest, ChatStatus, Decision, Event, InboxMessage, Item, ItemKind, RunStatus,
    ToolClass, ToolCx, ToolOutput, ToolSpec, Usage,
};
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, MutexGuard, mpsc};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_MAX_TURNS: u32 = 50;

/// Serializes same-cwd `write`/`edit`/`bash` across parent and child agents.
/// The child launcher clones this onto the child `Agent`.
#[derive(Clone, Default)]
pub struct CwdWriteLock {
    inner: Arc<Mutex<()>>,
}

pub struct CwdWriteGuard<'a> {
    _guard: MutexGuard<'a, ()>,
}

impl CwdWriteLock {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn acquire(&self) -> CwdWriteGuard<'_> {
        CwdWriteGuard {
            _guard: self.inner.lock().await,
        }
    }
}

fn is_overflow(err: &KeelError) -> bool {
    err.to_string().contains("overflow")
}

/// Host-filled queue. The loop drains `User` (reserved) and `Subagent` (REQ-3.4).
#[derive(Clone, Default)]
pub struct Inbox {
    inner: Arc<tokio::sync::Mutex<Vec<InboxMessage>>>,
}

impl Inbox {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn push(&self, msg: InboxMessage) {
        self.inner.lock().await.push(msg);
    }

    pub async fn drain(&self) -> Vec<InboxMessage> {
        std::mem::take(&mut *self.inner.lock().await)
    }

    pub async fn is_empty(&self) -> bool {
        self.inner.lock().await.is_empty()
    }
}

pub struct Agent {
    pub model: Arc<dyn Model>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub store: Arc<dyn Store>,
    pub approver: Arc<dyn Approver>,
    pub assembler: Arc<dyn ContextSource>,
    pub max_turns: u32,
    pub system: String,
    pub write_lock: CwdWriteLock,
    pub inbox: Inbox,
    pub hooks: Option<Arc<dyn Hooks>>,
}

impl Agent {
    fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    fn find_tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.spec().name == name).cloned()
    }

    pub async fn run(
        &self,
        session_id: &str,
        user_text: &str,
        cwd: &str,
        cancel: CancellationToken,
        events: mpsc::UnboundedSender<Event>,
    ) -> Result<RunStatus> {
        let mut session = self.store.open_session(session_id).await?;
        let title = user_text.lines().next().unwrap_or(user_text);
        let chat = self.store.begin_chat(session_id, Some(title)).await?;
        let user = Item::text(crate::types::Role::User, user_text);
        self.store.append(&chat.id, user.clone()).await?;
        session.chats.push({
            let mut c = chat.clone();
            c.items.push(user);
            c
        });
        if session.meta.title.is_none() {
            let _ = self.store.rename(session_id, title).await;
        }
        let _ = events.send(Event::RunStart {
            session_id: session_id.into(),
            chat_id: chat.id.clone(),
        });

        let mut usage_sum = Usage::default();
        let cx = ToolCx {
            cwd: cwd.into(),
            read_roots: session.read_roots.clone(),
            cancel: cancel.clone(),
        };

        for turn in 1..=self.max_turns {
            if cancel.is_cancelled() {
                self.finish_interrupted(&chat.id, session_id, usage_sum)
                    .await?;
                let _ = events.send(Event::RunEnd {
                    status: RunStatus::Interrupted,
                });
                return Ok(RunStatus::Interrupted);
            }
            self.drain_inbox(&chat.id).await?;
            let _ = events.send(Event::TurnStart { turn });
            session = self.store.open_session(session_id).await?;
            let model_events = match self.call_model(&session, cancel.clone(), &events).await {
                Ok(ev) => ev,
                Err(e) if is_overflow(&e) => {
                    self.store
                        .end_chat(&chat.id, ChatStatus::Error, usage_sum)
                        .await?;
                    let _ = events.send(Event::RunEnd {
                        status: RunStatus::Error,
                    });
                    return Err(e);
                }
                Err(e) => return Err(e),
            };
            let mut new_items = Vec::new();
            let mut usage = Usage::default();
            for ev in model_events {
                match ev {
                    crate::types::ModelEvent::TextDelta(_) => {}
                    crate::types::ModelEvent::ItemDone(item) => {
                        if let ItemKind::HostedTool { name, .. } = &item.kind {
                            let _ = events.send(Event::HostedTool { name: name.clone() });
                        }
                        new_items.push(item);
                    }
                    crate::types::ModelEvent::Completed { usage: u } => usage = u,
                }
            }
            for item in &new_items {
                self.store.append(&chat.id, item.clone()).await?;
                if let Some(last) = session.chats.last_mut() {
                    last.items.push(item.clone());
                }
            }
            usage_sum.input_tokens += usage.input_tokens;
            usage_sum.output_tokens += usage.output_tokens;
            let _ = events.send(Event::Usage(usage));

            if cancel.is_cancelled() {
                self.finish_interrupted(&chat.id, session_id, usage_sum)
                    .await?;
                let _ = events.send(Event::RunEnd {
                    status: RunStatus::Interrupted,
                });
                return Ok(RunStatus::Interrupted);
            }

            let pending: Vec<PendingCall> = new_items
                .iter()
                .filter_map(|item| match &item.kind {
                    ItemKind::ToolCall {
                        call_id,
                        name,
                        args,
                    } => Some(PendingCall {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        args: args.clone(),
                        tool: self.find_tool(name),
                    }),
                    _ => None,
                })
                .collect();
            if pending.is_empty() {
                self.store
                    .end_chat(&chat.id, ChatStatus::Done, usage_sum)
                    .await?;
                let _ = events.send(Event::RunEnd {
                    status: RunStatus::Done,
                });
                return Ok(RunStatus::Done);
            }
            let (reads, serials): (Vec<_>, Vec<_>) =
                pending.into_iter().partition(|c| is_read_tool(&c.tool));
            run_read_calls(
                reads,
                self.store.clone(),
                self.approver.clone(),
                self.hooks.clone(),
                cx.clone(),
                chat.id.clone(),
                cancel.clone(),
                events.clone(),
                self.write_lock.clone(),
            )
            .await?;
            for call in serials {
                run_tool_call(
                    self.store.clone(),
                    self.approver.clone(),
                    self.hooks.clone(),
                    call,
                    cx.clone(),
                    chat.id.clone(),
                    cancel.clone(),
                    events.clone(),
                    self.write_lock.clone(),
                )
                .await?;
            }
            if self.hooks.as_ref().is_some_and(|h| h.should_stop()) {
                self.store
                    .end_chat(&chat.id, ChatStatus::Done, usage_sum)
                    .await?;
                let _ = events.send(Event::RunEnd {
                    status: RunStatus::Done,
                });
                return Ok(RunStatus::Done);
            }
            if cancel.is_cancelled() {
                self.finish_interrupted(&chat.id, session_id, usage_sum)
                    .await?;
                let _ = events.send(Event::RunEnd {
                    status: RunStatus::Interrupted,
                });
                return Ok(RunStatus::Interrupted);
            }
        }
        self.store
            .end_chat(&chat.id, ChatStatus::Done, usage_sum)
            .await?;
        let _ = events.send(Event::RunEnd {
            status: RunStatus::Limit,
        });
        Ok(RunStatus::Limit)
    }

    async fn drain_inbox(&self, chat_id: &str) -> Result<()> {
        for msg in self.inbox.drain().await {
            match msg {
                InboxMessage::Subagent {
                    call_id,
                    text,
                    is_error,
                } => {
                    self.store
                        .append(chat_id, Item::tool_result(call_id, text, is_error))
                        .await?;
                }
                InboxMessage::User(_)
                | InboxMessage::Hook
                | InboxMessage::Cron
                | InboxMessage::Steer => {}
            }
        }
        Ok(())
    }

    async fn call_model(
        &self,
        session: &crate::types::WorkingSession,
        cancel: CancellationToken,
        events: &mpsc::UnboundedSender<Event>,
    ) -> Result<Vec<crate::types::ModelEvent>> {
        let mut extra_drop = 0usize;
        loop {
            let mut assembled = self.assembler.assemble_drop(
                session,
                self.system.clone(),
                self.specs(),
                extra_drop,
            );
            if let Some(h) = &self.hooks {
                h.prepare_next_turn(&mut assembled);
            }
            if assembled.dropped_chats > 0 {
                let _ = events.send(Event::ContextTrimmed {
                    dropped_chats: assembled.dropped_chats,
                });
            }
            let (live_tx, mut live_rx) = mpsc::unbounded_channel();
            let events_live = events.clone();
            let pump = tokio::spawn(async move {
                while let Some(ev) = live_rx.recv().await {
                    if let crate::types::ModelEvent::TextDelta(s) = ev {
                        let _ = events_live.send(Event::TextDelta(s));
                    }
                }
            });
            let result = self
                .model
                .stream(assembled.request, cancel.clone(), live_tx)
                .await;
            let _ = pump.await;
            match result {
                Ok(ev) => return Ok(ev),
                Err(e) if extra_drop == 0 && is_overflow(&e) => extra_drop = 1,
                Err(e) => return Err(e),
            }
        }
    }

    async fn finish_interrupted(
        &self,
        chat_id: &str,
        session_id: &str,
        usage: Usage,
    ) -> Result<()> {
        let session = self.store.open_session(session_id).await?;
        if let Some(chat) = session.chats.iter().rev().find(|c| c.id == chat_id) {
            for item in &chat.items {
                if let ItemKind::ToolCall { call_id, .. } = &item.kind {
                    let has = chat.items.iter().any(|other| {
                        matches!(&other.kind, ItemKind::ToolResult { call_id: id, .. } if id == call_id)
                    });
                    if !has {
                        self.store
                            .append(chat_id, Item::tool_result(call_id, "interrupted", true))
                            .await?;
                    }
                }
            }
        }
        self.store
            .end_chat(chat_id, ChatStatus::Interrupted, usage)
            .await
            .map_err(|e| KeelError::Msg(e.to_string()))
    }
}

struct PendingCall {
    call_id: String,
    name: String,
    args: Value,
    tool: Option<Arc<dyn Tool>>,
}

fn is_read_tool(tool: &Option<Arc<dyn Tool>>) -> bool {
    tool.as_ref().is_some_and(|t| t.class() == ToolClass::Read)
}

fn needs_cwd_write_lock(class: ToolClass) -> bool {
    matches!(class, ToolClass::Write | ToolClass::Exec)
}

/// spawn/wait honor the run token and must return the paired summary (REQ-3.3).
fn waits_for_cancel_result(name: &str) -> bool {
    name == "spawn" || name == "wait"
}

async fn run_read_calls(
    mut reads: Vec<PendingCall>,
    store: Arc<dyn Store>,
    approver: Arc<dyn Approver>,
    hooks: Option<Arc<dyn Hooks>>,
    cx: ToolCx,
    chat_id: String,
    cancel: CancellationToken,
    events: mpsc::UnboundedSender<Event>,
    write_lock: CwdWriteLock,
) -> Result<()> {
    match reads.len() {
        0 => Ok(()),
        1 => {
            run_tool_call(
                store,
                approver,
                hooks,
                reads.pop().expect("one read"),
                cx,
                chat_id,
                cancel,
                events,
                write_lock,
            )
            .await
        }
        _ => {
            let mut set = JoinSet::new();
            for call in reads {
                let store = store.clone();
                let approver = approver.clone();
                let hooks = hooks.clone();
                let cx = cx.clone();
                let chat_id = chat_id.clone();
                let cancel = cancel.clone();
                let events = events.clone();
                let write_lock = write_lock.clone();
                set.spawn(async move {
                    run_tool_call(
                        store, approver, hooks, call, cx, chat_id, cancel, events, write_lock,
                    )
                    .await
                });
            }
            while let Some(joined) = set.join_next().await {
                joined.map_err(|e| KeelError::Msg(e.to_string()))??;
            }
            Ok(())
        }
    }
}

async fn run_tool_call(
    store: Arc<dyn Store>,
    approver: Arc<dyn Approver>,
    hooks: Option<Arc<dyn Hooks>>,
    call: PendingCall,
    cx: ToolCx,
    chat_id: String,
    cancel: CancellationToken,
    events: mpsc::UnboundedSender<Event>,
    write_lock: CwdWriteLock,
) -> Result<()> {
    let PendingCall {
        call_id,
        name,
        args,
        tool,
    } = call;
    if cancel.is_cancelled() {
        store
            .append(&chat_id, Item::tool_result(&call_id, "interrupted", true))
            .await?;
        return Ok(());
    }
    let Some(tool) = tool else {
        store
            .append(
                &chat_id,
                Item::tool_result(&call_id, format!("unknown tool {name}"), true),
            )
            .await?;
        return Ok(());
    };
    let summary = args.to_string();
    let _ = events.send(Event::ToolStart {
        name: name.clone(),
        args: summary.clone(),
    });
    let req = ApprovalRequest {
        name: name.clone(),
        class: tool.class(),
        args_summary: summary,
        dangerous: false,
    };
    let decision = match &hooks {
        Some(h) => h.before_tool(&req).await,
        None if matches!(tool.class(), ToolClass::Read) => Decision::Once,
        None => approver.approve(&req).await,
    };
    if !matches!(tool.class(), ToolClass::Read) && decision == Decision::Deny {
        store
            .append(&chat_id, Item::tool_result(&call_id, "denied", true))
            .await?;
        let _ = events.send(Event::ToolEnd {
            name,
            ok: false,
            millis: 0,
            lines: 1,
        });
        return Ok(());
    }
    let started = Instant::now();
    let work = async {
        let _guard = if needs_cwd_write_lock(tool.class()) {
            Some(write_lock.acquire().await)
        } else {
            None
        };
        tool.call(args.clone(), &cx).await
    };
    tokio::pin!(work);
    let out: ToolOutput = tokio::select! {
        out = &mut work => out,
        _ = cancel.cancelled() => {
            if waits_for_cancel_result(&name) {
                work.await
            } else {
                ToolOutput {
                    text: "interrupted".into(),
                    is_error: true,
                }
            }
        }
    };
    let millis = started.elapsed().as_millis();
    let lines = out.text.lines().count();
    store
        .append(
            &chat_id,
            Item::tool_result(&call_id, &out.text, out.is_error),
        )
        .await?;
    if let Some(h) = &hooks {
        h.after_tool(&name, &out).await;
    }
    if name == "skill" && !out.is_error {
        if let Some(root) = out.text.lines().find_map(|l| l.strip_prefix("skill_root=")) {
            let sk_name = args.get("name").and_then(|v| v.as_str()).unwrap_or("skill");
            store
                .append(
                    &chat_id,
                    crate::types::Item {
                        id: ulid::Ulid::new().to_string(),
                        role: crate::types::Role::Assistant,
                        kind: ItemKind::Skill {
                            name: sk_name.into(),
                            root: root.into(),
                        },
                        raw: None,
                    },
                )
                .await?;
        }
    }
    let _ = events.send(Event::ToolEnd {
        name,
        ok: !out.is_error,
        millis,
        lines,
    });
    Ok(())
}
