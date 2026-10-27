//! SQLite session store. rusqlite stays on a blocking mutex; callers are async.

use async_trait::async_trait;
use keel_core::traits::Store;
use keel_core::types::{
    Chat, ChatStatus, Item, ItemKind, Role, SessionMeta, Usage, WorkingSession,
};
use keel_core::{KeelError, Result};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS session (
  id TEXT PRIMARY KEY,
  title TEXT,
  cwd TEXT NOT NULL,
  model TEXT NOT NULL,
  lock_pid INTEGER,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  archived INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS chat (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  title TEXT,
  status TEXT NOT NULL CHECK (status IN ('running','done','interrupted','error')),
  input_tokens INTEGER NOT NULL DEFAULT 0,
  output_tokens INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  ended_at INTEGER,
  UNIQUE (session_id, seq)
);
CREATE TABLE IF NOT EXISTS message (
  id TEXT PRIMARY KEY,
  chat_id TEXT NOT NULL REFERENCES chat(id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  role TEXT NOT NULL,
  kind TEXT NOT NULL,
  body TEXT NOT NULL,
  raw TEXT,
  provider TEXT,
  created_at INTEGER NOT NULL,
  UNIQUE (chat_id, seq)
);
CREATE INDEX IF NOT EXISTS idx_session_cwd_updated ON session (cwd, archived, updated_at DESC);
"#;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| KeelError::Msg(e.to_string()))?;
        }
        let conn = Connection::open(path).map_err(|e| KeelError::Msg(e.to_string()))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| KeelError::Msg(e.to_string()))?;
        conn.pragma_update(None, "user_version", 1)
            .map_err(|e| KeelError::Msg(e.to_string()))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|e| KeelError::Msg(e.to_string()))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| KeelError::Msg(e.to_string()))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn load_session_locked(conn: &Connection, id: &str) -> Result<WorkingSession> {
        let meta = conn
            .query_row(
                "SELECT id, title, cwd, model, lock_pid, created_at, updated_at, archived FROM session WHERE id=?1",
                [id],
                |row| {
                    Ok(SessionMeta {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        cwd: row.get(2)?,
                        model: row.get(3)?,
                        lock_pid: row.get(4)?,
                        created_at: row.get(5)?,
                        updated_at: row.get(6)?,
                        archived: row.get::<_, i64>(7)? != 0,
                    })
                },
            )
            .map_err(|e| KeelError::Msg(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, session_id, seq, title, status, input_tokens, output_tokens, created_at, ended_at
                 FROM chat WHERE session_id=?1 ORDER BY seq",
            )
            .map_err(|e| KeelError::Msg(e.to_string()))?;
        let chat_rows = stmt
            .query_map([id], |row| {
                Ok(Chat {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    seq: row.get::<_, i64>(2)? as u32,
                    title: row.get(3)?,
                    status: parse_status(&row.get::<_, String>(4)?),
                    input_tokens: row.get::<_, i64>(5)? as u32,
                    output_tokens: row.get::<_, i64>(6)? as u32,
                    created_at: row.get(7)?,
                    ended_at: row.get(8)?,
                    items: Vec::new(),
                })
            })
            .map_err(|e| KeelError::Msg(e.to_string()))?;
        let mut chats = Vec::new();
        for row in chat_rows {
            chats.push(row.map_err(|e| KeelError::Msg(e.to_string()))?);
        }
        for chat in &mut chats {
            chat.items = load_items(conn, &chat.id)?;
        }
        let read_roots = skill_roots(&chats);
        Ok(WorkingSession {
            meta,
            chats,
            read_roots,
            allow: Vec::new(),
        })
    }
}

fn parse_status(s: &str) -> ChatStatus {
    match s {
        "done" => ChatStatus::Done,
        "interrupted" => ChatStatus::Interrupted,
        "error" => ChatStatus::Error,
        _ => ChatStatus::Running,
    }
}

fn status_str(s: ChatStatus) -> &'static str {
    match s {
        ChatStatus::Running => "running",
        ChatStatus::Done => "done",
        ChatStatus::Interrupted => "interrupted",
        ChatStatus::Error => "error",
    }
}

fn role_str(r: Role) -> &'static str {
    match r {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::System => "system",
        Role::Developer => "developer",
    }
}

fn parse_role(s: &str) -> Role {
    match s {
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        "system" => Role::System,
        "developer" => Role::Developer,
        _ => Role::User,
    }
}

fn kind_name(k: &ItemKind) -> &'static str {
    match k {
        ItemKind::Text { .. } => "text",
        ItemKind::Reasoning { .. } => "reasoning",
        ItemKind::ToolCall { .. } => "tool_call",
        ItemKind::ToolResult { .. } => "tool_result",
        ItemKind::HostedTool { .. } => "hosted_tool",
        ItemKind::Skill { .. } => "skill",
        ItemKind::Notice { .. } => "notice",
    }
}

