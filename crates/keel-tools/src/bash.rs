use crate::jail::OsJail;
use crate::policy::dangerous_bash;
use async_trait::async_trait;
use keel_core::traits::Tool;
use keel_core::types::{ToolClass, ToolCx, ToolOutput, ToolSpec};
use serde_json::{Value, json};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

const DEFAULT_TIMEOUT_S: u64 = 120;
const MAX_TIMEOUT_S: u64 = 600;
const MAX_CHARS: usize = 30_000;

pub struct Bash {
    pub timeout_s: u64,
}

impl Default for Bash {
    fn default() -> Self {
        Self {
            timeout_s: DEFAULT_TIMEOUT_S,
        }
    }
}

fn clip(s: &str) -> String {
    if s.len() <= MAX_CHARS {
        return s.to_string();
    }
    let keep = MAX_CHARS / 2;
    format!(
        "{}\n… [{} chars omitted] …\n{}",
        &s[..keep],
        s.len() - MAX_CHARS,
        &s[s.len() - keep..]
    )
}

#[async_trait]
impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "Run a shell command in the session cwd. New process each call.".into(),
            parameters: json!({
                "type":"object",
                "properties": {
                    "command": {"type":"string"},
                    "timeout_s": {"type":"integer"}
                },
                "required": ["command"]
            }),
        }
    }
    fn class(&self) -> ToolClass {
        ToolClass::Exec
    }
    async fn call(&self, args: Value, cx: &ToolCx) -> ToolOutput {
        let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let t = args
            .get("timeout_s")
            .and_then(|v| v.as_u64())
            .unwrap_or(self.timeout_s)
            .min(MAX_TIMEOUT_S);
        let jail = OsJail::detect().filter(|_| !dangerous_bash(cmd));
        let mut command = if let Some(jail) = &jail {
            let (prog, args) = jail.wrap(&cx.cwd, cmd);
            let mut c = Command::new(prog);
            c.args(args);
            c
        } else {
            let mut c = Command::new("/bin/sh");
            c.arg("-c").arg(cmd);
            c
        };
        let child = match command
            .current_dir(&cx.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                return ToolOutput {
                    text: if jail.is_some() {
                        format!("sandbox failed to start: {e}")
                    } else {
                        e.to_string()
                    },
                    is_error: true,
                };
            }
        };
        let tag = match &jail {
            Some(j) => format!("[keel sandbox: {}]\n", j.name()),
            None => "[keel sandbox: host]\n".into(),
        };
        match timeout(Duration::from_secs(t), async {
            child.wait_with_output().await
        })
        .await
        {
            Ok(Ok(out)) => {
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                if !out.stderr.is_empty() {
                    text.push('\n');
                    text.push_str(&String::from_utf8_lossy(&out.stderr));
                }
                text = clip(&text);
                ToolOutput {
                    text: format!("{tag}exit {}\n{text}", out.status.code().unwrap_or(-1)),
                    is_error: !out.status.success(),
                }
            }
            Ok(Err(e)) => ToolOutput {
                text: format!("{tag}{e}"),
                is_error: true,
            },
            Err(_) => ToolOutput {
                text: format!("{tag}timeout after {t}s"),
                is_error: true,
            },
        }
    }
}
