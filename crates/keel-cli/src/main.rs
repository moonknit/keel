use anyhow::Result;
use async_trait::async_trait;
use clap::{Parser, Subcommand};
use keel_core::AlwaysDeny;
use keel_core::agent::Agent;
use keel_core::assembler::ContextAssembler;
use keel_core::config::LlmSettings;
use keel_core::scripted::ScriptedModel;
use keel_core::traits::{Approver, Model, Store};
use keel_core::types::{ApprovalRequest, Decision, Event, ModelEvent, Role, RunStatus, Usage};
use keel_openai::OpenAiModel;
use keel_skills::{SkillTool, catalog_text, discover, load_agents_md};
use keel_store::SqliteStore;
use keel_tools::{Policy, PolicyApprover, coding_tools};
use rustyline::DefaultEditor;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

mod config;
mod last;
mod logging;
mod slash;

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
    let db = SqliteStore::open(&home.join("keel.db"))?;
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
            return session_cmd(&db, &cwd_s, action).await;
        }
        None => {}
    }

    let model_name = LlmSettings::from_env()
        .map(|s| s.model)
        .unwrap_or_else(|_| "unknown".into());
    let session = pick_session(&db, &cwd_s, &model_name, &cli).await?;
    let print = cli.prompt.clone();
    let allow_edits = cli.allow_edits;
    if let Some(prompt) = print {
        let code = run_print(&db, &session.meta.id, &cwd_s, &prompt, allow_edits, &cfg).await?;
        std::process::exit(code);
    }
    repl(&db, &session.meta.id, &cwd, allow_edits, &cfg, &model_name).await
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

fn stdio_mcp_configs(cfg: &config::KeelConfig) -> Vec<keel_mcp::McpServerConfig> {
    cfg.servers
        .iter()
        .filter(|s| s.transport == "stdio")
        .filter_map(|s| {
            Some(keel_mcp::McpServerConfig {
                name: s.name.clone(),
                command: s.command.clone()?,
                args: s.args.clone(),
            })
        })
        .collect()
}

async fn attach_mcp_tools(cfg: &config::KeelConfig) -> Vec<Arc<dyn keel_core::traits::Tool>> {
    if cfg.needs_confirm {
        return Vec::new();
    }
    keel_mcp::connect_tools(&stdio_mcp_configs(cfg)).await
}

async fn build_agent(
    store: Arc<dyn Store>,
    cwd: &std::path::Path,
    print: bool,
    allow_edits: bool,
    cfg: &config::KeelConfig,
) -> Result<Agent> {
    let skills = discover(cwd);
    let mut tools = coding_tools();
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
    let policy = Policy {
        allow_edits,
        print_mode: print,
        ..Policy::default()
    };
    let approver: Arc<dyn Approver> = if print {
        Arc::new(PolicyApprover {
            policy,
            inner: Box::new(AlwaysDeny),
        })
    } else {
        Arc::new(PolicyApprover {
            policy,
            inner: Box::new(StdinApprover),
        })
    };
    Ok(Agent {
        model,
        tools,
        store,
        approver,
        assembler: Arc::new(ContextAssembler::default()),
        max_turns: 50,
        system,
    })
}

struct StdinApprover;

#[async_trait]
impl Approver for StdinApprover {
    async fn approve(&self, req: &ApprovalRequest) -> Decision {
        eprintln!(
            "  allow {} {}? [y]es [a]lways [n]o",
            req.name, req.args_summary
        );
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        match line.trim() {
            "a" | "always" => Decision::Always,
            "n" | "no" => Decision::Deny,
            _ => Decision::Once,
        }
    }
}

async fn run_print(
    db: &SqliteStore,
    session_id: &str,
    cwd: &str,
    prompt: &str,
    allow_edits: bool,
    cfg: &config::KeelConfig,
) -> Result<i32> {
    let store: Arc<dyn Store> = Arc::new(SqliteStore::open(&keel_home().join("keel.db"))?);
    let _ = db;
    let agent = build_agent(store, cwd.as_ref(), true, allow_edits, cfg).await?;
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

async fn repl(
    db: &SqliteStore,
    session_id: &str,
    cwd: &std::path::Path,
    allow_edits: bool,
    cfg: &config::KeelConfig,
    model: &str,
) -> Result<()> {
    let store: Arc<dyn Store> = Arc::new(SqliteStore::open(&keel_home().join("keel.db"))?);
    let _ = db;
    let agent = build_agent(store.clone(), cwd, false, allow_edits, cfg).await?;
    let mut rl = DefaultEditor::new()?;
    let mut last_tool = String::new();
    let mut session_id = session_id.to_string();
    let cwd_s = cwd.to_string_lossy().into_owned();
    loop {
        let line = match rl.readline("> ") {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line == "/exit" {
            break;
        }
        if line == "/help" {
            println!("/new /resume /name /session /skills /last /help /exit");
            continue;
        }
        if let Some(switch) = slash::parse_session_switch(&line) {
            match slash::apply_session_switch(store.as_ref(), &session_id, &cwd_s, model, switch)
                .await
            {
                Ok(new_id) => {
                    session_id = new_id;
                    println!("{session_id}");
                }
                Err(e) => eprintln!("{e}"),
            }
            continue;
        }
        if line == "/last" {
            println!("{last_tool}");
            continue;
        }
        if line == "/skills" {
            for s in discover(cwd) {
                println!("{}  {}", s.name, s.description);
            }
            continue;
        }
        if let Some(name) = line.strip_prefix("/name ") {
            store.rename(&session_id, name).await.map_err(into)?;
            continue;
        }
        if line == "/session" {
            let ws = store.open_session(&session_id).await.map_err(into)?;
            println!("{}  {}  {}", ws.meta.id, ws.meta.cwd, ws.meta.model);
            continue;
        }
        if line == "/compact" {
            println!("v2 예정");
            continue;
        }
        let prompt = if let Some(rest) = line.strip_prefix("/skill:") {
            let name = rest.split_whitespace().next().unwrap_or("");
            if let Some(meta) = discover(cwd).into_iter().find(|s| s.name == name) {
                keel_skills::skill_body(&meta)
            } else {
                format!("unknown skill {name}")
            }
        } else {
            line
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let cancel_c = cancel.clone();
        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            cancel_c.cancel();
        });
        let printer = tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                match ev {
                    Event::TextDelta(s) => {
                        print!("{s}");
                        let _ = io::stdout().flush();
                    }
                    Event::ToolStart { name, args } => println!("\n▸ {name}  {args}"),
                    Event::ToolEnd { ok, .. } => {
                        println!("  {}", if ok { "✓" } else { "✗" });
                    }
                    Event::RunEnd { .. } => println!(),
                    _ => {}
                }
            }
        });
        let status = agent
            .run(&session_id, &prompt, &cwd_s, cancel, tx)
            .await
            .map_err(into)?;
        let _ = printer.await;
        if let Ok(ws) = store.open_session(&session_id).await {
            if let Some(output) = last::last_tool_output(&ws) {
                last_tool = output.to_string();
            }
        }
        if status == RunStatus::Interrupted {
            println!("interrupted");
        }
    }
    store.close(&session_id).await.map_err(into)?;
    Ok(())
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

    /// REQ-3.18 HTTP entries are skipped without crashing (stdio-only client).
    #[tokio::test]
    async fn req_3_18_mcp_http_is_skipped() {
        let tools = attach_mcp_tools(&cfg(
            vec![spec(
                "search",
                "http",
                None,
                vec![],
                Some("https://mcp.example/search"),
            )],
            false,
        ))
        .await;
        assert!(tools.is_empty());
    }
}
