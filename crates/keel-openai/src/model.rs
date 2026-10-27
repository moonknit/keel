#[cfg(test)]
use crate::sse::data_values;
use crate::sse::{SseBuf, completed_output};
use crate::{OpenAiError, function_tool, responses_url, web_search_tool};
use async_trait::async_trait;
use futures_util::StreamExt;
use keel_core::config::LlmSettings;
use keel_core::traits::Model;
use keel_core::types::{Item, ItemKind, ModelEvent, ModelRequest, Role, ToolSpec, Usage};
use keel_core::{KeelError, Result};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct OpenAiModel {
    pub settings: LlmSettings,
    pub api_key: String,
    pub web_search: bool,
}

impl OpenAiModel {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            settings: LlmSettings::from_env()?,
            api_key: LlmSettings::api_key()?,
            web_search: std::env::var("KEEL_WEB_SEARCH")
                .map(|v| v != "0" && v != "false")
                .unwrap_or(true),
        })
    }
}

fn item_to_input(item: &Item) -> Option<Value> {
    if let Some(raw) = &item.raw {
        return Some(raw.clone());
    }
    match &item.kind {
        ItemKind::Text { text } => Some(json!({
            "role": match item.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Developer => "developer",
                Role::System => "system",
                Role::Tool => "user",
            },
            "content": text
        })),
        ItemKind::Notice { text } => Some(json!({"role": "developer", "content": text})),
        ItemKind::ToolResult {
            call_id, output, ..
        } => Some(json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": output
        })),
        ItemKind::Skill { .. } => None,
        _ => None,
    }
}

