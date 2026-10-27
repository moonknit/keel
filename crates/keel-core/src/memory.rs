use crate::error::{KeelError, Result};
use crate::traits::Store;
use crate::types::{Chat, ChatStatus, Item, ItemKind, SessionMeta, Usage, WorkingSession};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<String, WorkingSession>,
}

fn recover_chat(chat: &mut Chat) {
    if chat.status == ChatStatus::Running {
        chat.status = ChatStatus::Interrupted;
        chat.ended_at = Some(now_ms());
    }
    let mut pending = Vec::new();
    for item in &chat.items {
        if let ItemKind::ToolCall { call_id, .. } = &item.kind {
            let has = chat.items.iter().any(|other| {
                matches!(&other.kind, ItemKind::ToolResult { call_id: id, .. } if id == call_id)
            });
            if !has {
                pending.push(call_id.clone());
            }
        }
    }
    for call_id in pending {
        chat.items
            .push(Item::tool_result(call_id, "interrupted", true));
    }
}

#[async_trait]
impl Store for MemoryStore {
    async fn create_session(&self, cwd: &str, model: &str) -> Result<WorkingSession> {
        let now = now_ms();
        let ws = WorkingSession {
            meta: SessionMeta {
                id: ulid::Ulid::new().to_string(),
                title: None,
                cwd: cwd.into(),
                model: model.into(),
                lock_pid: Some(std::process::id() as i64),
                created_at: now,
                updated_at: now,
                archived: false,
            },
            chats: Vec::new(),
            read_roots: Vec::new(),
            allow: Vec::new(),
        };
        self.inner
            .lock()
            .map_err(|_| KeelError::Msg("lock".into()))?
            .sessions
            .insert(ws.meta.id.clone(), ws.clone());
        Ok(ws)
    }

    async fn open_session(&self, id: &str) -> Result<WorkingSession> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| KeelError::Msg("lock".into()))?;
        let ws = inner
            .sessions
            .get_mut(id)
            .ok_or_else(|| KeelError::Msg(format!("session {id} not found")))?;
        let me = std::process::id() as i64;
        match ws.meta.lock_pid {
            Some(pid) if pid == me => return Ok(ws.clone()),
            Some(pid) if pid_alive(pid) => {
                return Err(KeelError::Msg(format!("locked by pid {pid}")));
            }
            _ => {}
        }
        for chat in &mut ws.chats {
            recover_chat(chat);
        }
        ws.meta.lock_pid = Some(me);
        ws.read_roots.clear();
        for chat in &ws.chats {
            for item in &chat.items {
                if let ItemKind::Skill { root, .. } = &item.kind {
                    if !ws.read_roots.contains(root) {
                        ws.read_roots.push(root.clone());
                    }
                }
            }
        }
        Ok(ws.clone())
    }

    async fn list_sessions(
        &self,
        cwd: Option<&str>,
        include_archived: bool,
    ) -> Result<Vec<SessionMeta>> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| KeelError::Msg("lock".into()))?;
        let mut out: Vec<_> = inner
            .sessions
            .values()
            .filter(|s| include_archived || !s.meta.archived)
            .filter(|s| cwd.is_none_or(|c| s.meta.cwd == c))
            .map(|s| s.meta.clone())
            .collect();
        out.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        Ok(out)
    }

    async fn begin_chat(&self, session_id: &str, title: Option<&str>) -> Result<Chat> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| KeelError::Msg("lock".into()))?;
        let ws = inner
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| KeelError::Msg("session not found".into()))?;
        let seq = ws.chats.len() as u32 + 1;
        let chat = Chat {
            id: ulid::Ulid::new().to_string(),
            session_id: session_id.into(),
            seq,
            title: title.map(str::to_string),
            status: ChatStatus::Running,
            input_tokens: 0,
            output_tokens: 0,
            created_at: now_ms(),
            ended_at: None,
            items: Vec::new(),
        };
        ws.chats.push(chat.clone());
        ws.meta.updated_at = now_ms();
        Ok(chat)
    }

    async fn append(&self, chat_id: &str, item: Item) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| KeelError::Msg("lock".into()))?;
        for ws in inner.sessions.values_mut() {
            if let Some(chat) = ws.chats.iter_mut().find(|c| c.id == chat_id) {
                chat.items.push(item);
                ws.meta.updated_at = now_ms();
                return Ok(());
            }
        }
        Err(KeelError::Msg("chat not found".into()))
    }

    async fn end_chat(&self, chat_id: &str, status: ChatStatus, usage: Usage) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| KeelError::Msg("lock".into()))?;
        for ws in inner.sessions.values_mut() {
            if let Some(chat) = ws.chats.iter_mut().find(|c| c.id == chat_id) {
                chat.status = status;
                chat.input_tokens = usage.input_tokens;
                chat.output_tokens = usage.output_tokens;
                chat.ended_at = Some(now_ms());
                return Ok(());
            }
        }
        Err(KeelError::Msg("chat not found".into()))
    }

    async fn rename(&self, session_id: &str, title: &str) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| KeelError::Msg("lock".into()))?;
        let ws = inner
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| KeelError::Msg("session not found".into()))?;
        ws.meta.title = Some(title.into());
        Ok(())
    }

    async fn close(&self, session_id: &str) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| KeelError::Msg("lock".into()))?;
        if let Some(ws) = inner.sessions.get_mut(session_id) {
            ws.meta.lock_pid = None;
        }
        Ok(())
    }

    async fn archive(&self, session_id: &str) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| KeelError::Msg("lock".into()))?;
        let ws = inner
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| KeelError::Msg("session not found".into()))?;
        ws.meta.archived = true;
        Ok(())
    }
}

fn pid_alive(_pid: i64) -> bool {
    false
}

impl MemoryStore {
    pub fn force_lock(&self, session_id: &str, pid: i64) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(ws) = inner.sessions.get_mut(session_id) {
                ws.meta.lock_pid = Some(pid);
            }
        }
    }
}
