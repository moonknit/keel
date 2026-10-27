use serde_json::Value;

#[derive(Default)]
pub struct SseBuf {
    bytes: Vec<u8>,
}

impl SseBuf {
    /// Push a chunk. Complete `data:` lines become JSON values.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Value> {
        self.bytes.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.bytes.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.bytes.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\r', '\n']);
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            if data == "[DONE]" {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                out.push(v);
            }
        }
        out
    }
}

/// Collect unique `event:` names from an SSE body. REQ-3.6 / RV-004.
pub fn event_names(body: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in body.lines() {
        let Some(name) = line.strip_prefix("event: ") else {
            continue;
        };
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    names
}

pub fn data_values(body: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(data) {
            out.push(v);
        }
    }
    out
}

/// Pull completed response `output` array if present.
pub fn completed_output(events: &[Value]) -> Option<Vec<Value>> {
    events.iter().rev().find_map(|v| {
        if v.get("type").and_then(|t| t.as_str()) == Some("response.completed") {
            v.pointer("/response/output")
                .and_then(|o| o.as_array())
                .cloned()
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// REQ-3.6 parses recorded Responses SSE event names.
    #[test]
    fn req_3_6_parses_fixture_event_names() {
        let body = include_str!("../tests/fixtures/responses-sample.sse");
        let names = event_names(body);
        assert!(names.contains(&"response.output_text.delta".into()));
        assert!(names.contains(&"response.output_item.done".into()));
        assert!(names.contains(&"response.completed".into()));
        let output = completed_output(&data_values(body)).expect("output");
        assert!(!output.is_empty());
    }

    /// REQ-3.6 incremental chunks yield the same data values as a full body.
    #[test]
    fn req_3_6_incremental_chunks() {
        let body = include_str!("../tests/fixtures/responses-sample.sse");
        let full = data_values(body);
        let mut buf = SseBuf::default();
        let mut got = Vec::new();
        for chunk in body.as_bytes().chunks(17) {
            got.extend(buf.push(chunk));
        }
        assert_eq!(got, full);
        assert!(
            got.iter()
                .any(|v| v.get("type").and_then(|t| t.as_str())
                    == Some("response.output_text.delta"))
        );
    }
}
