//! MCP client via `rmcp`. One server's failure does not stop the loop.

use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use keel_core::traits::Tool;
use keel_core::types::{ToolClass, ToolCx, ToolOutput, ToolSpec};
use rmcp::model::{CallToolRequestParams, CallToolResult, ProtocolVersion};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{ClientLifecycleMode, ClientServiceExt, RoleClient};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

#[derive(Clone)]
pub enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

impl fmt::Debug for McpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio { command, args } => f
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", args)
                .finish(),
            Self::Http { url, headers } => f
                .debug_struct("Http")
                .field("url", url)
                .field("headers", &headers.keys().collect::<Vec<_>>())
                .finish(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
}

type Client = RunningService<RoleClient, ()>;

fn lifecycle() -> ClientLifecycleMode {
    ClientLifecycleMode::Auto {
        preferred_versions: ProtocolVersion::KNOWN_VERSIONS
            .iter()
            .rev()
            .cloned()
            .collect(),
        legacy_version: Some(ProtocolVersion::V_2024_11_05),
    }
}

fn spec_from_rmcp(server: &str, tool: &rmcp::model::Tool) -> ToolSpec {
    ToolSpec {
        name: format!("{server}__{}", tool.name),
        description: tool.description.as_deref().unwrap_or("").into(),
        parameters: Value::Object((*tool.input_schema).clone()),
    }
}

fn output_text(result: &CallToolResult) -> String {
    let texts: Vec<&str> = result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|text| text.text.as_str()))
        .collect();
    if texts.is_empty() {
        serde_json::to_string(result).unwrap_or_default()
    } else {
        texts.join("\n")
    }
}

struct McpTool {
    spec: ToolSpec,
    local_name: String,
    client: Arc<Client>,
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }
    fn class(&self) -> ToolClass {
        ToolClass::External
    }
    async fn call(&self, args: Value, _cx: &ToolCx) -> ToolOutput {
        let mut params = CallToolRequestParams::new(self.local_name.clone());
        if let Value::Object(map) = args {
            params = params.with_arguments(map);
        }
        match timeout(Duration::from_secs(30), self.client.call_tool(params)).await {
            Ok(Ok(result)) => ToolOutput {
                text: output_text(&result),
                is_error: result.is_error.unwrap_or(false),
            },
            Ok(Err(e)) => ToolOutput {
                text: format!("mcp {}: {e}", self.spec.name),
                is_error: true,
            },
            Err(_) => ToolOutput {
                text: format!("mcp {}: timeout", self.spec.name),
                is_error: true,
            },
        }
    }
}

fn http_config(
    url: &str,
    headers: &BTreeMap<String, String>,
) -> Result<StreamableHttpClientTransportConfig, String> {
    let mut custom = HashMap::new();
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| e.to_string())?;
        let value = HeaderValue::from_str(value).map_err(|_| "invalid header value".to_string())?;
        custom.insert(name, value);
    }
    Ok(StreamableHttpClientTransportConfig::with_uri(url.to_string()).custom_headers(custom))
}

async fn finish_connect(name: &str, client: Client) -> Result<Vec<Arc<dyn Tool>>, String> {
    let listed = client.list_all_tools().await.map_err(|e| e.to_string())?;
    let client = Arc::new(client);
    Ok(listed
        .iter()
        .map(|tool| {
            Arc::new(McpTool {
                spec: spec_from_rmcp(name, tool),
                local_name: tool.name.to_string(),
                client: client.clone(),
            }) as Arc<dyn Tool>
        })
        .collect())
}

async fn connect_one(cfg: &McpServerConfig) -> Result<Vec<Arc<dyn Tool>>, String> {
    match &cfg.transport {
        McpTransport::Stdio { command, args } => {
            let transport =
                TokioChildProcess::new(tokio::process::Command::new(command).configure(|cmd| {
                    cmd.args(args);
                }))
                .map_err(|e| e.to_string())?;
            let client =
                ().serve_with_lifecycle(transport, lifecycle())
                    .await
                    .map_err(|e| e.to_string())?;
            finish_connect(&cfg.name, client).await
        }
        McpTransport::Http { url, headers } => {
            let transport = StreamableHttpClientTransport::from_config(http_config(url, headers)?);
            let client =
                ().serve_with_lifecycle(transport, lifecycle())
                    .await
                    .map_err(|e| e.to_string())?;
            finish_connect(&cfg.name, client).await
        }
    }
}