fn load_items(conn: &Connection, chat_id: &str) -> Result<Vec<Item>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, role, kind, body, raw FROM message WHERE chat_id=?1 ORDER BY seq",
        )
        .map_err(|e| KeelError::Msg(e.to_string()))?;
    let rows = stmt
        .query_map([chat_id], |row| {
            let id: String = row.get(0)?;
            let role = parse_role(&row.get::<_, String>(1)?);
            let body: String = row.get(3)?;
            let raw: Option<String> = row.get(4)?;
            let kind: ItemKind =
                serde_json::from_str(&body).unwrap_or(ItemKind::Text { text: body });
            Ok(Item {
                id,
                role,
                kind,
                raw: raw.and_then(|s| serde_json::from_str(&s).ok()),
            })
        })
        .map_err(|e| KeelError::Msg(e.to_string()))?;
    rows.map(|r| r.map_err(|e| KeelError::Msg(e.to_string())))
        .collect()
}

fn skill_roots(chats: &[Chat]) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for chat in chats {
        for item in &chat.items {
            if let ItemKind::Skill { root, .. } = &item.kind {
                if !roots.contains(root) {
                    roots.push(root.clone());
                }
            }
        }
    }
    roots
}

fn recover(conn: &Connection, session_id: &str) -> Result<()> {
    let mut stmt = conn
        .prepare("SELECT id FROM chat WHERE session_id=?1 AND status='running'")
        .map_err(|e| KeelError::Msg(e.to_string()))?;
    let ids: Vec<String> = stmt
        .query_map([session_id], |row| row.get(0))
        .map_err(|e| KeelError::Msg(e.to_string()))?
        .filter_map(|r| r.ok())
        .collect();
    drop(stmt);
    for chat_id in ids {
        conn.execute(
            "UPDATE chat SET status='interrupted', ended_at=?1 WHERE id=?2",
            params![now_ms(), chat_id],
        )
        .map_err(|e| KeelError::Msg(e.to_string()))?;
        let items = load_items(conn, &chat_id)?;
        let mut pending = Vec::new();
        for item in &items {
            if let ItemKind::ToolCall { call_id, .. } = &item.kind {
                let has = items.iter().any(|other| {
                    matches!(&other.kind, ItemKind::ToolResult { call_id: id, .. } if id == call_id)
                });
                if !has {
                    pending.push(call_id.clone());
                }
            }
        }
        for call_id in pending {
            insert_item(conn, &chat_id, &Item::tool_result(call_id, "interrupted", true))?;
        }
    }
    Ok(())
}

fn insert_item(conn: &Connection, chat_id: &str, item: &Item) -> Result<()> {
    let seq: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(seq),0)+1 FROM message WHERE chat_id=?1",
            [chat_id],
            |row| row.get(0),
        )
        .map_err(|e| KeelError::Msg(e.to_string()))?;
    let raw = item
        .raw
        .as_ref()
        .map(|v| v.to_string());
    conn.execute(
        "INSERT INTO message (id, chat_id, seq, role, kind, body, raw, provider, created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            item.id,
            chat_id,
            seq,
            role_str(item.role),
            kind_name(&item.kind),
            serde_json::to_string(&item.kind).unwrap_or_else(|_| "{}".into()),
            raw,
            item.raw.as_ref().map(|_| "openai"),
            now_ms()
        ],
    )
    .map_err(|e| KeelError::Msg(e.to_string()))?;
    Ok(())
}

fn pid_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    #[cfg(unix)]
    {
        libc_kill(pid as i32, 0) == 0
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(unix)]
fn libc_kill(pid: i32, sig: i32) -> i32 {
    unsafe { libc::kill(pid, sig) }
}

#[cfg(unix)]
mod libc {
    unsafe extern "C" {
        pub fn kill(pid: i32, sig: i32) -> i32;
    }
}

#[async_trait]
impl Store for SqliteStore {
    async fn create_session(&self, cwd: &str, model: &str) -> Result<WorkingSession> {
        let id = ulid::Ulid::new().to_string();
        let now = now_ms();
        let pid = std::process::id() as i64;
        {
            let conn = self.conn.lock().map_err(|_| KeelError::Msg("lock".into()))?;
            conn.execute(
                "INSERT INTO session (id,title,cwd,model,lock_pid,created_at,updated_at,archived)
                 VALUES (?1,NULL,?2,?3,?4,?5,?5,0)",
                params![id, cwd, model, pid, now],
            )
            .map_err(|e| KeelError::Msg(e.to_string()))?;
        }
        self.open_session(&id).await
    }

