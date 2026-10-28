use anyhow::Result;
use clap::{Parser, Subcommand};
use keel_core::AlwaysDeny;
use keel_core::agent::{Agent, CwdWriteLock, Inbox};
use keel_core::assembler::ContextAssembler;
use keel_core::config::LlmSettings;
use keel_core::scripted::ScriptedModel;
use keel_core::traits::{Approver, ContextSource, Model, Store};
use keel_core::types::{Event, ModelEvent, Role, RunStatus, Usage};
use keel_openai::OpenAiModel;
use keel_skills::{SkillTool, catalog_text, discover, load_agents_md};
use keel_store::SqliteStore;
use keel_tools::{Policy, PolicyApprover, default_tools};
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

mod config;
mod cron;
mod dashboard;
mod last;
mod logging;
mod slash;
mod spawn;
mod tui;

#[derive(Parser)]
#[command(name = "keel")]
struct Cli {
    #[arg(long)]
    new: bool,
    #[arg(long)]
    resume: Option<Option<String>>,
    #[arg(short = 'p', long)]
    prompt: Option<String>,
    #[arg(long)]
    allow_edits: bool,
    #[arg(long, default_value_t = false)]
    r#continue: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    Session {
        #[command(subcommand)]
        action: SessionCmd,
    },
    Serve,
    Cron {
        #[command(subcommand)]
        action: CronCmd,
    },
}

#[derive(Subcommand)]
enum CronCmd {
    List,
    Add {
        #[arg(long)]
        schedule: String,
        #[arg(long)]
        prompt: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        session: Option<String>,
        #[arg(long, default_value_t = false)]
        allow_edits: bool,
        #[arg(long, default_value = "")]
        allow_bash: String,
    },
    Rm {
        id: String,
    },
}

