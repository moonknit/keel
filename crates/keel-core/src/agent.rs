use crate::error::{KeelError, Result};
use crate::traits::{Approver, ContextSource, Model, Store, Tool};
use crate::types::{
    ApprovalRequest, ChatStatus, Decision, Event, Item, ItemKind, RunStatus, ToolClass, ToolCx,
    ToolOutput, ToolSpec, Usage,
};
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_MAX_TURNS: u32 = 50;

fn is_overflow(err: &KeelError) -> bool {
    err.to_string().contains("overflow")
}

pub struct Agent {
    pub model: Arc<dyn Model>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub store: Arc<dyn Store>,
    pub approver: Arc<dyn Approver>,
    pub assembler: Arc<dyn ContextSource>,
    pub max_turns: u32,
    pub system: String,
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
            let _ = events.send(Event::TurnStart { turn });
            session = self.store.open_session(session_id).await?;
            let model_events = match self
                .call_model(&session, cancel.clone(), &events)
                .await
            {
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

            let calls: Vec<(String, String, Value)> = new_items
                .iter()
                .filter_map(|item| match &item.kind {
                    ItemKind::ToolCall {
                        call_id,
                        name,
                        args,
                    } => Some((call_id.clone(), name.clone(), args.clone())),
                    _ => None,
                })
                .collect();
            if calls.is_empty() {
                self.store
                    .end_chat(&chat.id, ChatStatus::Done, usage_sum)
                    .await?;
                let _ = events.send(Event::RunEnd {
                    status: RunStatus::Done,
                });
                return Ok(RunStatus::Done);
            }
            for (call_id, name, args) in calls {
                if cancel.is_cancelled() {
                    let result = Item::tool_result(&call_id, "interrupted", true);
                    self.store.append(&chat.id, result).await?;
                    continue;
                }
                let Some(tool) = self.find_tool(&name) else {
                    let result = Item::tool_result(&call_id, format!("unknown tool {name}"), true);
                    self.store.append(&chat.id, result).await?;
                    continue;
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
                if !matches!(tool.class(), ToolClass::Read)
                    && self.approver.approve(&req).await == Decision::Deny
                {
                    let result = Item::tool_result(&call_id, "denied", true);
                    self.store.append(&chat.id, result).await?;
                    let _ = events.send(Event::ToolEnd {
                        name,
                        ok: false,
                        millis: 0,
                        lines: 1,
                    });
                    continue;
                }
                let started = Instant::now();
                let out: ToolOutput = tokio::select! {
                    _ = cancel.cancelled() => ToolOutput {
                        text: "interrupted".into(),
                        is_error: true,
                    },
                    out = tool.call(args.clone(), &cx) => out,
                };
                let millis = started.elapsed().as_millis();
                let lines = out.text.lines().count();
                let result = Item::tool_result(&call_id, &out.text, out.is_error);
                self.store.append(&chat.id, result).await?;
                if name == "skill" && !out.is_error {
                    if let Some(root) = out.text.lines().find_map(|l| l.strip_prefix("skill_root="))
                    {
                        let sk_name = args.get("name").and_then(|v| v.as_str()).unwrap_or("skill");
                        let skill = crate::types::Item {
                            id: ulid::Ulid::new().to_string(),
                            role: crate::types::Role::Assistant,
                            kind: ItemKind::Skill {
                                name: sk_name.into(),
                                root: root.into(),
                            },
                            raw: None,
                        };
                        self.store.append(&chat.id, skill).await?;
                    }
                }
                let _ = events.send(Event::ToolEnd {
                    name,
                    ok: !out.is_error,
                    millis,
                    lines,
                });
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

    async fn call_model(
        &self,
        session: &crate::types::WorkingSession,
        cancel: CancellationToken,
        events: &mpsc::UnboundedSender<Event>,
    ) -> Result<Vec<crate::types::ModelEvent>> {
        let mut extra_drop = 0usize;
        loop {
            let assembled = self.assembler.assemble_drop(
                session,
                self.system.clone(),
                self.specs(),
                extra_drop,
            );
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
