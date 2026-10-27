use assert_cmd::cargo::cargo_bin;
use std::fs;
use std::path::Path;
use std::process::Command;

fn scripted(dir: &Path) -> Command {
    let mut cmd = Command::new(cargo_bin("keel"));
    cmd.current_dir(dir)
        .env("KEEL_HOME", dir)
        .env("KEEL_SCRIPTED", "1")
        .env("LLM_MODEL", "test")
        .env("OPENAI_API_KEY", "test")
        .env_remove("KEEL_LIVE");
    cmd
}

fn write_mock_server(dir: &Path) -> std::path::PathBuf {
    let script = dir.join("server.py");
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
    script
}

/// REQ-3.18 CLI start connects stdio MCP from home config without failing print.
#[test]
fn req_3_18_mcp_cli_starts_stdio_from_home_config() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_mock_server(dir.path());
    fs::write(
        dir.path().join("config.toml"),
        format!(
            r#"
[[mcp.servers]]
name = "demo"
transport = "stdio"
command = "python3"
args = ["{}"]
"#,
            script.display()
        ),
    )
    .unwrap();

    let out = scripted(dir.path())
        .args(["-p", "hello"])
        .output()
        .expect("run");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("ok"), "{stdout}");
}

/// REQ-3.18 a dead MCP server must not fail CLI start.
#[test]
fn req_3_18_mcp_cli_survives_dead_server() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("config.toml"),
        r#"
[[mcp.servers]]
name = "dead"
command = "/bin/false"
"#,
    )
    .unwrap();

    let out = scripted(dir.path())
        .args(["-p", "hello"])
        .output()
        .expect("run");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}