#[derive(Subcommand)]
enum SessionCmd {
    List {
        #[arg(long)]
        all: bool,
    },
    Show {
        id: String,
    },
    Rm {
        id: String,
    },
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    if let Err(e) = run().await {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let home = keel_home();
    std::fs::create_dir_all(&home).ok();
    let _logging = logging::init(&home)?;
    let db = Arc::new(SqliteStore::open(&home.join("keel.db"))?);
    let cwd = std::env::current_dir()?;
    let cwd_s = cwd.to_string_lossy().into_owned();
    let mut cfg = config::load(&home, &cwd)?;
    for server in &cfg.servers {
        for value in server.headers.values() {
            logging::register_secret(value);
            if let Some(token) = value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
            {
                logging::register_secret(token);
            }
        }
    }
    maybe_confirm_project(&home, &mut cfg, &cli)?;

    match cli.cmd {
        Some(Cmd::Session { action }) => {
            return session_cmd(db.as_ref(), &cwd_s, action).await;
        }
        Some(Cmd::Serve) => {
            let model_name = LlmSettings::from_env()
                .map(|s| s.model)
                .unwrap_or_else(|_| "unknown".into());
            return run_serve(db, &cwd, &cfg, &model_name).await;
        }
        Some(Cmd::Cron { action }) => {
            return cron_cmd(db.as_ref(), &cwd_s, action);
        }
        None => {}
    }

    let model_name = LlmSettings::from_env()
        .map(|s| s.model)
        .unwrap_or_else(|_| "unknown".into());
    let session = pick_session(db.as_ref(), &cwd_s, &model_name, &cli).await?;
    let print = cli.prompt.clone();
    let allow_edits = cli.allow_edits;
    if let Some(prompt) = print {
        let code = run_print(
            db.as_ref(),
            &session.meta.id,
            &cwd_s,
            &prompt,
            allow_edits,
            &cfg,
            &model_name,
        )
        .await?;
        std::process::exit(code);
    }
    if !tui::interactive_allowed(io::stdin().is_terminal()) {
        anyhow::bail!("TTY required for interactive keel");
    }
    let store: Arc<dyn Store> = db.clone();
    let (channel, approvals) = tui::ChannelApprover::pair();
    let (agent, policy_approver) = build_agent(
        store,
        &cwd,
        false,
        allow_edits,
        &cfg,
        &model_name,
        Box::new(channel),
    )
    .await?;
    tui::run(
        db,
        &session.meta.id,
        &cwd,
        agent,
        policy_approver,
        approvals,
        &model_name,
    )
    .await
}

fn keel_home() -> PathBuf {
    if let Ok(h) = std::env::var("KEEL_HOME") {
        return PathBuf::from(h);
    }
    dirs_home().join(".keel")
}

fn maybe_confirm_project(
    home: &std::path::Path,
    cfg: &mut config::KeelConfig,
    cli: &Cli,
) -> Result<()> {
    if !cfg.needs_confirm {
        return Ok(());
    }
    let interactive = cli.prompt.is_none() && cli.cmd.is_none() && io::stdin().is_terminal();
    if !interactive {
        return Ok(());
    }
    let Some(path) = cfg.project_path.clone() else {
        return Ok(());
    };
    if config::prompt_trust(&path)? {
        if let Some(raw) = cfg.project_raw.as_deref() {
            config::mark_trusted(home, &path, raw)?;
        }
        cfg.needs_confirm = false;
    } else {
        eprintln!("project config not trusted; MCP servers will not start");
    }
    Ok(())
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

async fn session_cmd(db: &SqliteStore, cwd: &str, action: SessionCmd) -> Result<()> {
    match action {
        SessionCmd::List { all } => {
            let list = db
                .list_sessions(if all { None } else { Some(cwd) }, all)
                .await?;
            for s in list {
                println!(
                    "{}  {}  {}",
                    s.id,
                    s.title.unwrap_or_else(|| "-".into()),
                    s.cwd
                );
            }
        }
        SessionCmd::Show { id } => {
            let ws = db.open_session(&id).await?;
            for chat in ws.chats {
                println!("# chat {} {}", chat.seq, chat.title.unwrap_or_default());
                for item in chat.items {
                    println!("  {:?}", item.kind);
                }
            }
            db.close(&id).await.ok();
        }
        SessionCmd::Rm { id } => {
            db.archive(&id).await?;
        }
    }
    Ok(())
}

fn cron_cmd(db: &SqliteStore, cwd: &str, action: CronCmd) -> Result<()> {
    match action {
        CronCmd::List => {
            for job in db.list_cron(Some(cwd))? {
                println!(
                    "{}  {}  {}  {}",
                    job.id,
                    job.schedule,
                    if job.enabled { "on" } else { "off" },
                    job.prompt
                );
            }
        }
        CronCmd::Add {
            schedule,
            prompt,
            title,
            session,
            allow_edits,
            allow_bash,
        } => {
            cron::parse_schedule(&schedule)?;
            let job = db.add_cron(
                title.as_deref(),
                cwd,
                session.as_deref(),
                &schedule,
                &prompt,
                allow_edits,
                &allow_bash,
            )?;
            println!("{}", job.id);
        }
        CronCmd::Rm { id } => {
            db.delete_cron(&id)?;
        }
    }
    Ok(())
}

async fn run_serve(
    db: Arc<SqliteStore>,
    cwd: &std::path::Path,
    cfg: &config::KeelConfig,
    model_name: &str,
) -> Result<()> {
    let me = i64::from(std::process::id());
    if !db.acquire_scheduler(me)? {
        anyhow::bail!("scheduler lock held by another process");
    }
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        stop.cancel();
    });
    while !cancel.is_cancelled() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let actions = cron::tick(db.as_ref(), now, me, None, false, None).await;
        for action in actions {
            if let cron::DueAction::Run {
                session_id,
                job_id,
                prompt,
            } = action
            {
                if let Ok(job) = db.get_cron(&job_id) {
                    match build_cron_agent(db.clone(), cwd, cfg, model_name, &job).await {
                        Ok(agent) => {
                            let _ = cron::fire_run(&agent, &session_id, &prompt, &job.cwd).await;
                        }
                        Err(e) => eprintln!("cron {job_id}: {e}"),
                    }
                }
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }
    }
    db.release_scheduler(me)?;
    Ok(())
}

