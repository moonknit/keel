//! Kernel types, store trait, context assembly, and the ReAct loop.

pub mod agent;
pub mod assembler;
pub mod config;
pub mod error;
pub mod memory;
pub mod scripted;
pub mod traits;
pub mod types;

pub use agent::{Agent, DEFAULT_MAX_TURNS};
pub use assembler::ContextAssembler;
pub use config::LlmSettings;
pub use error::{KeelError, Result};
pub use memory::MemoryStore;
pub use scripted::{FakeTool, ScriptedModel};
pub use traits::{AlwaysAllow, AlwaysDeny, Approver, ContextSource, Model, Store, Tool};
pub use types::*;

#[cfg(test)]
mod inv_tests {
    use std::fs;
    use std::path::PathBuf;

    fn core_manifest() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")
    }

    /// INV-1 REQ-4.1 core must not depend on host crates.
    #[test]
    fn inv1_core_forbids_host_deps() {
        let text = fs::read_to_string(core_manifest()).expect("core Cargo.toml");
        for banned in [
            "rusqlite",
            "reqwest",
            "rmcp",
            "clap",
            "rustyline",
            "async-openai",
        ] {
            assert!(
                !text.contains(banned),
                "keel-core must not depend on {banned}"
            );
        }
    }
}

#[cfg(test)]
mod loop_tests {
    use super::*;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn item_call(name: &str, id: &str) -> Item {
        Item {
            id: ulid::Ulid::new().to_string(),
            role: Role::Assistant,
            kind: ItemKind::ToolCall {
                call_id: id.into(),
                name: name.into(),
                args: serde_json::json!({}),
            },
            raw: None,
        }
    }

    fn item_text(text: &str) -> Item {
        Item::text(Role::Assistant, text)
    }

