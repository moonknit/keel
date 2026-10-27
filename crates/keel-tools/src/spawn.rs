use keel_core::types::ToolSpec;
use serde_json::{Value, json};

/// Cap for the parent-facing child summary (assistant text only).
pub const CHILD_SUMMARY_MAX: usize = 4000;

pub const MAX_CHILDREN: usize = 4;

pub const CHILD_LIMIT_MSG: &str = "limit: already 4 children running";

pub fn spawn_spec() -> ToolSpec {
    ToolSpec {
        name: "spawn".into(),
        description: "Delegate a task to a one-level child agent. \
wait=true blocks until the child finishes and returns a summary plus the child session id. \
wait=false (default) returns the child session id immediately."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "task": {"type": "string", "description": "Task for the child agent"},
                "wait": {"type": "boolean", "description": "If true, block until the child finishes", "default": false}
            },
            "required": ["task"]
        }),
    }
}

pub fn wait_spec() -> ToolSpec {
    ToolSpec {
        name: "wait".into(),
        description: "Wait for child agents started with spawn wait=false.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "ids": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Child session ids. Omit to wait for this parent's unfinished children."
                }
            }
        }),
    }
}

pub struct SpawnArgs {
    pub task: String,
    pub wait: bool,
}

pub struct WaitArgs {
    pub ids: Option<Vec<String>>,
}

pub fn parse_wait_args(args: &Value) -> WaitArgs {
    let ids = args.get("ids").and_then(|v| v.as_array()).map(|a| {
        a.iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
    });
    WaitArgs { ids }
}

pub fn parse_spawn_args(args: &Value) -> Result<SpawnArgs, String> {
    let task = args
        .get("task")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if task.is_empty() {
        return Err("spawn requires task".into());
    }
    let wait = args.get("wait").and_then(|v| v.as_bool()).unwrap_or(false);
    Ok(SpawnArgs {
        task: task.into(),
        wait,
    })
}

pub fn clip_summary(text: &str) -> String {
    if text.len() <= CHILD_SUMMARY_MAX {
        return text.to_string();
    }
    let mut end = CHILD_SUMMARY_MAX;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

pub fn child_summary(session_id: &str, status: &str, body: &str) -> String {
    let body = clip_summary(body);
    if body.is_empty() {
        format!("child_session: {session_id}\nstatus: {status}")
    } else {
        format!("child_session: {session_id}\nstatus: {status}\n{body}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// REQ-3.27 spawn spec is task + wait; wait defaults to false when omitted.
    #[test]
    fn req_3_27_spawn_spec_and_args() {
        let spec = spawn_spec();
        assert_eq!(spec.name, "spawn");
        assert_eq!(wait_spec().name, "wait");
        let parsed = parse_spawn_args(&json!({"task":"do it"})).unwrap();
        assert_eq!(parsed.task, "do it");
        assert!(!parsed.wait);
        let waited = parse_spawn_args(&json!({"task":"do it","wait":true})).unwrap();
        assert!(waited.wait);
        assert!(parse_spawn_args(&json!({})).is_err());
        let omitted = parse_wait_args(&json!({}));
        assert!(omitted.ids.is_none());
        let listed = parse_wait_args(&json!({"ids":["a","b"]}));
        assert_eq!(listed.ids.unwrap(), ["a", "b"]);
    }
}
