//! Minimal MCP stdio client. One server's failure does not stop the loop.

use async_trait::async_trait;
use keel_core::traits::Tool;
use keel_core::types::{ToolClass, ToolCx, ToolOutput, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;

#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
}

pub struct McpProcess {
    name: String,
    _child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<BufReader<ChildStdout>>,
    next_id: AtomicU64,
}

#[derive(Serialize)]
struct RpcReq {
    jsonrpc: &'static str,
    id: u64,
    method: String,
    params: Value,
}

#[derive(Deserialize)]
struct RpcResp {
    result: Option<Value>,
    error: Option<Value>,
}

impl McpProcess {
    pub async fn spawn(cfg: &McpServerConfig) -> Result<Self, String> {
        let mut child = Command::new(&cfg.command)
            .args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| e.to_string())?;
        let stdin = child.stdin.take().ok_or("stdin")?;
        let stdout = child.stdout.take().ok_or("stdout")?;
        let proc = Self {
            name: cfg.name.clone(),
            _child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(BufReader::new(stdout)),
            next_id: AtomicU64::new(1),
        };
        let _ = timeout(
            Duration::from_secs(10),
            proc.rpc(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "keel", "version": "0.1.0"}
                }),
            ),
        )
        .await
        .map_err(|_| "connect timeout".to_string())?
        .map_err(|e| e)?;
        let _ = proc.notify("notifications/initialized", json!({})).await;
        Ok(proc)
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let line = json!({"jsonrpc":"2.0","method":method,"params":params}).to_string();
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        stdin.flush().await.map_err(|e| e.to_string())
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = RpcReq {
            jsonrpc: "2.0",
            id,
            method: method.into(),
            params,
        };
        let line = serde_json::to_string(&req).map_err(|e| e.to_string())?;
        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(format!("{line}\n").as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            stdin.flush().await.map_err(|e| e.to_string())?;
        }
        let mut stdout = self.stdout.lock().await;
        let mut buf = String::new();
        stdout
            .read_line(&mut buf)
            .await
            .map_err(|e| e.to_string())?;
        let resp: RpcResp = serde_json::from_str(buf.trim()).map_err(|e| e.to_string())?;
        if let Some(err) = resp.error {
            return Err(err.to_string());
        }
        Ok(resp.result.unwrap_or(Value::Null))
    }

    pub async fn list_tools(&self) -> Result<Vec<ToolSpec>, String> {
        let result = self.rpc("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(tools
            .into_iter()
            .filter_map(|t| {
                Some(ToolSpec {
                    name: format!("{}__{}", self.name, t.get("name")?.as_str()?),
                    description: t
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .into(),
                    parameters: t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or(json!({"type":"object"})),
                })
            })
            .collect())
    }
}

pub struct McpTool {
    pub spec: ToolSpec,
    pub local_name: String,
    pub proc: std::sync::Arc<McpProcess>,
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
        match timeout(
            Duration::from_secs(30),
            self.proc.rpc(
                "tools/call",
                json!({"name": self.local_name, "arguments": args}),
            ),
        )
        .await
        {
            Ok(Ok(v)) => ToolOutput {
                text: v.to_string(),
                is_error: false,
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

pub async fn connect_tools(cfgs: &[McpServerConfig]) -> Vec<std::sync::Arc<dyn Tool>> {
    let mut out = Vec::new();
    for cfg in cfgs {
        match timeout(Duration::from_secs(10), McpProcess::spawn(cfg)).await {
            Ok(Ok(proc)) => {
                let proc = std::sync::Arc::new(proc);
                if let Ok(specs) = proc.list_tools().await {
                    for spec in specs {
                        let local = spec
                            .name
                            .strip_prefix(&format!("{}__", cfg.name))
                            .unwrap_or(&spec.name)
                            .to_string();
                        out.push(std::sync::Arc::new(McpTool {
                            spec,
                            local_name: local,
                            proc: proc.clone(),
                        }) as std::sync::Arc<dyn Tool>);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// REQ-3.18 INV-9 names are server__tool; a dead server does not panic.
    #[tokio::test]
    async fn req_3_18_name_and_failure_isolation() {
        let dead = McpServerConfig {
            name: "dead".into(),
            command: "/bin/false".into(),
            args: vec![],
        };
        let tools = connect_tools(&[dead]).await;
        assert!(tools.is_empty());

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("server.py");
        fs::write(
            &script,
            r#"
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if method == "notifications/initialized":
        continue
    if method == "initialize":
        result = {"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"t","version":"0"}}
    elif method == "tools/list":
        result = {"tools":[{"name":"ping","description":"p","inputSchema":{"type":"object"}}]}
    elif method == "tools/call":
        result = {"content":[{"type":"text","text":"pong"}]}
    else:
        result = {}
    print(json.dumps({"jsonrpc":"2.0","id":mid,"result":result}), flush=True)
"#,
        )
        .unwrap();
        let cfg = McpServerConfig {
            name: "demo".into(),
            command: "python3".into(),
            args: vec![script.to_string_lossy().into()],
        };
        let tools = connect_tools(&[cfg]).await;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].spec().name, "demo__ping");
        let out = tools[0]
            .call(
                json!({}),
                &ToolCx {
                    cwd: dir.path().into(),
                    read_roots: vec![],
                    cancel: Default::default(),
                },
            )
            .await;
        assert!(!out.is_error);
        assert!(out.text.contains("pong"));
    }
}