pub async fn connect_tools(cfgs: &[McpServerConfig]) -> Vec<Arc<dyn Tool>> {
    let mut out = Vec::new();
    for cfg in cfgs {
        match timeout(Duration::from_secs(10), connect_one(cfg)).await {
            Ok(Ok(tools)) => out.extend(tools),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn handshake_script() -> &'static str {
        r#"
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if mid is None:
        continue
    if method == "server/discover":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"error":{"code":-32601,"message":"Method not found"}}), flush=True)
        continue
    if method == "initialize":
        result = {"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"t","version":"0"}}
    elif method == "tools/list":
        result = {"tools":[{"name":"ping","description":"p","inputSchema":{"type":"object"}}]}
    elif method == "tools/call":
        result = {"content":[{"type":"text","text":"pong"}]}
    else:
        print(json.dumps({"jsonrpc":"2.0","id":mid,"error":{"code":-32601,"message":"Method not found"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc":"2.0","id":mid,"result":result}), flush=True)
"#
    }

    fn discover_script() -> &'static str {
        r#"
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if mid is None:
        continue
    if method == "server/discover":
        result = {
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": {},
            "ttlMs": 0,
            "cacheScope": "private",
        }
    elif method == "tools/list":
        result = {"tools":[{"name":"echo","description":"e","inputSchema":{"type":"object"}}]}
    elif method == "tools/call":
        result = {"content":[{"type":"text","text":"ok-2026"}]}
    else:
        print(json.dumps({"jsonrpc":"2.0","id":mid,"error":{"code":-32601,"message":"Method not found"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc":"2.0","id":mid,"result":result}), flush=True)
"#
    }

    fn write_script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        path
    }

    fn cfg(name: &str, script: &std::path::Path) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            transport: McpTransport::Stdio {
                command: "python3".into(),
                args: vec![script.to_string_lossy().into()],
            },
        }
    }

    fn http_script() -> &'static str {
        r#"
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json

class H(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass
    def do_GET(self):
        self.send_response(405)
        self.end_headers()
    def do_DELETE(self):
        self.send_response(200)
        self.end_headers()
    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        msg = json.loads(self.rfile.read(n) or b"{}")
        mid = msg.get("id")
        method = msg.get("method")
        if mid is None:
            self.send_response(202)
            self.end_headers()
            return
        if method == "server/discover":
            result = {
                "resultType": "complete",
                "supportedVersions": ["2026-07-28"],
                "capabilities": {},
                "ttlMs": 0,
                "cacheScope": "private",
            }
        elif method == "tools/list":
            result = {"tools":[{"name":"search","description":"s","inputSchema":{"type":"object"}}]}
        elif method == "tools/call":
            result = {"content":[{"type":"text","text":"pong-http"}]}
        else:
            body = json.dumps({"jsonrpc":"2.0","id":mid,"error":{"code":-32601,"message":"Method not found"}})
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(body.encode())
            return
        body = json.dumps({"jsonrpc":"2.0","id":mid,"result":result})
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(body.encode())

httpd = ThreadingHTTPServer(("127.0.0.1", 0), H)
print(httpd.server_address[1], flush=True)
httpd.serve_forever()
"#
    }

    fn spawn_http(dir: &std::path::Path) -> (std::process::Child, String) {
        use std::io::BufRead;
        let script = write_script(dir, "mcp_http.py", http_script());
        let err_path = dir.join("http.err");
        let err_file = fs::File::create(&err_path).unwrap();
        let mut child = std::process::Command::new("python3")
            .arg("-u")
            .arg(&script)
            .stdout(std::process::Stdio::piped())
            .stderr(err_file)
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let port: u16 = line.trim().parse().unwrap_or_else(|_| {
            let _ = child.try_wait();
            panic!(
                "http mock port {:?}: {}",
                line,
                fs::read_to_string(&err_path).unwrap_or_default()
            )
        });
        (child, format!("http://127.0.0.1:{port}/"))
    }

    async fn ping(tool: &Arc<dyn Tool>, cwd: &std::path::Path) -> ToolOutput {
        tool.call(
            serde_json::json!({}),
            &ToolCx {
                cwd: cwd.into(),
                read_roots: vec![],
                cancel: Default::default(),
            },
        )
        .await
    }

    /// REQ-3.18 INV-9 names are server__tool; a dead server does not panic.
    #[tokio::test]
    async fn req_3_18_name_and_failure_isolation() {
        let dead = McpServerConfig {
            name: "dead".into(),
            transport: McpTransport::Stdio {
                command: "/bin/false".into(),
                args: vec![],
            },
        };
        let tools = connect_tools(&[dead]).await;
        assert!(tools.is_empty());

        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "server.py", handshake_script());
        let tools = connect_tools(&[cfg("demo", &script)]).await;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].spec().name, "demo__ping");
        let out = ping(&tools[0], dir.path()).await;
        assert!(!out.is_error);
        assert!(out.text.contains("pong"));
    }

    /// REQ-3.29 Auto negotiates 2026-07-28 discover and 2024-11-05 initialize in one process.
    #[tokio::test]
    async fn req_3_29_mixed_protocol_in_one_process() {
        let dir = tempfile::tempdir().unwrap();
        let modern = write_script(dir.path(), "modern.py", discover_script());
        let legacy = write_script(dir.path(), "legacy.py", handshake_script());
        let tools = connect_tools(&[cfg("new", &modern), cfg("old", &legacy)]).await;
        let names: Vec<_> = tools.iter().map(|t| t.spec().name).collect();
        assert_eq!(
            names,
            vec!["new__echo".to_string(), "old__ping".to_string()]
        );
        let modern_out = ping(&tools[0], dir.path()).await;
        assert!(!modern_out.is_error);
        assert!(modern_out.text.contains("ok-2026"));
        let legacy_out = ping(&tools[1], dir.path()).await;
        assert!(!legacy_out.is_error);
        assert!(legacy_out.text.contains("pong"));
    }

    /// REQ-3.18 INV-17 HTTP streamable connect registers server__tool; a dead URL is isolated.
    #[tokio::test]
    async fn req_3_18_http_name_and_failure_isolation() {
        let dead = McpServerConfig {
            name: "dead".into(),
            transport: McpTransport::Http {
                url: "http://127.0.0.1:1/".into(),
                headers: BTreeMap::new(),
            },
        };
        let dir = tempfile::tempdir().unwrap();
        let (mut child, url) = spawn_http(dir.path());
        let live = McpServerConfig {
            name: "web".into(),
            transport: McpTransport::Http {
                url,
                headers: BTreeMap::new(),
            },
        };
        let tools = connect_tools(&[dead, live]).await;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].spec().name, "web__search");
        let out = ping(&tools[0], dir.path()).await;
        let _ = child.kill();
        assert!(!out.is_error);
        assert!(out.text.contains("pong-http"));
    }
}
