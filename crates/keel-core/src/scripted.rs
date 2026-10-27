use crate::error::Result;
use crate::traits::{Model, Tool};
use crate::types::{ModelEvent, ModelRequest, ToolClass, ToolCx, ToolOutput, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub struct ScriptedModel {
    turns: Mutex<Vec<Vec<ModelEvent>>>,
}

impl ScriptedModel {
    pub fn new(turns: Vec<Vec<ModelEvent>>) -> Self {
        Self {
            turns: Mutex::new(turns),
        }
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn stream(
        &self,
        _req: ModelRequest,
        cancel: CancellationToken,
        live: mpsc::UnboundedSender<ModelEvent>,
    ) -> Result<Vec<ModelEvent>> {
        if cancel.is_cancelled() {
            return Ok(Vec::new());
        }
        let mut turns = self.turns.lock().expect("script");
        if turns.is_empty() {
            let done = vec![ModelEvent::Completed {
                usage: Default::default(),
            }];
            let _ = live.send(done[0].clone());
            return Ok(done);
        }
        let ev = turns.remove(0);
        for item in &ev {
            let _ = live.send(item.clone());
        }
        Ok(ev)
    }
}

pub struct FakeTool {
    spec: ToolSpec,
    class: ToolClass,
    output: String,
    error: bool,
}

impl FakeTool {
    pub fn new(name: &str, output: &str) -> Self {
        Self {
            spec: ToolSpec {
                name: name.into(),
                description: name.into(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            },
            class: ToolClass::Read,
            output: output.into(),
            error: false,
        }
    }

    pub fn write(name: &str, output: &str) -> Self {
        let mut t = Self::new(name, output);
        t.class = ToolClass::Write;
        t
    }
}

#[async_trait]
impl Tool for FakeTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }
    fn class(&self) -> ToolClass {
        self.class
    }
    async fn call(&self, _args: Value, _cx: &ToolCx) -> ToolOutput {
        ToolOutput {
            text: self.output.clone(),
            is_error: self.error,
        }
    }
}
