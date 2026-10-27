use crate::pathx::resolve;
use async_trait::async_trait;
use keel_core::traits::Tool;
use keel_core::types::{ToolClass, ToolCx, ToolOutput, ToolSpec};
use serde_json::{json, Value};

pub struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".into(),
            description: "Read a text file. Optional offset/limit are 1-based line numbers.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type":"string"},
                    "offset": {"type":"integer"},
                    "limit": {"type":"integer"}
                },
                "required": ["path"]
            }),
        }
    }
    fn class(&self) -> ToolClass {
        ToolClass::Read
    }
    async fn call(&self, args: Value, cx: &ToolCx) -> ToolOutput {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let target = match resolve(cx, path) {
            Ok(p) => p,
            Err(e) => return ToolOutput { text: e, is_error: true },
        };
        let text = match std::fs::read_to_string(&target) {
            Ok(t) => t,
            Err(e) => return ToolOutput { text: e.to_string(), is_error: true },
        };
        let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(u64::MAX) as usize;
        let start = offset.saturating_sub(1);
        let lines: Vec<String> = text
            .lines()
            .enumerate()
            .skip(start)
            .take(limit)
            .map(|(i, l)| format!("{:>6}|{}", i + 1, l))
            .collect();
        ToolOutput {
            text: lines.join("\n"),
            is_error: false,
        }
    }
}