    async fn open_session(&self, id: &str) -> Result<WorkingSession> {
        let me = std::process::id() as i64;
        let conn = self.conn.lock().map_err(|_| KeelError::Msg("lock".into()))?;
        let lock: Option<i64> = conn
            .query_row("SELECT lock_pid FROM session WHERE id=?1", [id], |row| row.get(0))
            .map_err(|e| KeelError::Msg(e.to_string()))?;
        match lock {
            Some(pid) if pid == me => {}
            Some(pid) if pid_alive(pid) => {
                return Err(KeelError::Msg(format!("locked by pid {pid}")));
            }
            _ => {
                recover(&conn, id)?;
                conn.execute(
                    "UPDATE session SET lock_pid=?1 WHERE id=?2",
                    params![me, id],
                )
                .map_err(|e| KeelError::Msg(e.to_string()))?;
            }
        }
        Self::load_session_locked(&conn, id)
    }

    async fn list_sessions(&self, cwd: Option<&str>, include_archived: bool) -> Result<Vec<SessionMeta>> {
        let conn = self.conn.lock().map_err(|_| KeelError::Msg("lock".into()))?;
        let sql = if include_archived {
            "SELECT id, title, cwd, model, lock_pid, created_at, updated_at, archived FROM session
             WHERE (?1 IS NULL OR cwd=?1) ORDER BY updated_at DESC"
        } else {
            "SELECT id, title, cwd, model, lock_pid, created_at, updated_at, archived FROM session
             WHERE archived=0 AND (?1 IS NULL OR cwd=?1) ORDER BY updated_at DESC"
        };
        let mut stmt = conn.prepare(sql).map_err(|e| KeelError::Msg(e.to_string()))?;
        let rows = stmt
            .query_map([cwd], |row| {
                Ok(SessionMeta {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    cwd: row.get(2)?,
                    model: row.get(3)?,
                    lock_pid: row.get(4)?,
                    created_at: row.get(5)?,
                    updated_at: row.get(6)?,
                    archived: row.get::<_, i64>(7)? != 0,
                })
            })
            .map_err(|e| KeelError::Msg(e.to_string()))?;
        rows.map(|r| r.map_err(|e| KeelError::Msg(e.to_string())))
            .collect()
    }

