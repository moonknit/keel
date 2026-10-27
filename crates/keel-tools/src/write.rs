use crate::pathx::resolve_write;
use async_trait::async_trait;
use keel_core::traits::Tool;
use keel_core::types::{ToolClass, ToolCx, ToolOutput, ToolSpec};
use serde_json::{json, Value};

pub struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".into(),
            description: "Write a file under the session cwd.".into(),
            parameters: json!({
                "type":"object",
                "properties": {
                    "path": {"type":"string"},
                    "contents": {"type":"string"}
                },
                "required": ["path","contents"]
            }),
        }
    }
    fn class(&self) -> ToolClass {
        ToolClass::Write
    }
    async fn call(&self, args: Value, cx: &ToolCx) -> ToolOutput {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let contents = args.get("contents").and_then(|v| v.as_str()).unwrap_or("");
        let target = match resolve_write(cx, path) {
            Ok(p) => p,
            Err(e) => return ToolOutput { text: e, is_error: true },
        };
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(&target, contents) {
            Ok(()) => ToolOutput {
                text: format!("wrote {}", target.display()),
                is_error: false,
            },
            Err(e) => ToolOutput {
                text: e.to_string(),
                is_error: true,
            },
        }
    }
}