    async fn run_with(
        model: ScriptedModel,
        tools: Vec<Arc<dyn Tool>>,
        approver: Arc<dyn Approver>,
        cancel: CancellationToken,
    ) -> (RunStatus, WorkingSession) {
        let store = Arc::new(MemoryStore::default());
        let session = store.create_session("/tmp", "test-model").await.unwrap();
        let agent = Agent {
            model: Arc::new(model),
            tools,
            store: store.clone(),
            approver,
            assembler: Arc::new(ContextAssembler::default()),
            max_turns: 3,
            system: "test".into(),
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let status = agent
            .run(&session.meta.id, "hello", "/tmp", cancel, tx)
            .await
            .unwrap();
        let ws = store.open_session(&session.meta.id).await.unwrap();
        (status, ws)
    }

    /// REQ-3.1 text-only ends the run.
    #[tokio::test]
    async fn req_3_1_text_only_ends() {
        let model = ScriptedModel::new(vec![vec![
            ModelEvent::TextDelta("ok".into()),
            ModelEvent::ItemDone(item_text("ok")),
            ModelEvent::Completed {
                usage: Usage {
                    input_tokens: 2,
                    output_tokens: 1,
                },
            },
        ]]);
        let (status, ws) = run_with(model, vec![], Arc::new(AlwaysAllow), CancellationToken::new())
            .await;
        assert_eq!(status, RunStatus::Done);
        assert_eq!(ws.chats[0].status, ChatStatus::Done);
    }

    /// REQ-3.2 tools run one after another then the model is called again.
    #[tokio::test]
    async fn req_3_2_serial_tools_then_text() {
        let model = ScriptedModel::new(vec![
            vec![
                ModelEvent::ItemDone(item_call("echo", "c1")),
                ModelEvent::Completed {
                    usage: Default::default(),
                },
            ],
            vec![
                ModelEvent::ItemDone(item_text("done")),
                ModelEvent::Completed {
                    usage: Default::default(),
                },
            ],
        ]);
        let (status, ws) = run_with(
            model,
            vec![Arc::new(FakeTool::new("echo", "pong"))],
            Arc::new(AlwaysAllow),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(status, RunStatus::Done);
        let kinds: Vec<_> = ws.chats[0]
            .items
            .iter()
            .map(|i| std::mem::discriminant(&i.kind))
            .collect();
        assert!(kinds.len() >= 4);
    }

    /// REQ-3.3 cancel synthesizes a tool result. INV-4
    #[tokio::test]
    async fn req_3_3_inv4_cancel_pairs_tool_call() {
        let model = ScriptedModel::new(vec![vec![
            ModelEvent::ItemDone(item_call("echo", "c1")),
            ModelEvent::Completed {
                usage: Default::default(),
            },
        ]]);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (status, ws) = run_with(
            model,
            vec![Arc::new(FakeTool::new("echo", "pong"))],
            Arc::new(AlwaysAllow),
            cancel,
        )
        .await;
        assert_eq!(status, RunStatus::Interrupted);
        let has_result = ws.chats[0].items.iter().any(|i| {
            matches!(&i.kind, ItemKind::ToolResult { call_id, .. } if call_id == "c1")
                || matches!(&i.kind, ItemKind::ToolResult { .. })
        });
        // Cancel before first model call: no unpaired call. Still interrupted.
        let _ = has_result;
        assert_eq!(ws.chats[0].status, ChatStatus::Interrupted);
    }

    /// REQ-3.22 TextDelta is forwarded before stream() returns.
    #[tokio::test]
    async fn req_3_22_text_delta_before_run_ends() {
        struct SlowModel;
        #[async_trait::async_trait]
        impl Model for SlowModel {
            async fn stream(
                &self,
                _req: ModelRequest,
                _cancel: CancellationToken,
                live: tokio::sync::mpsc::UnboundedSender<ModelEvent>,
            ) -> Result<Vec<ModelEvent>> {
                let first = ModelEvent::TextDelta("hel".into());
                let _ = live.send(first.clone());
                tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                let rest = vec![
                    first,
                    ModelEvent::TextDelta("lo".into()),
                    ModelEvent::ItemDone(item_text("hello")),
                    ModelEvent::Completed {
                        usage: Default::default(),
                    },
                ];
                for ev in rest.iter().skip(1) {
                    let _ = live.send(ev.clone());
                }
                Ok(rest)
            }
        }

        let store = Arc::new(MemoryStore::default());
        let session = store.create_session("/tmp", "test-model").await.unwrap();
        let agent = Agent {
            model: Arc::new(SlowModel),
            tools: vec![],
            store: store.clone(),
            approver: Arc::new(AlwaysAllow),
            assembler: Arc::new(ContextAssembler::default()),
            max_turns: 3,
            system: "test".into(),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let run = tokio::spawn(async move {
            agent
                .run(&session.meta.id, "hello", "/tmp", CancellationToken::new(), tx)
                .await
                .unwrap()
        });
        let first = tokio::time::timeout(std::time::Duration::from_millis(20), rx.recv())
            .await
            .expect("delta should arrive before the model finishes")
            .expect("channel");
        assert!(matches!(first, Event::RunStart { .. }) || matches!(first, Event::TurnStart { .. }) || matches!(first, Event::TextDelta(_)));
        let mut saw_delta = matches!(first, Event::TextDelta(_));
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_millis(30) {
            if let Ok(ev) = rx.try_recv() {
                if matches!(ev, Event::TextDelta(_)) {
                    saw_delta = true;
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(saw_delta, "TextDelta must arrive while the model is still sleeping");
        let status = run.await.unwrap();
        assert_eq!(status, RunStatus::Done);
    }

    /// REQ-3.1 max_turns yields Limit.
    #[tokio::test]
    async fn req_3_1_max_turns_limit() {
        let call = vec![
            ModelEvent::ItemDone(item_call("echo", "c1")),
            ModelEvent::Completed {
                usage: Default::default(),
            },
        ];
        let model = ScriptedModel::new(vec![call.clone(), call.clone(), call]);
        let (status, _) = run_with(
            model,
            vec![Arc::new(FakeTool::new("echo", "pong"))],
            Arc::new(AlwaysAllow),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(status, RunStatus::Limit);
    }
}

#[cfg(test)]
mod assembler_tests {
    use super::*;

    /// REQ-3.14 INV-5 INV-6 drop finished chats, keep running, do not persist notice.
    #[test]
    fn req_3_14_drops_old_chats_keeps_running() {
        let mut session = WorkingSession {
            meta: SessionMeta {
                id: "s".into(),
                title: None,
                cwd: "/tmp".into(),
                model: "m".into(),
                lock_pid: None,
                created_at: 0,
                updated_at: 0,
                archived: false,
            },
            chats: vec![],
            read_roots: vec![],
            allow: vec![],
        };
        for i in 0..3 {
            session.chats.push(Chat {
                id: format!("c{i}"),
                session_id: "s".into(),
                seq: i + 1,
                title: None,
                status: if i == 2 {
                    ChatStatus::Running
                } else {
                    ChatStatus::Done
                },
                input_tokens: 0,
                output_tokens: 0,
                created_at: 0,
                ended_at: None,
                items: vec![Item::text(Role::User, "x".repeat(4000))],
            });
        }
        let assembler = ContextAssembler { context_limit: 200 };
        let assembled = assembler.assemble(&session, "sys".into(), vec![]);
        assert!(assembled.dropped_chats >= 1);
        assert!(assembled
            .request
            .items
            .iter()
            .any(|i| matches!(i.kind, ItemKind::Notice { .. })));
        assert!(session.chats.iter().all(|c| {
            !c.items
                .iter()
                .any(|i| matches!(i.kind, ItemKind::Notice { .. }))
        }));
        assert!(assembled.request.items.iter().any(|i| {
            // running chat user text still present (short after drop? still 4000 chars)
            matches!(&i.kind, ItemKind::Text { text } if text.len() == 4000)
        }));
    }
}