async fn pick_session(
    db: &SqliteStore,
    cwd: &str,
    model: &str,
    cli: &Cli,
) -> Result<keel_core::WorkingSession> {
    if cli.new {
        return db.create_session(cwd, model).await.map_err(into);
    }
    slash::resume_session(db, cwd, model, cli.resume.clone().flatten().as_deref())
        .await
        .map_err(into)
}

fn into(e: keel_core::KeelError) -> anyhow::Error {
    anyhow::anyhow!(e.to_string())
}

fn mcp_configs(cfg: &config::KeelConfig) -> Vec<keel_mcp::McpServerConfig> {
    cfg.servers
        .iter()
        .filter_map(|s| match s.transport.as_str() {
            "stdio" => Some(keel_mcp::McpServerConfig {
                name: s.name.clone(),
                transport: keel_mcp::McpTransport::Stdio {
                    command: s.command.clone()?,
                    args: s.args.clone(),
                },
            }),
            "http" => Some(keel_mcp::McpServerConfig {
                name: s.name.clone(),
                transport: keel_mcp::McpTransport::Http {
                    url: s.url.clone()?,
                    headers: s.headers.clone(),
                },
            }),
            _ => None,
        })
        .collect()
}

async fn attach_mcp_tools(cfg: &config::KeelConfig) -> Vec<Arc<dyn keel_core::traits::Tool>> {
    if cfg.needs_confirm {
        return Vec::new();
    }
    keel_mcp::connect_tools(&mcp_configs(cfg)).await
}

async fn build_agent(
    store: Arc<dyn Store>,
    cwd: &std::path::Path,
    print: bool,
    allow_edits: bool,
    cfg: &config::KeelConfig,
    model_name: &str,
    inner: Box<dyn Approver>,
) -> Result<(Agent, Arc<PolicyApprover>)> {
    let skills = discover(cwd);
    let mut tools = default_tools();
    tools.push(Arc::new(SkillTool {
        skills: skills.clone(),
    }));
    tools.extend(attach_mcp_tools(cfg).await);
    let system = format!("{}\n{}", load_agents_md(cwd), catalog_text(&skills));
    let model: Arc<dyn Model> = if std::env::var("KEEL_SCRIPTED").ok().as_deref() == Some("1") {
        Arc::new(ScriptedModel::new(vec![vec![
            ModelEvent::TextDelta("ok".into()),
            ModelEvent::ItemDone(keel_core::Item::text(Role::Assistant, "ok")),
            ModelEvent::Completed {
                usage: Usage::default(),
            },
        ]]))
    } else {
        Arc::new(OpenAiModel::from_env().map_err(into)?)
    };
    let policy_approver = Arc::new(PolicyApprover {
        policy: session_policy(print, allow_edits),
        inner,
    });
    let approver: Arc<dyn Approver> = policy_approver.clone();
    let write_lock = CwdWriteLock::new();
    let assembler: Arc<dyn ContextSource> = Arc::new(ContextAssembler::default());
    let inbox = Inbox::new();
    let hub = spawn::ChildHub::new();
    let spawn = spawn::Spawn {
        store: store.clone(),
        model: model.clone(),
        tools: tools.clone(),
        approver: approver.clone(),
        assembler: assembler.clone(),
        write_lock: write_lock.clone(),
        system: system.clone(),
        max_turns: 50,
        model_name: model_name.into(),
        inbox: inbox.clone(),
        hub: hub.clone(),
    };
    tools.push(Arc::new(spawn));
    tools.push(Arc::new(spawn::Wait { hub }));
    Ok((
        Agent {
            model,
            tools,
            store,
            approver,
            assembler,
            max_turns: 50,
            system,
            write_lock,
            inbox,
            hooks: None,
        },
        policy_approver,
    ))
}

