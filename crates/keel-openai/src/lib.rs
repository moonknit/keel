//! OpenAI Responses client. Spike helpers live here; Model impl is TASK-006.

pub mod model;
pub mod sse;

pub use model::OpenAiModel;

use serde_json::{Value, json};

#[derive(Debug, thiserror::Error)]
pub enum OpenAiError {
    #[error("{0}")]
    Msg(String),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub fn responses_url(base: &str) -> String {
    format!("{}/responses", base.trim_end_matches('/'))
}

pub fn mask_secret(value: &str) -> String {
    if value.len() <= 8 {
        return "****".into();
    }
    format!("{}…{}", &value[..4], &value[value.len() - 4..])
}

/// REQ-3.7: Value round-trip must keep unknown fields.
pub fn json_roundtrip(value: &Value) -> Result<Value, OpenAiError> {
    Ok(serde_json::from_str(&serde_json::to_string(value)?)?)
}

pub fn function_tool(name: &str, description: &str) -> Value {
    json!({
        "type": "function",
        "name": name,
        "description": description,
        "parameters": {
            "type": "object",
            "properties": {
                "text": { "type": "string" }
            },
            "required": ["text"],
            "additionalProperties": false
        }
    })
}

pub fn web_search_tool() -> Value {
    json!({
        "type": "web_search",
        "user_location": { "type": "approximate", "country": "KR" }
    })
}