    async fn begin_chat(&self, session_id: &str, title: Option<&str>) -> Result<Chat> {
        let conn = self.conn.lock().map_err(|_| KeelError::Msg("lock".into()))?;
        let seq: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(seq),0)+1 FROM chat WHERE session_id=?1",
                [session_id],
                |row| row.get(0),
            )
            .map_err(|e| KeelError::Msg(e.to_string()))?;
        let id = ulid::Ulid::new().to_string();
        let now = now_ms();
        conn.execute(
            "INSERT INTO chat (id,session_id,seq,title,status,created_at) VALUES (?1,?2,?3,?4,'running',?5)",
            params![id, session_id, seq, title, now],
        )
        .map_err(|e| KeelError::Msg(e.to_string()))?;
        conn.execute(
            "UPDATE session SET updated_at=?1 WHERE id=?2",
            params![now, session_id],
        )
        .map_err(|e| KeelError::Msg(e.to_string()))?;
        Ok(Chat {
            id,
            session_id: session_id.into(),
            seq: seq as u32,
            title: title.map(str::to_string),
            status: ChatStatus::Running,
            input_tokens: 0,
            output_tokens: 0,
            created_at: now,
            ended_at: None,
            items: Vec::new(),
        })
    }

    async fn append(&self, chat_id: &str, item: Item) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| KeelError::Msg("lock".into()))?;
        insert_item(&conn, chat_id, &item)
    }

    async fn end_chat(&self, chat_id: &str, status: ChatStatus, usage: Usage) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| KeelError::Msg("lock".into()))?;
        conn.execute(
            "UPDATE chat SET status=?1, input_tokens=?2, output_tokens=?3, ended_at=?4 WHERE id=?5",
            params![
                status_str(status),
                usage.input_tokens as i64,
                usage.output_tokens as i64,
                now_ms(),
                chat_id
            ],
        )
        .map_err(|e| KeelError::Msg(e.to_string()))?;
        Ok(())
    }

    async fn rename(&self, session_id: &str, title: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| KeelError::Msg("lock".into()))?;
        conn.execute(
            "UPDATE session SET title=?1 WHERE id=?2",
            params![title, session_id],
        )
        .map_err(|e| KeelError::Msg(e.to_string()))?;
        Ok(())
    }

    async fn close(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| KeelError::Msg("lock".into()))?;
        conn.execute(
            "UPDATE session SET lock_pid=NULL WHERE id=?1",
            [session_id],
        )
        .map_err(|e| KeelError::Msg(e.to_string()))?;
        Ok(())
    }

    async fn archive(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| KeelError::Msg("lock".into()))?;
        conn.execute(
            "UPDATE session SET archived=1 WHERE id=?1",
            [session_id],
        )
        .map_err(|e| KeelError::Msg(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_core::Item;

    fn force_lock_pid(store: &SqliteStore, session_id: &str, pid: i64) {
        let conn = store.conn.lock().unwrap();
        conn.execute(
            "UPDATE session SET lock_pid=?1 WHERE id=?2",
            params![pid, session_id],
        )
        .unwrap();
    }

    fn me() -> i64 {
        i64::from(std::process::id())
    }

    /// REQ-3.13 INV-10 recover running chat and pair orphan tool calls.
    #[tokio::test]
    async fn req_3_13_recover_running_and_orphan_tool() {
        let store = SqliteStore::open_memory().unwrap();
        let ws = store.create_session("/tmp/ws", "m").await.unwrap();
        let chat = store.begin_chat(&ws.meta.id, Some("t")).await.unwrap();
        store
            .append(
                &chat.id,
                Item {
                    id: ulid::Ulid::new().to_string(),
                    role: Role::Assistant,
                    kind: ItemKind::ToolCall {
                        call_id: "c1".into(),
                        name: "bash".into(),
                        args: serde_json::json!({}),
                    },
                    raw: None,
                },
            )
            .await
            .unwrap();
        store.close(&ws.meta.id).await.unwrap();
        let again = store.open_session(&ws.meta.id).await.unwrap();
        assert_eq!(again.chats[0].status, ChatStatus::Interrupted);
        assert!(again.chats[0].items.iter().any(|i| {
            matches!(&i.kind, ItemKind::ToolResult { call_id, output, .. } if call_id == "c1" && output == "interrupted")
        }));
    }

    /// REQ-3.13 INV-10 same process may reopen; a dead lock_pid is reclaimed.
    #[tokio::test]
    async fn req_3_13_inv_10_same_pid_reopen_and_reclaim_dead_lock() {
        let store = SqliteStore::open_memory().unwrap();
        let ws = store.create_session("/tmp/ws", "m").await.unwrap();
        let again = store.open_session(&ws.meta.id).await.unwrap();
        assert_eq!(again.meta.lock_pid, Some(me()));

        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn exited child");
        let dead = i64::from(child.id());
        let _ = child.wait();
        assert!(!pid_alive(dead), "child must have exited");

        force_lock_pid(&store, &ws.meta.id, dead);
        let reclaimed = store.open_session(&ws.meta.id).await.unwrap();
        assert_eq!(reclaimed.meta.lock_pid, Some(me()));
    }

    /// REQ-3.13 INV-10 a living foreign lock_pid rejects open with locked + pid.
    #[cfg(unix)]
    #[tokio::test]
    async fn req_3_13_inv_10_reject_live_foreign_lock_pid() {
        let store = SqliteStore::open_memory().unwrap();
        let ws = store.create_session("/tmp/ws", "m").await.unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn live child");
        let foreign = i64::from(child.id());
        assert_ne!(foreign, me());
        assert!(pid_alive(foreign), "child must still be running");

        force_lock_pid(&store, &ws.meta.id, foreign);
        let err = store.open_session(&ws.meta.id).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("locked"), "{msg}");
        assert!(msg.contains(&foreign.to_string()), "{msg}");
        let listed = store.list_sessions(None, true).await.unwrap();
        assert_eq!(listed[0].lock_pid, Some(foreign));

        let _ = child.kill();
        let _ = child.wait();
    }

    /// REQ-5.2 session title starts empty; rename (used by /name) persists it.
    #[tokio::test]
    async fn req_5_2_rename_sets_session_title() {
        let store = SqliteStore::open_memory().unwrap();
        let ws = store.create_session("/tmp/ws", "m").await.unwrap();
        assert!(ws.meta.title.is_none());
        store.rename(&ws.meta.id, "from /name").await.unwrap();
        let loaded = store.open_session(&ws.meta.id).await.unwrap();
        assert_eq!(loaded.meta.title.as_deref(), Some("from /name"));
    }

    /// REQ-5.1 write-through then reload.
    #[tokio::test]
    async fn req_5_1_write_through_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keel.db");
        let store = SqliteStore::open(&path).unwrap();
        let ws = store.create_session("/repo", "gpt").await.unwrap();
        let chat = store.begin_chat(&ws.meta.id, Some("hi")).await.unwrap();
        store
            .append(&chat.id, Item::text(Role::User, "hello"))
            .await
            .unwrap();
        drop(store);
        let store = SqliteStore::open(&path).unwrap();
        let loaded = store.open_session(&ws.meta.id).await.unwrap();
        assert_eq!(loaded.meta.cwd, "/repo");
        assert_eq!(loaded.chats[0].items.len(), 1);
    }
}