async fn build_cron_agent(
    db: Arc<SqliteStore>,
    cwd: &std::path::Path,
    cfg: &config::KeelConfig,
    model_name: &str,
    job: &keel_store::CronJob,
) -> Result<Agent> {
    let store: Arc<dyn Store> = db;
    let skills = discover(cwd);
    let mut tools = default_tools();
    tools.push(Arc::new(SkillTool {
        skills: skills.clone(),
    }));
    tools.extend(attach_mcp_tools(cfg).await);
    let system = format!("{}\n{}", load_agents_md(cwd), catalog_text(&skills));
    let model: Arc<dyn Model> = if std::env::var("KEEL_SCRIPTED").ok().as_deref() == Some("1") {
        Arc::new(ScriptedModel::new(vec![vec![
            ModelEvent::TextDelta("ok".into()),
            ModelEvent::ItemDone(keel_core::Item::text(Role::Assistant, "ok")),
            ModelEvent::Completed {
                usage: Usage::default(),
            },
        ]]))
    } else {
        Arc::new(OpenAiModel::from_env().map_err(into)?)
    };
    let _ = model_name;
    Ok(Agent {
        model,
        tools,
        store,
        approver: Arc::new(cron::CronApprover::from_job(job)),
        assembler: Arc::new(ContextAssembler::default()),
        max_turns: 50,
        system,
        write_lock: CwdWriteLock::new(),
        inbox: Inbox::new(),
        hooks: None,
    })
}

fn session_policy(print: bool, allow_edits: bool) -> Policy {
    Policy {
        allow_edits,
        print_mode: print,
        ..Policy::default()
    }
}

async fn run_print(
    db: &SqliteStore,
    session_id: &str,
    cwd: &str,
    prompt: &str,
    allow_edits: bool,
    cfg: &config::KeelConfig,
    model_name: &str,
) -> Result<i32> {
    let store: Arc<dyn Store> = Arc::new(SqliteStore::open(&keel_home().join("keel.db"))?);
    let _ = db;
    let (agent, _) = build_agent(
        store,
        cwd.as_ref(),
        true,
        allow_edits,
        cfg,
        model_name,
        Box::new(AlwaysDeny),
    )
    .await?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let printer = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if let Event::TextDelta(s) = ev {
                print!("{s}");
                let _ = io::stdout().flush();
            }
        }
        println!();
    });
    let status = agent
        .run(session_id, prompt, cwd, cancel, tx)
        .await
        .map_err(into)?;
    let _ = printer.await;
    Ok(match status {
        RunStatus::Done => 0,
        RunStatus::Error => 1,
        RunStatus::Limit => 2,
        RunStatus::Interrupted => 130,
    })
}

#[cfg(test)]
mod plan_tests {
    use super::*;
    use keel_core::types::{ApprovalRequest, Decision, ToolClass};

    /// REQ-3.26 print mode (`-p`) must not turn plan on; allow_edits still follows print policy.
    #[test]
    fn req_3_26_print_does_not_enable_plan() {
        let policy = session_policy(true, true);
        assert!(!policy.is_plan_mode());
        assert_eq!(
            policy.decide(&ApprovalRequest {
                name: "write".into(),
                class: ToolClass::Write,
                args_summary: "a".into(),
                dangerous: false,
            }),
            Some(Decision::Once)
        );
    }
}

