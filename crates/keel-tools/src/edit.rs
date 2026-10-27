use crate::pathx::resolve_write;
use async_trait::async_trait;
use keel_core::traits::Tool;
use keel_core::types::{ToolClass, ToolCx, ToolOutput, ToolSpec};
use serde_json::{json, Value};

pub struct Edit;

#[async_trait]
impl Tool for Edit {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".into(),
            description: "Replace old with new. old must match exactly once unless replace_all.".into(),
            parameters: json!({
                "type":"object",
                "properties": {
                    "path": {"type":"string"},
                    "old": {"type":"string"},
                    "new": {"type":"string"},
                    "replace_all": {"type":"boolean"}
                },
                "required": ["path","old","new"]
            }),
        }
    }
    fn class(&self) -> ToolClass {
        ToolClass::Write
    }
    async fn call(&self, args: Value, cx: &ToolCx) -> ToolOutput {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let old = args.get("old").and_then(|v| v.as_str()).unwrap_or("");
        let new = args.get("new").and_then(|v| v.as_str()).unwrap_or("");
        let replace_all = args.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);
        let target = match resolve_write(cx, path) {
            Ok(p) => p,
            Err(e) => return ToolOutput { text: e, is_error: true },
        };
        let text = match std::fs::read_to_string(&target) {
            Ok(t) => t,
            Err(e) => return ToolOutput { text: e.to_string(), is_error: true },
        };
        let count = text.matches(old).count();
        if old.is_empty() || count == 0 || (!replace_all && count != 1) {
            return ToolOutput {
                text: format!("edit: expected 1 match, found {count}"),
                is_error: true,
            };
        }
        let next = if replace_all {
            text.replace(old, new)
        } else {
            text.replacen(old, new, 1)
        };
        match std::fs::write(&target, next) {
            Ok(()) => ToolOutput {
                text: format!("edited {}", target.display()),
                is_error: false,
            },
            Err(e) => ToolOutput {
                text: e.to_string(),
                is_error: true,
            },
        }
    }
}
