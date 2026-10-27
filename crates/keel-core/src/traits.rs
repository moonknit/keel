use crate::error::Result;
use crate::types::{
    ApprovalRequest, Assembled, Chat, Decision, Item, ModelEvent, ModelRequest, SessionMeta,
    ToolClass, ToolCx, ToolOutput, ToolSpec, WorkingSession,
};
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[async_trait]
pub trait Model: Send + Sync {
    async fn stream(
        &self,
        req: ModelRequest,
        cancel: CancellationToken,
        live: mpsc::UnboundedSender<ModelEvent>,
    ) -> Result<Vec<ModelEvent>>;
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn class(&self) -> ToolClass;
    async fn call(&self, args: Value, cx: &ToolCx) -> ToolOutput;
}

#[async_trait]
pub trait Approver: Send + Sync {
    async fn approve(&self, req: &ApprovalRequest) -> Decision;
}

pub struct AlwaysAllow;

#[async_trait]
impl Approver for AlwaysAllow {
    async fn approve(&self, _req: &ApprovalRequest) -> Decision {
        Decision::Once
    }
}

pub struct AlwaysDeny;

#[async_trait]
impl Approver for AlwaysDeny {
    async fn approve(&self, _req: &ApprovalRequest) -> Decision {
        Decision::Deny
    }
}

#[async_trait]
pub trait Store: Send + Sync {
    async fn create_session(&self, cwd: &str, model: &str) -> Result<WorkingSession>;
    async fn open_session(&self, id: &str) -> Result<WorkingSession>;
    async fn list_sessions(
        &self,
        cwd: Option<&str>,
        include_archived: bool,
    ) -> Result<Vec<SessionMeta>>;
    async fn begin_chat(&self, session_id: &str, title: Option<&str>) -> Result<Chat>;
    async fn append(&self, chat_id: &str, item: Item) -> Result<()>;
    async fn end_chat(
        &self,
        chat_id: &str,
        status: crate::types::ChatStatus,
        usage: crate::types::Usage,
    ) -> Result<()>;
    async fn rename(&self, session_id: &str, title: &str) -> Result<()>;
    async fn close(&self, session_id: &str) -> Result<()>;
    async fn archive(&self, session_id: &str) -> Result<()>;
}

pub trait ContextSource: Send + Sync {
    fn assemble(
        &self,
        session: &WorkingSession,
        system: String,
        tools: Vec<ToolSpec>,
    ) -> Assembled {
        self.assemble_drop(session, system, tools, 0)
    }

    /// `extra_drop` drops that many additional finished chats after the budget pass.
    /// The running chat is never dropped (INV-5).
    fn assemble_drop(
        &self,
        session: &WorkingSession,
        system: String,
        tools: Vec<ToolSpec>,
        extra_drop: usize,
    ) -> Assembled;
}