#[cfg(test)]
mod mcp_tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn mock_server_script(dir: &std::path::Path) -> PathBuf {
        let script = dir.join("server.py");
        fs::write(
            &script,
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
"#,
        )
        .unwrap();
        script
    }

    fn spec(
        name: &str,
        transport: &str,
        command: Option<&str>,
        args: Vec<String>,
        url: Option<&str>,
    ) -> config::McpServerSpec {
        config::McpServerSpec {
            name: name.into(),
            transport: transport.into(),
            command: command.map(str::to_string),
            args,
            url: url.map(str::to_string),
            headers: BTreeMap::new(),
        }
    }

    fn cfg(servers: Vec<config::McpServerSpec>, needs_confirm: bool) -> config::KeelConfig {
        config::KeelConfig {
            servers,
            project_path: None,
            project_raw: None,
            needs_confirm,
        }
    }

    /// REQ-3.18 INV-9 stdio servers land on Agent.tools as server__tool.
    #[tokio::test]
    async fn req_3_18_inv_9_mcp_stdio_registers_server_tool() {
        let dir = tempfile::tempdir().unwrap();
        let script = mock_server_script(dir.path());
        let tools = attach_mcp_tools(&cfg(
            vec![spec(
                "demo",
                "stdio",
                Some("python3"),
                vec![script.to_string_lossy().into()],
                None,
            )],
            false,
        ))
        .await;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].spec().name, "demo__ping");
        assert!(!tools[0].spec().name.contains('.'));
    }

    /// REQ-3.18 one dead MCP server does not prevent others from registering.
    #[tokio::test]
    async fn req_3_18_mcp_dead_server_does_not_stop_loop() {
        let dir = tempfile::tempdir().unwrap();
        let script = mock_server_script(dir.path());
        let tools = attach_mcp_tools(&cfg(
            vec![
                spec("dead", "stdio", Some("/bin/false"), vec![], None),
                spec(
                    "demo",
                    "stdio",
                    Some("python3"),
                    vec![script.to_string_lossy().into()],
                    None,
                ),
            ],
            false,
        ))
        .await;
        let names: Vec<_> = tools.iter().map(|t| t.spec().name).collect();
        assert_eq!(names, vec!["demo__ping".to_string()]);
    }

    /// REQ-3.18 untrusted project config does not start MCP servers.
    #[tokio::test]
    async fn req_3_18_mcp_skips_when_needs_confirm() {
        let dir = tempfile::tempdir().unwrap();
        let script = mock_server_script(dir.path());
        let tools = attach_mcp_tools(&cfg(
            vec![spec(
                "demo",
                "stdio",
                Some("python3"),
                vec![script.to_string_lossy().into()],
                None,
            )],
            true,
        ))
        .await;
        assert!(tools.is_empty());
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
        let script = dir.join("mcp_http.py");
        fs::write(&script, http_script()).unwrap();
        let mut child = std::process::Command::new("python3")
            .arg(&script)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let port: u16 = line.trim().parse().expect("http mock port");
        (child, format!("http://127.0.0.1:{port}/"))
    }

    /// REQ-3.18 INV-17 trusted HTTP connects at start as server__tool.
    #[tokio::test]
    async fn req_3_18_inv_17_mcp_http_registers_server_tool() {
        let dir = tempfile::tempdir().unwrap();
        let (mut child, url) = spawn_http(dir.path());
        let tools = attach_mcp_tools(&cfg(
            vec![spec("search", "http", None, vec![], Some(&url))],
            false,
        ))
        .await;
        let _ = child.kill();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].spec().name, "search__search");
    }

    /// REQ-3.18 a dead HTTP server does not prevent others from registering.
    #[tokio::test]
    async fn req_3_18_mcp_dead_http_does_not_stop_loop() {
        let dir = tempfile::tempdir().unwrap();
        let script = mock_server_script(dir.path());
        let tools = attach_mcp_tools(&cfg(
            vec![
                spec("dead", "http", None, vec![], Some("http://127.0.0.1:1/")),
                spec(
                    "demo",
                    "stdio",
                    Some("python3"),
                    vec![script.to_string_lossy().into()],
                    None,
                ),
            ],
            false,
        ))
        .await;
        let names: Vec<_> = tools.iter().map(|t| t.spec().name).collect();
        assert_eq!(names, vec!["demo__ping".to_string()]);
    }
}
