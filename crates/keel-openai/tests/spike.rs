//! Responses spike. Fixture tests always run. Live path needs KEEL_LIVE=1.

use keel_openai::sse::{completed_output, data_values, event_names};
use keel_openai::{OpenAiError, function_tool, json_roundtrip, responses_url, web_search_tool};
use rusqlite::Connection;
use serde_json::{Value, json};

fn sqlite_roundtrip(items: &[Value]) -> Result<Vec<Value>, rusqlite::Error> {
    let conn = Connection::open_in_memory()?;
    conn.execute(
        "CREATE TABLE spike (seq INTEGER PRIMARY KEY, raw TEXT NOT NULL)",
        [],
    )?;
    for (i, item) in items.iter().enumerate() {
        conn.execute(
            "INSERT INTO spike (seq, raw) VALUES (?1, ?2)",
            rusqlite::params![i as i64, item.to_string()],
        )?;
    }
    let mut stmt = conn.prepare("SELECT raw FROM spike ORDER BY seq")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(serde_json::from_str(&row?).expect("json"));
    }
    Ok(out)
}

/// REQ-3.7 unknown fields survive JSON + SQLite text storage.
#[test]
fn req_3_7_sqlite_json_keeps_unknown_fields() {
    let original = json!([{
        "type": "reasoning",
        "id": "rs_1",
        "encrypted_content": "abc",
        "extra_future_field": {"nested": true}
    }]);
    let stored = sqlite_roundtrip(original.as_array().unwrap()).expect("db");
    let again = json_roundtrip(&stored[0]).expect("rt");
    assert_eq!(stored[0]["extra_future_field"]["nested"], true);
    assert_eq!(again["encrypted_content"], "abc");
}

/// RV-005: names we will send. Live spike checks the API accepts them.
#[test]
fn rv_005_function_name_candidates() {
    for name in ["echo", "server__tool", "ok_name"] {
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        assert!(name.len() <= 64);
    }
    assert!(
        !"server.tool"
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    );
}

async fn live_stream(body: Value) -> Result<(String, Vec<Value>), OpenAiError> {
    let _ = dotenvy::dotenv();
    let key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| OpenAiError::Msg("OPENAI_API_KEY missing".into()))?;
    let model = std::env::var("LLM_MODEL").unwrap_or_else(|_| "gpt-6-luna".into());
    let base = std::env::var("LLM_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());
    let mut req = body;
    req["model"] = json!(model);
    let client = reqwest::Client::new();
    let resp = client
        .post(responses_url(&base))
        .bearer_auth(&key)
        .header("accept", "text/event-stream")
        .json(&req)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(OpenAiError::Msg(format!(
            "HTTP {status}: {}",
            text.chars().take(400).collect::<String>()
        )));
    }
    let events = data_values(&text);
    Ok((text, events))
}

/// REQ-3.6 REQ-3.7 REQ-3.9 live store:false round-trip. Ignored without KEEL_LIVE=1.
#[tokio::test]
async fn req_3_6_live_store_false_roundtrip() {
    if std::env::var("KEEL_LIVE").ok().as_deref() != Some("1") {
        eprintln!("skip live spike; set KEEL_LIVE=1");
        return;
    }
    let first = json!({
        "store": false,
        "stream": true,
        "input": [
            {"role": "user", "content": "Call echo with text=ping. Then stop. Do not search the web."}
        ],
        "tools": [function_tool("echo", "Repeat the text"), function_tool("server__tool", "MCP-style name")],
        "tool_choice": "required"
    });
    let (sse, events) = live_stream(first).await.expect("first call");
    let names = event_names(&sse);
    eprintln!("SSE events: {names:?}");
    assert!(names.iter().any(|n| n.starts_with("response.")));
    let output = completed_output(&events).expect("completed output");
    assert!(!output.is_empty(), "empty output: {sse}");
    let stored = sqlite_roundtrip(&output).expect("sqlite");
    assert_eq!(stored, output);

    let mut input = vec![
        json!({"role": "user", "content": "Call echo with text=ping. Then stop. Do not search the web."}),
    ];
    for item in &stored {
        input.push(item.clone());
        if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
            input.push(json!({
                "type": "function_call_output",
                "call_id": item.get("call_id"),
                "output": "pong"
            }));
        }
    }
    let second = json!({
        "store": false,
        "stream": true,
        "input": input,
        "tools": [function_tool("echo", "Repeat the text")]
    });
    let (sse2, events2) = live_stream(second).await.expect("replay");
    assert!(
        completed_output(&events2).is_some(),
        "replay failed: {sse2}"
    );

    let search = json!({
        "store": false,
        "stream": true,
        "input": [{"role": "user", "content": "What is the current year? Use web_search once."}],
        "tools": [web_search_tool()]
    });
    match live_stream(search).await {
        Ok((sse_s, _)) => {
            let search_names = event_names(&sse_s);
            eprintln!("web_search SSE events: {search_names:?}");
        }
        Err(e) => eprintln!("web_search live note: {e}"),
    }
}
