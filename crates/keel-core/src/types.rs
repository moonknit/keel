use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    Tool,
    System,
    Developer,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ItemKind {
    Text {
        text: String,
    },
    Reasoning {
        summary: Option<String>,
    },
    ToolCall {
        call_id: String,
        name: String,
        args: Value,
    },
    ToolResult {
        call_id: String,
        output: String,
        is_error: bool,
    },
    HostedTool {
        name: String,
        detail: Value,
    },
    Skill {
        name: String,
        root: PathBuf,
    },
    Notice {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub role: Role,
    pub kind: ItemKind,
    pub raw: Option<Value>,
}

impl Item {
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            id: ulid::Ulid::new().to_string(),
            role,
            kind: ItemKind::Text { text: text.into() },
            raw: None,
        }
    }

    pub fn notice(text: impl Into<String>) -> Self {
        Self {
            id: ulid::Ulid::new().to_string(),
            role: Role::Developer,
            kind: ItemKind::Notice { text: text.into() },
            raw: None,
        }
    }

    pub fn tool_result(
        call_id: impl Into<String>,
        output: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            id: ulid::Ulid::new().to_string(),
            role: Role::Tool,
            kind: ItemKind::ToolResult {
                call_id: call_id.into(),
                output: output.into(),
                is_error,
            },
            raw: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModelEvent {
    TextDelta(String),
    ItemDone(Item),
    Completed { usage: Usage },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass {
    Read,
    Write,
    Exec,
    External,
}

#[derive(Debug, Clone)]
pub struct ToolCx {
    pub cwd: PathBuf,
    pub read_roots: Vec<PathBuf>,
    /// Run token. Host spawn/wait bind child tokens to this (REQ-3.3).
    pub cancel: CancellationToken,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    pub name: String,
    pub class: ToolClass,
    pub args_summary: String,
    pub dangerous: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Once,
    Always,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboxMessage {
    User(String),
    Hook,
    Cron,
    Subagent {
        call_id: String,
        text: String,
        is_error: bool,
    },
    Steer,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    RunStart {
        session_id: String,
        chat_id: String,
    },
    TurnStart {
        turn: u32,
    },
    TextDelta(String),
    ToolStart {
        name: String,
        args: String,
    },
    ToolEnd {
        name: String,
        ok: bool,
        millis: u128,
        lines: usize,
    },
    HostedTool {
        name: String,
    },
    ContextTrimmed {
        dropped_chats: usize,
    },
    Usage(Usage),
    RunEnd {
        status: RunStatus,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Done,
    Interrupted,
    Error,
    Limit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatStatus {
    Running,
    Done,
    Interrupted,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: Option<String>,
    pub cwd: String,
    pub model: String,
    pub lock_pid: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub archived: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chat {
    pub id: String,
    pub session_id: String,
    pub seq: u32,
    pub title: Option<String>,
    pub status: ChatStatus,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub created_at: i64,
    pub ended_at: Option<i64>,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkingSession {
    pub meta: SessionMeta,
    pub chats: Vec<Chat>,
    pub read_roots: Vec<PathBuf>,
    pub allow: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub system: String,
    pub items: Vec<Item>,
    pub tools: Vec<ToolSpec>,
}

#[derive(Debug, Clone)]
pub struct Assembled {
    pub request: ModelRequest,
    pub dropped_chats: usize,
}