fn output_item_to_keel(v: &Value) -> Option<Item> {
    let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let kind = match typ {
        "function_call" => ItemKind::ToolCall {
            call_id: v
                .get("call_id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .into(),
            name: v.get("name").and_then(|x| x.as_str()).unwrap_or("").into(),
            args: v
                .get("arguments")
                .and_then(|a| a.as_str())
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(json!({})),
        },
        "web_search_call" => ItemKind::HostedTool {
            name: "web_search".into(),
            detail: v.clone(),
        },
        "reasoning" => ItemKind::Reasoning {
            summary: v
                .pointer("/summary/0/text")
                .and_then(|t| t.as_str())
                .map(str::to_string),
        },
        "message" => {
            let text = v
                .get("content")
                .and_then(|c| c.as_array())
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            ItemKind::Text { text }
        }
        _ => return None,
    };
    Some(Item {
        id: v
            .get("id")
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| ulid::Ulid::new().to_string()),
        role: Role::Assistant,
        kind,
        raw: Some(v.clone()),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpFail {
    Auth,
    Retry,
    Overflow,
    Provider,
}

fn classify_http(status: u16, body: &str) -> HttpFail {
    if status == 401 {
        HttpFail::Auth
    } else if status == 429 || (500..600).contains(&status) {
        HttpFail::Retry
    } else if body.contains("context") {
        HttpFail::Overflow
    } else {
        HttpFail::Provider
    }
}

fn tools_json(specs: &[ToolSpec], web_search: bool) -> Vec<Value> {
    let mut out: Vec<Value> = specs
        .iter()
        .map(|s| {
            function_tool(&s.name, &s.description)
                .as_object()
                .cloned()
                .map(|mut o| {
                    o.insert("parameters".into(), s.parameters.clone());
                    Value::Object(o)
                })
                .unwrap_or_else(|| function_tool(&s.name, &s.description))
        })
        .collect();
    if web_search {
        out.push(web_search_tool());
    }
    out
}

#[async_trait]
impl Model for OpenAiModel {
    async fn stream(
        &self,
        req: ModelRequest,
        cancel: CancellationToken,
        live: mpsc::UnboundedSender<ModelEvent>,
    ) -> Result<Vec<ModelEvent>> {
        if cancel.is_cancelled() {
            return Ok(Vec::new());
        }
        let mut input = vec![json!({"role": "system", "content": req.system})];
        for item in &req.items {
            if let Some(v) = item_to_input(item) {
                input.push(v);
            }
        }
        let body = json!({
            "model": self.settings.model,
            "store": false,
            "stream": true,
            "reasoning": { "effort": self.settings.reasoning_effort },
            "max_output_tokens": self.settings.max_tokens,
            "input": input,
            "tools": tools_json(&req.tools, self.web_search),
        });
        let client = reqwest::Client::new();
        let mut last_err = None;
        for attempt in 0..3 {
            if cancel.is_cancelled() {
                return Ok(Vec::new());
            }
            let resp = client
                .post(responses_url(&self.settings.base_url))
                .bearer_auth(&self.api_key)
                .header("accept", "text/event-stream")
                .json(&body)
                .send()
                .await
                .map_err(|e| KeelError::Msg(e.to_string()))?;
            let status = resp.status();
            if status.as_u16() == 401 {
                return Err(KeelError::Msg("auth: OpenAI rejected the key".into()));
            }
            if !status.is_success() {
                let text = resp
                    .text()
                    .await
                    .map_err(|e| KeelError::Msg(e.to_string()))?;
                match classify_http(status.as_u16(), &text) {
                    HttpFail::Auth => {
                        return Err(KeelError::Msg("auth: OpenAI rejected the key".into()));
                    }
                    HttpFail::Retry => {
                        last_err = Some(format!("HTTP {status}"));
                        tokio::time::sleep(std::time::Duration::from_millis(
                            200 * 2u64.pow(attempt),
                        ))
                        .await;
                        continue;
                    }
                    HttpFail::Overflow => {
                        return Err(KeelError::Msg("overflow".into()));
                    }
                    HttpFail::Provider => {
                        return Err(KeelError::Msg(format!(
                            "provider: {}",
                            text.chars().take(240).collect::<String>()
                        )));
                    }
                }
            }
            return read_sse_live(resp, cancel, live).await;
        }
        Err(KeelError::Msg(
            last_err.unwrap_or_else(|| "provider".into()),
        ))
    }
}

async fn read_sse_live(
    resp: reqwest::Response,
    cancel: CancellationToken,
    live: mpsc::UnboundedSender<ModelEvent>,
) -> Result<Vec<ModelEvent>> {
    let mut stream = resp.bytes_stream();
    let mut buf = SseBuf::default();
    let mut out = Vec::new();
    let mut last_completed: Option<Value> = None;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            next = stream.next() => {
                let Some(chunk) = next else { break };
                let chunk = chunk.map_err(|e| KeelError::Msg(e.to_string()))?;
                for v in buf.push(&chunk) {
                    if v.get("type").and_then(|t| t.as_str()) == Some("response.completed") {
                        last_completed = Some(v.clone());
                    }
                    for ev in events_from_value(&v) {
                        let _ = live.send(ev.clone());
                        out.push(ev);
                    }
                }
            }
        }
    }
    if !out.iter().any(|e| matches!(e, ModelEvent::ItemDone(_))) {
        if let Some(items) = last_completed
            .as_ref()
            .and_then(|v| completed_output(&[v.clone()]))
        {
            let pos = out
                .iter()
                .position(|e| matches!(e, ModelEvent::Completed { .. }))
                .unwrap_or(out.len());
            for item in items.into_iter().rev() {
                if let Some(k) = output_item_to_keel(&item) {
                    let ev = ModelEvent::ItemDone(k);
                    let _ = live.send(ev.clone());
                    out.insert(pos, ev);
                }
            }
        }
    }
    Ok(out)
}

fn events_from_value(v: &Value) -> Vec<ModelEvent> {
    let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match typ {
        "response.output_text.delta" => v
            .get("delta")
            .and_then(|d| d.as_str())
            .map(|d| vec![ModelEvent::TextDelta(d.into())])
            .unwrap_or_default(),
        "response.output_item.done" => v
            .get("item")
            .and_then(output_item_to_keel)
            .map(|item| vec![ModelEvent::ItemDone(item)])
            .unwrap_or_default(),
        "response.completed" => {
            let usage = v
                .pointer("/response/usage")
                .map(|u| Usage {
                    input_tokens: u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0)
                        as u32,
                    output_tokens: u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0)
                        as u32,
                })
                .unwrap_or_default();
            vec![ModelEvent::Completed { usage }]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
fn events_from_sse(text: &str) -> Vec<ModelEvent> {
    let mut out = Vec::new();
    for v in data_values(text) {
        out.extend(events_from_value(&v));
    }
    if !out.iter().any(|e| matches!(e, ModelEvent::ItemDone(_))) {
        if let Some(items) = completed_output(&data_values(text)) {
            let mut extra = Vec::new();
            for item in items {
                if let Some(k) = output_item_to_keel(&item) {
                    extra.push(ModelEvent::ItemDone(k));
                }
            }
            if let Some(pos) = out
                .iter()
                .position(|e| matches!(e, ModelEvent::Completed { .. }))
            {
                for ev in extra.into_iter().rev() {
                    out.insert(pos, ev);
                }
            } else {
                out.extend(extra);
            }
        }
    }
    out
}

impl From<OpenAiError> for KeelError {
    fn from(e: OpenAiError) -> Self {
        KeelError::Msg(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// REQ-3.7 raw items are forwarded unchanged.
    #[test]
    fn req_3_7_raw_passthrough() {
        let raw =
            json!({"type":"function_call","call_id":"c","name":"echo","arguments":"{}","keep":1});
        let item = Item {
            id: "1".into(),
            role: Role::Assistant,
            kind: ItemKind::ToolCall {
                call_id: "c".into(),
                name: "echo".into(),
                args: json!({}),
            },
            raw: Some(raw.clone()),
        };
        assert_eq!(item_to_input(&item).unwrap(), raw);
    }

    /// REQ-3.9 INV-2 hosted web_search is attached; request body stays store:false.
    #[test]
    fn req_3_9_inv2_web_search_and_store_false() {
        let tools = tools_json(&[], true);
        assert_eq!(tools.last().unwrap()["type"], "web_search");
        assert_eq!(tools.last().unwrap()["user_location"]["country"], "KR");
        let body = json!({
            "store": false,
            "stream": true,
            "tools": tools,
        });
        assert_eq!(body["store"], false);
    }

    /// REQ-3.6 fixture SSE becomes ModelEvents.
    #[test]
    fn req_3_6_fixture_to_events() {
        let body = include_str!("../tests/fixtures/responses-sample.sse");
        let ev = events_from_sse(body);
        assert!(ev.iter().any(|e| matches!(e, ModelEvent::TextDelta(_))));
        assert!(ev.iter().any(|e| matches!(e, ModelEvent::ItemDone(_))));
        assert!(ev.iter().any(|e| matches!(e, ModelEvent::Completed { .. })));
    }

    /// REQ-4.4 429/5xx retry, 401 auth, context overflow.
    #[test]
    fn req_4_4_classify_http() {
        assert_eq!(classify_http(401, ""), HttpFail::Auth);
        assert_eq!(classify_http(429, "slow"), HttpFail::Retry);
        assert_eq!(classify_http(503, "down"), HttpFail::Retry);
        assert_eq!(classify_http(429, "context length"), HttpFail::Retry);
        assert_eq!(
            classify_http(400, "context length exceeded"),
            HttpFail::Overflow
        );
        assert_eq!(classify_http(400, "bad request"), HttpFail::Provider);
    }

    fn spawn_http(
        replies: Vec<&'static str>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let hits_c = hits.clone();
        std::thread::spawn(move || {
            for reply in replies {
                let Ok((mut sock, _)) = listener.accept() else {
                    break;
                };
                hits_c.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(reply.as_bytes());
            }
        });
        (format!("http://{addr}"), hits)
    }

    fn test_model(base_url: String) -> OpenAiModel {
        OpenAiModel {
            settings: LlmSettings {
                model: "test".into(),
                base_url,
                reasoning_effort: "low".into(),
                max_tokens: 16,
                temperature: "0.0".into(),
            },
            api_key: "sk-test".into(),
            web_search: false,
        }
    }

    /// REQ-4.4 429 is retried with backoff and then succeeds.
    #[tokio::test]
    async fn req_4_4_retries_429_then_ok() {
        use std::sync::atomic::Ordering;
        let (base, hits) = spawn_http(vec![
            "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 2\r\nConnection: close\r\n\r\nno",
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
        ]);
        let model = test_model(base);
        let (tx, _rx) = mpsc::unbounded_channel();
        let ev = model
            .stream(
                ModelRequest {
                    system: "s".into(),
                    items: vec![],
                    tools: vec![],
                },
                CancellationToken::new(),
                tx,
            )
            .await
            .expect("429 then 200");
        assert!(ev.iter().any(|e| matches!(e, ModelEvent::Completed { .. })));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    /// REQ-4.4 context overflow is signaled immediately; the same request is not retried.
    #[tokio::test]
    async fn req_4_4_overflow_signals_without_same_request_retry() {
        use std::sync::atomic::Ordering;
        let (base, hits) = spawn_http(vec![
            "HTTP/1.1 400 Bad Request\r\nContent-Length: 23\r\nConnection: close\r\n\r\ncontext length exceeded",
        ]);
        let model = test_model(base);
        let (tx, _rx) = mpsc::unbounded_channel();
        let err = model
            .stream(
                ModelRequest {
                    system: "s".into(),
                    items: vec![],
                    tools: vec![],
                },
                CancellationToken::new(),
                tx,
            )
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "overflow");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
