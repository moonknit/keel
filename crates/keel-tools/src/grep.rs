use crate::pathx::resolve;
use async_trait::async_trait;
use keel_core::traits::Tool;
use keel_core::types::{ToolClass, ToolCx, ToolOutput, ToolSpec};
use regex::Regex;
use serde_json::{json, Value};
use std::fs;

pub struct Grep;

#[async_trait]
impl Tool for Grep {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".into(),
            description: "Search file contents with a regex.".into(),
            parameters: json!({
                "type":"object",
                "properties": {
                    "pattern": {"type":"string"},
                    "path": {"type":"string"}
                },
                "required": ["pattern"]
            }),
        }
    }
    fn class(&self) -> ToolClass {
        ToolClass::Read
    }
    async fn call(&self, args: Value, cx: &ToolCx) -> ToolOutput {
        let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
        let re = match Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => return ToolOutput { text: e.to_string(), is_error: true },
        };
        let root = args
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| resolve(cx, p))
            .unwrap_or_else(|| Ok(cx.cwd.clone()));
        let root = match root {
            Ok(p) => p,
            Err(e) => return ToolOutput { text: e, is_error: true },
        };
        let mut hits = Vec::new();
        let walker = ignore::WalkBuilder::new(&root).hidden(false).build();
        for entry in walker.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Ok(text) = fs::read_to_string(path) else { continue };
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    hits.push(format!("{}:{}:{}", path.display(), i + 1, line));
                    if hits.len() >= 200 {
                        break;
                    }
                }
            }
            if hits.len() >= 200 {
                break;
            }
        }
        ToolOutput {
            text: if hits.is_empty() {
                "no matches".into()
            } else {
                hits.join("\n")
            },
            is_error: false,
        }
    }
}
