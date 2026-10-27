use async_trait::async_trait;
use globset::{Glob as G, GlobSetBuilder};
use keel_core::traits::Tool;
use keel_core::types::{ToolClass, ToolCx, ToolOutput, ToolSpec};
use serde_json::{Value, json};

pub struct Glob;

#[async_trait]
impl Tool for Glob {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".into(),
            description: "Find paths matching a glob from cwd.".into(),
            parameters: json!({
                "type":"object",
                "properties": { "pattern": {"type":"string"} },
                "required": ["pattern"]
            }),
        }
    }
    fn class(&self) -> ToolClass {
        ToolClass::Read
    }
    async fn call(&self, args: Value, cx: &ToolCx) -> ToolOutput {
        let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("*");
        let glob = match G::new(pattern) {
            Ok(g) => g,
            Err(e) => {
                return ToolOutput {
                    text: e.to_string(),
                    is_error: true,
                };
            }
        };
        let set = GlobSetBuilder::new().add(glob).build().unwrap();
        let mut hits = Vec::new();
        for entry in ignore::WalkBuilder::new(&cx.cwd).build().flatten() {
            let rel = entry.path().strip_prefix(&cx.cwd).unwrap_or(entry.path());
            if set.is_match(rel) {
                hits.push(rel.display().to_string());
                if hits.len() >= 200 {
                    break;
                }
            }
        }
        ToolOutput {
            text: hits.join("\n"),
            is_error: false,
        }
    }
}
