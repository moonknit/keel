use anyhow::Result;
use async_trait::async_trait;
use clap::{Parser, Subcommand};
use keel_core::agent::Agent;
use keel_core::assembler::ContextAssembler;
use keel_core::config::LlmSettings;
use keel_core::scripted::ScriptedModel;
use keel_core::traits::{Approver, Model, Store};
use keel_core::types::{
    ApprovalRequest, Decision, Event, ModelEvent, Role, RunStatus, Usage,
};
use keel_core::AlwaysDeny;
use keel_openai::OpenAiModel;
use keel_skills::{catalog_text, discover, load_agents_md, SkillTool};
use keel_store::SqliteStore;
use keel_tools::{coding_tools, Policy, PolicyApprover};
use rustyline::DefaultEditor;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

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
    let db = SqliteStore::open(&home.join("keel.db"))?;
    let cwd = std::env::current_dir()?;
    let cwd_s = cwd.to_string_lossy().into_owned();

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
        let code = run_print(&db, &session.meta.id, &cwd_s, &prompt, allow_edits).await?;
        std::process::exit(code);
    }
    repl(&db, &session.meta.id, &cwd, allow_edits).await
}

fn keel_home() -> PathBuf {
    if let Ok(h) = std::env::var("KEEL_HOME") {
        return PathBuf::from(h);
    }
    dirs_home().join(".keel")
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

async fn session_cmd(db: &SqliteStore, cwd: &str, action: SessionCmd) -> Result<()> {
    match action {
        SessionCmd::List { all } => {
            let list = db.list_sessions(if all { None } else { Some(cwd) }, all).await?;
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
    if let Some(id) = cli.resume.clone().flatten() {
        return db.open_session(&id).await.map_err(into);
    }
    let list = db.list_sessions(Some(cwd), false).await.map_err(into)?;
    if let Some(meta) = list.into_iter().next() {
        db.open_session(&meta.id).await.map_err(into)
    } else {
        db.create_session(cwd, model).await.map_err(into)
    }
}

fn into(e: keel_core::KeelError) -> anyhow::Error {
    anyhow::anyhow!(e.to_string())
}

async fn build_agent(
    store: Arc<dyn Store>,
    cwd: &std::path::Path,
    print: bool,
    allow_edits: bool,
) -> Result<Agent> {
    let skills = discover(cwd);
    let mut tools = coding_tools();
    tools.push(Arc::new(SkillTool {
        skills: skills.clone(),
    }));
    let system = format!(
        "{}\n{}",
        load_agents_md(cwd),
        catalog_text(&skills)
    );
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
) -> Result<i32> {
    let store: Arc<dyn Store> = Arc::new(SqliteStore::open(
        &keel_home().join("keel.db"),
    )?);
    let _ = db;
    let agent = build_agent(store, cwd.as_ref(), true, allow_edits).await?;
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

async fn repl(db: &SqliteStore, session_id: &str, cwd: &std::path::Path, allow_edits: bool) -> Result<()> {
    let store: Arc<dyn Store> = Arc::new(SqliteStore::open(&keel_home().join("keel.db"))?);
    let _ = db;
    let agent = build_agent(store.clone(), cwd, false, allow_edits).await?;
    let mut rl = DefaultEditor::new()?;
    let mut last_tool = String::new();
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
            store.rename(session_id, name).await.map_err(into)?;
            continue;
        }
        if line == "/session" {
            let ws = store.open_session(session_id).await.map_err(into)?;
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
        let last_tool_slot = Arc::new(Mutex::new(String::new()));
        let last_tool_w = last_tool_slot.clone();
        let printer = tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                match ev {
                    Event::TextDelta(s) => {
                        print!("{s}");
                        let _ = io::stdout().flush();
                    }
                    Event::ToolStart { name, args } => println!("\n▸ {name}  {args}"),
                    Event::ToolEnd {
                        name,
                        ok,
                        millis,
                        lines,
                    } => {
                        if let Ok(mut slot) = last_tool_w.lock() {
                            *slot = format!("{name} ok={ok} {millis}ms {lines} lines");
                        }
                        println!("  {}", if ok { "✓" } else { "✗" });
                    }
                    Event::RunEnd { .. } => println!(),
                    _ => {}
                }
            }
        });
        let cwd_s = cwd.to_string_lossy().into_owned();
        let sid = session_id.to_string();
        let status = agent
            .run(&sid, &prompt, &cwd_s, cancel, tx)
            .await
            .map_err(into)?;
        let _ = printer.await;
        if let Ok(slot) = last_tool_slot.lock() {
            last_tool = slot.clone();
        }
        if status == RunStatus::Interrupted {
            println!("interrupted");
        }
    }
    store.close(session_id).await.map_err(into)?;
    Ok(())
}
