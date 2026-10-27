//! Kernel types, store trait, context assembly, and the ReAct loop.

pub mod agent;
pub mod assembler;
pub mod config;
pub mod error;
pub mod memory;
pub mod scripted;
pub mod traits;
pub mod types;

pub use agent::{Agent, CwdWriteLock, DEFAULT_MAX_TURNS, Inbox};
pub use assembler::ContextAssembler;
pub use config::LlmSettings;
pub use error::{KeelError, Result};
pub use memory::MemoryStore;
pub use scripted::{FakeTool, ScriptedModel};
pub use traits::{
    AlwaysAllow, AlwaysDeny, Approver, ApproverHooks, ContextSource, Hooks, Model, Store, Tool,
};
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
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
            write_lock: CwdWriteLock::default(),
            inbox: Inbox::default(),
            hooks: None,
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let status = agent
            .run(&session.meta.id, "hello", "/tmp", cancel, tx)
            .await
            .unwrap();
        let ws = store.open_session(&session.meta.id).await.unwrap();
        (status, ws)
    }

    async fn run_with_hooks(
        model: ScriptedModel,
        tools: Vec<Arc<dyn Tool>>,
        approver: Arc<dyn Approver>,
        hooks: Arc<dyn Hooks>,
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
            write_lock: CwdWriteLock::default(),
            inbox: Inbox::default(),
            hooks: Some(hooks),
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
        let (status, ws) = run_with(
            model,
            vec![],
            Arc::new(AlwaysAllow),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(status, RunStatus::Done);
        assert_eq!(ws.chats[0].status, ChatStatus::Done);
    }

    /// REQ-3.2 tools in a turn then the model is called again.
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

    #[derive(Clone)]
    struct Probe {
        inflight: Arc<AtomicUsize>,
        max: Arc<AtomicUsize>,
        starts: Arc<Mutex<Vec<Instant>>>,
        ends: Arc<Mutex<Vec<Instant>>>,
    }

    impl Probe {
        fn new() -> Self {
            Self {
                inflight: Arc::new(AtomicUsize::new(0)),
                max: Arc::new(AtomicUsize::new(0)),
                starts: Arc::new(Mutex::new(Vec::new())),
                ends: Arc::new(Mutex::new(Vec::new())),
            }
        }
        fn enter(&self) {
            self.starts.lock().expect("starts").push(Instant::now());
            let n = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max.fetch_max(n, Ordering::SeqCst);
        }
        fn leave(&self) {
            self.ends.lock().expect("ends").push(Instant::now());
            self.inflight.fetch_sub(1, Ordering::SeqCst);
        }
    }

    struct ProbeTool {
        name: String,
        class: ToolClass,
        delay: Duration,
        probe: Probe,
        text: String,
    }

    #[async_trait::async_trait]
    impl Tool for ProbeTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.clone(),
                description: self.name.clone(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            }
        }
        fn class(&self) -> ToolClass {
            self.class
        }
        async fn call(&self, _args: serde_json::Value, _cx: &ToolCx) -> ToolOutput {
            self.probe.enter();
            tokio::time::sleep(self.delay).await;
            self.probe.leave();
            ToolOutput {
                text: self.text.clone(),
                is_error: false,
            }
        }
    }

    /// REQ-3.2 two Read tools in one turn overlap.
    #[tokio::test]
    async fn req_3_2_reads_run_in_parallel() {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        struct BarrierRead {
            name: String,
            barrier: Arc<tokio::sync::Barrier>,
        }
        #[async_trait::async_trait]
        impl Tool for BarrierRead {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: self.name.clone(),
                    description: self.name.clone(),
                    parameters: serde_json::json!({"type":"object","properties":{}}),
                }
            }
            fn class(&self) -> ToolClass {
                ToolClass::Read
            }
            async fn call(&self, _args: serde_json::Value, _cx: &ToolCx) -> ToolOutput {
                self.barrier.wait().await;
                ToolOutput {
                    text: self.name.clone(),
                    is_error: false,
                }
            }
        }
        let model = ScriptedModel::new(vec![
            vec![
                ModelEvent::ItemDone(item_call("r1", "c1")),
                ModelEvent::ItemDone(item_call("r2", "c2")),
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
        let ran = tokio::time::timeout(
            Duration::from_secs(2),
            run_with(
                model,
                vec![
                    Arc::new(BarrierRead {
                        name: "r1".into(),
                        barrier: barrier.clone(),
                    }),
                    Arc::new(BarrierRead {
                        name: "r2".into(),
                        barrier,
                    }),
                ],
                Arc::new(AlwaysAllow),
                CancellationToken::new(),
            ),
        )
        .await
        .expect("REQ-3.2 reads must meet at the barrier together");
        let (status, ws) = ran;
        assert_eq!(status, RunStatus::Done);
        for id in ["c1", "c2"] {
            assert!(ws.chats[0].items.iter().any(|i| {
                matches!(&i.kind, ItemKind::ToolResult { call_id, is_error, .. } if call_id == id && !is_error)
            }));
        }
    }

    /// REQ-3.2 Write and Exec in one turn do not overlap.
    #[tokio::test]
    async fn req_3_2_writes_and_exec_stay_serial() {
        let probe = Probe::new();
        let delay = Duration::from_millis(40);
        let model = ScriptedModel::new(vec![
            vec![
                ModelEvent::ItemDone(item_call("w1", "c1")),
                ModelEvent::ItemDone(item_call("bash", "c2")),
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
        let (status, _) = run_with(
            model,
            vec![
                Arc::new(ProbeTool {
                    name: "w1".into(),
                    class: ToolClass::Write,
                    delay,
                    probe: probe.clone(),
                    text: "w".into(),
                }),
                Arc::new(ProbeTool {
                    name: "bash".into(),
                    class: ToolClass::Exec,
                    delay,
                    probe: probe.clone(),
                    text: "b".into(),
                }),
            ],
            Arc::new(AlwaysAllow),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(status, RunStatus::Done);
        assert_eq!(probe.max.load(Ordering::SeqCst), 1);
    }

    /// REQ-3.2 mixed Read and Write: the read batch finishes before Write starts.
    #[tokio::test]
    async fn req_3_2_reads_finish_before_writes() {
        let reads = Probe::new();
        let writes = Probe::new();
        let model = ScriptedModel::new(vec![
            vec![
                ModelEvent::ItemDone(item_call("w1", "cw")),
                ModelEvent::ItemDone(item_call("r1", "cr1")),
                ModelEvent::ItemDone(item_call("r2", "cr2")),
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
        let (status, _) = run_with(
            model,
            vec![
                Arc::new(ProbeTool {
                    name: "w1".into(),
                    class: ToolClass::Write,
                    delay: Duration::from_millis(10),
                    probe: writes.clone(),
                    text: "w".into(),
                }),
                Arc::new(ProbeTool {
                    name: "r1".into(),
                    class: ToolClass::Read,
                    delay: Duration::from_millis(40),
                    probe: reads.clone(),
                    text: "r1".into(),
                }),
                Arc::new(ProbeTool {
                    name: "r2".into(),
                    class: ToolClass::Read,
                    delay: Duration::from_millis(40),
                    probe: reads.clone(),
                    text: "r2".into(),
                }),
            ],
            Arc::new(AlwaysAllow),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(status, RunStatus::Done);
        let last_read = reads
            .ends
            .lock()
            .expect("ends")
            .iter()
            .copied()
            .max()
            .expect("reads finished");
        let first_write = writes
            .starts
            .lock()
            .expect("starts")
            .first()
            .copied()
            .expect("write started");
        assert!(
            first_write >= last_read,
            "REQ-3.2 write must start after the read batch"
        );
        assert!(reads.max.load(Ordering::SeqCst) >= 2);
    }

    fn assert_inv4_paired(ws: &WorkingSession, call_id: &str) {
        let items = &ws.chats[0].items;
        let calls = items
            .iter()
            .filter(|i| matches!(&i.kind, ItemKind::ToolCall { call_id: id, .. } if id == call_id))
            .count();
        let results: Vec<_> = items
            .iter()
            .filter_map(|i| match &i.kind {
                ItemKind::ToolResult {
                    call_id: id,
                    output,
                    is_error,
                } if id == call_id => Some((output.as_str(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(calls, 1, "transcript must contain tool_call {call_id}");
        assert_eq!(
            results.len(),
            1,
            "transcript must contain exactly one tool_result for {call_id}"
        );
        assert!(
            results[0].1,
            "synthesized pair for {call_id} should be an error result"
        );
        assert_eq!(results[0].0, "interrupted");
    }

    /// REQ-3.3 INV-4 cancel before the first model call still ends interrupted.
    #[tokio::test]
    async fn req_3_3_inv4_cancel_before_first_call() {
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
        assert_eq!(ws.chats[0].status, ChatStatus::Interrupted);
        assert!(
            !ws.chats[0]
                .items
                .iter()
                .any(|i| { matches!(&i.kind, ItemKind::ToolCall { .. }) })
        );
    }

    /// REQ-3.3 INV-4 cancel after a streamed tool_call pairs that call_id.
    #[tokio::test]
    async fn req_3_3_inv4_cancel_mid_stream_pairs_tool_call() {
        let (yielded_tx, mut yielded_rx) = tokio::sync::watch::channel(false);
        struct MidStreamModel {
            yielded: tokio::sync::watch::Sender<bool>,
        }
        #[async_trait::async_trait]
        impl Model for MidStreamModel {
            async fn stream(
                &self,
                _req: ModelRequest,
                cancel: CancellationToken,
                live: tokio::sync::mpsc::UnboundedSender<ModelEvent>,
            ) -> Result<Vec<ModelEvent>> {
                let ev = vec![
                    ModelEvent::ItemDone(item_call("echo", "c-stream")),
                    ModelEvent::Completed {
                        usage: Default::default(),
                    },
                ];
                for item in &ev {
                    let _ = live.send(item.clone());
                }
                let _ = self.yielded.send(true);
                tokio::select! {
                    _ = cancel.cancelled() => {}
                    _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
                }
                Ok(ev)
            }
        }

        let store = Arc::new(MemoryStore::default());
        let session = store.create_session("/tmp", "test-model").await.unwrap();
        let cancel = CancellationToken::new();
        let agent = Agent {
            model: Arc::new(MidStreamModel {
                yielded: yielded_tx,
            }),
            tools: vec![Arc::new(FakeTool::new("echo", "pong"))],
            store: store.clone(),
            approver: Arc::new(AlwaysAllow),
            assembler: Arc::new(ContextAssembler::default()),
            max_turns: 3,
            system: "test".into(),
            write_lock: CwdWriteLock::default(),
            inbox: Inbox::default(),
            hooks: None,
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel_run = cancel.clone();
        let sid = session.meta.id.clone();
        let run =
            tokio::spawn(
                async move { agent.run(&sid, "hello", "/tmp", cancel, tx).await.unwrap() },
            );
        yielded_rx
            .wait_for(|ready| *ready)
            .await
            .expect("model must yield the tool_call");
        cancel_run.cancel();
        let status = tokio::time::timeout(std::time::Duration::from_secs(2), run)
            .await
            .expect("mid-stream cancel must finish the run")
            .unwrap();
        assert_eq!(status, RunStatus::Interrupted);
        let ws = store.open_session(&session.meta.id).await.unwrap();
        assert_eq!(ws.chats[0].status, ChatStatus::Interrupted);
        assert_inv4_paired(&ws, "c-stream");
    }

    struct SlowTool {
        name: String,
    }

    impl SlowTool {
        fn new(name: &str) -> Self {
            Self { name: name.into() }
        }
    }

    #[async_trait::async_trait]
    impl Tool for SlowTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.clone(),
                description: self.name.clone(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            }
        }
        fn class(&self) -> ToolClass {
            ToolClass::Read
        }
        async fn call(&self, _args: serde_json::Value, _cx: &ToolCx) -> ToolOutput {
            tokio::time::sleep(Duration::from_secs(30)).await;
            ToolOutput {
                text: "late".into(),
                is_error: false,
            }
        }
    }

    /// REQ-3.3 INV-4 cancel while a tool is running still pairs the call.
    #[tokio::test]
    async fn req_3_3_inv4_cancel_mid_tool_pairs_tool_call() {
        let model = ScriptedModel::new(vec![vec![
            ModelEvent::ItemDone(item_call("slow", "c-tool")),
            ModelEvent::Completed {
                usage: Default::default(),
            },
        ]]);
        let store = Arc::new(MemoryStore::default());
        let session = store.create_session("/tmp", "test-model").await.unwrap();
        let cancel = CancellationToken::new();
        let agent = Agent {
            model: Arc::new(model),
            tools: vec![Arc::new(SlowTool::new("slow"))],
            store: store.clone(),
            approver: Arc::new(AlwaysAllow),
            assembler: Arc::new(ContextAssembler::default()),
            max_turns: 3,
            system: "test".into(),
            write_lock: CwdWriteLock::default(),
            inbox: Inbox::default(),
            hooks: None,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel_run = cancel.clone();
        let sid = session.meta.id.clone();
        let run =
            tokio::spawn(
                async move { agent.run(&sid, "hello", "/tmp", cancel, tx).await.unwrap() },
            );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut saw_start = false;
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await {
                Ok(Some(Event::ToolStart { .. })) => {
                    saw_start = true;
                    break;
                }
                Ok(None) => break,
                Ok(Some(_)) | Err(_) => {}
            }
        }
        assert!(saw_start, "tool must start before mid-tool cancel");
        cancel_run.cancel();
        let status = tokio::time::timeout(std::time::Duration::from_secs(2), run)
            .await
            .expect("mid-tool cancel must finish the run")
            .unwrap();
        assert_eq!(status, RunStatus::Interrupted);
        let ws = store.open_session(&session.meta.id).await.unwrap();
        assert_eq!(ws.chats[0].status, ChatStatus::Interrupted);
        assert_inv4_paired(&ws, "c-tool");
    }

    /// REQ-3.3 INV-4 cancel during parallel reads pairs every call_id.
    #[tokio::test]
    async fn req_3_3_inv4_cancel_parallel_reads_pairs_all() {
        let model = ScriptedModel::new(vec![vec![
            ModelEvent::ItemDone(item_call("slow-a", "c-a")),
            ModelEvent::ItemDone(item_call("slow-b", "c-b")),
            ModelEvent::Completed {
                usage: Default::default(),
            },
        ]]);
        let store = Arc::new(MemoryStore::default());
        let session = store.create_session("/tmp", "test-model").await.unwrap();
        let cancel = CancellationToken::new();
        let agent = Agent {
            model: Arc::new(model),
            tools: vec![
                Arc::new(SlowTool::new("slow-a")),
                Arc::new(SlowTool::new("slow-b")),
            ],
            store: store.clone(),
            approver: Arc::new(AlwaysAllow),
            assembler: Arc::new(ContextAssembler::default()),
            max_turns: 3,
            system: "test".into(),
            write_lock: CwdWriteLock::default(),
            inbox: Inbox::default(),
            hooks: None,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel_run = cancel.clone();
        let sid = session.meta.id.clone();
        let run =
            tokio::spawn(
                async move { agent.run(&sid, "hello", "/tmp", cancel, tx).await.unwrap() },
            );
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut starts = 0usize;
        while Instant::now() < deadline && starts < 2 {
            match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                Ok(Some(Event::ToolStart { .. })) => starts += 1,
                Ok(None) => break,
                Ok(Some(_)) | Err(_) => {}
            }
        }
        assert_eq!(starts, 2, "both reads must start before cancel");
        cancel_run.cancel();
        let status = tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .expect("parallel-read cancel must finish the run")
            .unwrap();
        assert_eq!(status, RunStatus::Interrupted);
        let ws = store.open_session(&session.meta.id).await.unwrap();
        assert_eq!(ws.chats[0].status, ChatStatus::Interrupted);
        assert_inv4_paired(&ws, "c-a");
        assert_inv4_paired(&ws, "c-b");
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
            write_lock: CwdWriteLock::default(),
            inbox: Inbox::default(),
            hooks: None,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let run = tokio::spawn(async move {
            agent
                .run(
                    &session.meta.id,
                    "hello",
                    "/tmp",
                    CancellationToken::new(),
                    tx,
                )
                .await
                .unwrap()
        });
        let first = tokio::time::timeout(std::time::Duration::from_millis(20), rx.recv())
            .await
            .expect("delta should arrive before the model finishes")
            .expect("channel");
        assert!(
            matches!(first, Event::RunStart { .. })
                || matches!(first, Event::TurnStart { .. })
                || matches!(first, Event::TextDelta(_))
        );
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
        assert!(
            saw_delta,
            "TextDelta must arrive while the model is still sleeping"
        );
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

    async fn seed_done_chats(store: &MemoryStore, session_id: &str, n: usize) {
        for i in 0..n {
            let chat = store.begin_chat(session_id, Some("old")).await.unwrap();
            store
                .append(&chat.id, Item::text(Role::User, format!("old-{i}")))
                .await
                .unwrap();
            store
                .end_chat(&chat.id, ChatStatus::Done, Usage::default())
                .await
                .unwrap();
        }
    }

    /// REQ-4.4 overflow drops more finished chats and retries the model once.
    #[tokio::test]
    async fn req_4_4_overflow_drops_finished_and_retries_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct OverflowThenOk {
            calls: AtomicUsize,
            saw_oldest: std::sync::Mutex<Vec<bool>>,
        }
        #[async_trait::async_trait]
        impl Model for OverflowThenOk {
            async fn stream(
                &self,
                req: ModelRequest,
                _cancel: CancellationToken,
                live: tokio::sync::mpsc::UnboundedSender<ModelEvent>,
            ) -> Result<Vec<ModelEvent>> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                let has_oldest = req
                    .items
                    .iter()
                    .any(|i| matches!(&i.kind, ItemKind::Text { text } if text == "old-0"));
                self.saw_oldest.lock().unwrap().push(has_oldest);
                if n == 0 {
                    return Err(KeelError::Msg("overflow".into()));
                }
                let ev = vec![
                    ModelEvent::ItemDone(item_text("ok")),
                    ModelEvent::Completed {
                        usage: Default::default(),
                    },
                ];
                for item in &ev {
                    let _ = live.send(item.clone());
                }
                Ok(ev)
            }
        }

        let store = Arc::new(MemoryStore::default());
        let session = store.create_session("/tmp", "test-model").await.unwrap();
        seed_done_chats(&store, &session.meta.id, 2).await;
        let model = Arc::new(OverflowThenOk {
            calls: AtomicUsize::new(0),
            saw_oldest: std::sync::Mutex::new(Vec::new()),
        });
        let agent = Agent {
            model: model.clone(),
            tools: vec![],
            store: store.clone(),
            approver: Arc::new(AlwaysAllow),
            assembler: Arc::new(ContextAssembler::default()),
            max_turns: 3,
            system: "test".into(),
            write_lock: CwdWriteLock::default(),
            inbox: Inbox::default(),
            hooks: None,
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let status = agent
            .run(
                &session.meta.id,
                "hello",
                "/tmp",
                CancellationToken::new(),
                tx,
            )
            .await
            .unwrap();
        assert_eq!(status, RunStatus::Done);
        assert_eq!(model.calls.load(Ordering::SeqCst), 2);
        let saw = model.saw_oldest.lock().unwrap().clone();
        assert_eq!(
            saw,
            vec![true, false],
            "retry must drop the oldest finished chat"
        );
        let ws = store.open_session(&session.meta.id).await.unwrap();
        assert_eq!(ws.chats.last().unwrap().status, ChatStatus::Done);
    }

    /// REQ-4.4 a second overflow after the one retry is an overflow error.
    #[tokio::test]
    async fn req_4_4_overflow_after_retry_is_error() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct AlwaysOverflow {
            calls: AtomicUsize,
        }
        #[async_trait::async_trait]
        impl Model for AlwaysOverflow {
            async fn stream(
                &self,
                _req: ModelRequest,
                _cancel: CancellationToken,
                _live: tokio::sync::mpsc::UnboundedSender<ModelEvent>,
            ) -> Result<Vec<ModelEvent>> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(KeelError::Msg("overflow".into()))
            }
        }

        let store = Arc::new(MemoryStore::default());
        let session = store.create_session("/tmp", "test-model").await.unwrap();
        seed_done_chats(&store, &session.meta.id, 1).await;
        let model = Arc::new(AlwaysOverflow {
            calls: AtomicUsize::new(0),
        });
        let agent = Agent {
            model: model.clone(),
            tools: vec![],
            store: store.clone(),
            approver: Arc::new(AlwaysAllow),
            assembler: Arc::new(ContextAssembler::default()),
            max_turns: 3,
            system: "test".into(),
            write_lock: CwdWriteLock::default(),
            inbox: Inbox::default(),
            hooks: None,
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let err = agent
            .run(
                &session.meta.id,
                "hello",
                "/tmp",
                CancellationToken::new(),
                tx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("overflow"), "{err}");
        assert_eq!(
            model.calls.load(Ordering::SeqCst),
            2,
            "overflow retries the model exactly once"
        );
        let ws = store.open_session(&session.meta.id).await.unwrap();
        assert_eq!(ws.chats.last().unwrap().status, ChatStatus::Error);
    }

    /// REQ-5.2 first user line fills an empty session title; later chats do not overwrite it.
    #[tokio::test]
    async fn req_5_2_first_user_line_sets_session_title() {
        let store = Arc::new(MemoryStore::default());
        let session = store.create_session("/tmp", "test-model").await.unwrap();
        assert!(session.meta.title.is_none());
        let model = ScriptedModel::new(vec![
            vec![
                ModelEvent::ItemDone(item_text("ok")),
                ModelEvent::Completed {
                    usage: Default::default(),
                },
            ],
            vec![
                ModelEvent::ItemDone(item_text("ok2")),
                ModelEvent::Completed {
                    usage: Default::default(),
                },
            ],
        ]);
        let agent = Agent {
            model: Arc::new(model),
            tools: vec![],
            store: store.clone(),
            approver: Arc::new(AlwaysAllow),
            assembler: Arc::new(ContextAssembler::default()),
            max_turns: 3,
            system: "test".into(),
            write_lock: CwdWriteLock::default(),
            inbox: Inbox::default(),
            hooks: None,
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        agent
            .run(
                &session.meta.id,
                "first line\nsecond",
                "/tmp",
                CancellationToken::new(),
                tx,
            )
            .await
            .unwrap();
        let ws = store.open_session(&session.meta.id).await.unwrap();
        assert_eq!(ws.meta.title.as_deref(), Some("first line"));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        agent
            .run(
                &session.meta.id,
                "other title",
                "/tmp",
                CancellationToken::new(),
                tx,
            )
            .await
            .unwrap();
        let ws = store.open_session(&session.meta.id).await.unwrap();
        assert_eq!(ws.meta.title.as_deref(), Some("first line"));
    }

    fn scripted_one_call(name: &str, id: &str) -> ScriptedModel {
        ScriptedModel::new(vec![
            vec![
                ModelEvent::ItemDone(item_call(name, id)),
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
        ])
    }

    async fn run_locked(
        model: ScriptedModel,
        tools: Vec<Arc<dyn Tool>>,
        write_lock: CwdWriteLock,
        cancel: CancellationToken,
    ) -> RunStatus {
        let store = Arc::new(MemoryStore::default());
        let session = store.create_session("/tmp", "test-model").await.unwrap();
        let agent = Agent {
            model: Arc::new(model),
            tools,
            store: store.clone(),
            approver: Arc::new(AlwaysAllow),
            assembler: Arc::new(ContextAssembler::default()),
            max_turns: 3,
            system: "test".into(),
            write_lock,
            inbox: Inbox::default(),
            hooks: None,
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        agent
            .run(&session.meta.id, "hello", "/tmp", cancel, tx)
            .await
            .unwrap()
    }

    struct HoldTool {
        name: String,
        class: ToolClass,
        started: tokio::sync::watch::Sender<bool>,
        release: tokio::sync::watch::Receiver<bool>,
        probe: Probe,
    }

    #[async_trait::async_trait]
    impl Tool for HoldTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.clone(),
                description: self.name.clone(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            }
        }
        fn class(&self) -> ToolClass {
            self.class
        }
        async fn call(&self, _args: serde_json::Value, _cx: &ToolCx) -> ToolOutput {
            self.probe.enter();
            let _ = self.started.send(true);
            let mut release = self.release.clone();
            release.wait_for(|ready| *ready).await.expect("release");
            self.probe.leave();
            ToolOutput {
                text: self.name.clone(),
                is_error: false,
            }
        }
    }

    /// REQ-3.28 INV-14 shared cwd lock serializes write/edit/bash across agents.
    #[tokio::test]
    async fn req_3_28_inv14_shared_lock_serializes_write_across_agents() {
        let lock = CwdWriteLock::new();
        let probe = Probe::new();
        let (started_tx, mut started_rx) = tokio::sync::watch::channel(false);
        let (release_tx, release_rx) = tokio::sync::watch::channel(false);
        let parent = tokio::spawn(run_locked(
            scripted_one_call("write", "p1"),
            vec![Arc::new(HoldTool {
                name: "write".into(),
                class: ToolClass::Write,
                started: started_tx,
                release: release_rx,
                probe: probe.clone(),
            })],
            lock.clone(),
            CancellationToken::new(),
        ));
        started_rx
            .wait_for(|ready| *ready)
            .await
            .expect("parent write must start");
        let child = tokio::spawn(run_locked(
            scripted_one_call("bash", "c1"),
            vec![Arc::new(ProbeTool {
                name: "bash".into(),
                class: ToolClass::Exec,
                delay: Duration::from_millis(10),
                probe: probe.clone(),
                text: "b".into(),
            })],
            lock,
            CancellationToken::new(),
        ));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            probe.inflight.load(Ordering::SeqCst),
            1,
            "REQ-3.28 child bash must wait while parent write holds the cwd lock"
        );
        let _ = release_tx.send(true);
        let parent_status = tokio::time::timeout(Duration::from_secs(2), parent)
            .await
            .expect("parent write must finish")
            .unwrap();
        let child_status = tokio::time::timeout(Duration::from_secs(2), child)
            .await
            .expect("child bash must finish after the lock is released")
            .unwrap();
        assert_eq!(parent_status, RunStatus::Done);
        assert_eq!(child_status, RunStatus::Done);
        assert_eq!(probe.max.load(Ordering::SeqCst), 1);
        assert_eq!(probe.inflight.load(Ordering::SeqCst), 0);
    }

    /// REQ-3.28 INV-14 Read does not take the cwd write lock.
    #[tokio::test]
    async fn req_3_28_inv14_reads_do_not_take_cwd_lock() {
        let lock = CwdWriteLock::new();
        let (started_tx, mut started_rx) = tokio::sync::watch::channel(false);
        let (release_tx, release_rx) = tokio::sync::watch::channel(false);
        let writer = tokio::spawn(run_locked(
            scripted_one_call("write", "w1"),
            vec![Arc::new(HoldTool {
                name: "write".into(),
                class: ToolClass::Write,
                started: started_tx,
                release: release_rx,
                probe: Probe::new(),
            })],
            lock.clone(),
            CancellationToken::new(),
        ));
        started_rx
            .wait_for(|ready| *ready)
            .await
            .expect("write must start");
        let read_status = tokio::time::timeout(
            Duration::from_secs(2),
            run_locked(
                scripted_one_call("read", "r1"),
                vec![Arc::new(FakeTool::new("read", "ok"))],
                lock,
                CancellationToken::new(),
            ),
        )
        .await
        .expect("REQ-3.28 Read must finish while write holds the cwd lock");
        assert_eq!(read_status, RunStatus::Done);
        let _ = release_tx.send(true);
        let write_status = tokio::time::timeout(Duration::from_secs(2), writer)
            .await
            .expect("held write must finish after release")
            .unwrap();
        assert_eq!(write_status, RunStatus::Done);
    }

    /// REQ-3.28 INV-14 agents must share one lock object; separate locks do not serialize.
    #[tokio::test]
    async fn req_3_28_inv14_separate_locks_do_not_serialize() {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        struct BarrierWrite {
            name: String,
            barrier: Arc<tokio::sync::Barrier>,
        }
        #[async_trait::async_trait]
        impl Tool for BarrierWrite {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: self.name.clone(),
                    description: self.name.clone(),
                    parameters: serde_json::json!({"type":"object","properties":{}}),
                }
            }
            fn class(&self) -> ToolClass {
                ToolClass::Write
            }
            async fn call(&self, _args: serde_json::Value, _cx: &ToolCx) -> ToolOutput {
                self.barrier.wait().await;
                ToolOutput {
                    text: self.name.clone(),
                    is_error: false,
                }
            }
        }
        let ran = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(
                run_locked(
                    scripted_one_call("write", "a1"),
                    vec![Arc::new(BarrierWrite {
                        name: "write".into(),
                        barrier: barrier.clone(),
                    })],
                    CwdWriteLock::new(),
                    CancellationToken::new(),
                ),
                run_locked(
                    scripted_one_call("edit", "b1"),
                    vec![Arc::new(BarrierWrite {
                        name: "edit".into(),
                        barrier,
                    })],
                    CwdWriteLock::new(),
                    CancellationToken::new(),
                ),
            )
        })
        .await
        .expect("REQ-3.28 separate CwdWriteLock objects must not block each other");
        assert_eq!(ran.0, RunStatus::Done);
        assert_eq!(ran.1, RunStatus::Done);
    }

    struct StopAfterTool;

    #[async_trait::async_trait]
    impl Hooks for StopAfterTool {
        fn should_stop(&self) -> bool {
            true
        }
    }

    /// REQ-3.4 INV-18 should_stop ends the run after pairing tool results.
    #[tokio::test]
    async fn req_3_4_inv18_should_stop_pairs_then_ends() {
        let model = ScriptedModel::new(vec![
            vec![
                ModelEvent::ItemDone(item_call("echo", "c1")),
                ModelEvent::Completed {
                    usage: Default::default(),
                },
            ],
            vec![
                ModelEvent::ItemDone(item_text("should-not-run")),
                ModelEvent::Completed {
                    usage: Default::default(),
                },
            ],
        ]);
        let (status, ws) = run_with_hooks(
            model,
            vec![Arc::new(FakeTool::new("echo", "pong"))],
            Arc::new(AlwaysAllow),
            Arc::new(StopAfterTool),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(status, RunStatus::Done);
        let kinds: Vec<_> = ws.chats[0]
            .items
            .iter()
            .map(|i| match &i.kind {
                ItemKind::ToolCall { call_id, .. } => format!("call:{call_id}"),
                ItemKind::ToolResult { call_id, .. } => format!("result:{call_id}"),
                ItemKind::Text { text } => format!("text:{text}"),
                _ => "other".into(),
            })
            .collect();
        assert!(kinds.iter().any(|k| k == "call:c1"));
        assert!(kinds.iter().any(|k| k == "result:c1"));
        assert!(!kinds.iter().any(|k| k.contains("should-not-run")));
    }

    struct CountPrepare {
        n: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Hooks for CountPrepare {
        fn prepare_next_turn(&self, assembled: &mut Assembled) {
            self.n.fetch_add(1, Ordering::SeqCst);
            let _ = assembled;
        }
    }

    /// REQ-3.4 prepare_next_turn runs before each model call.
    #[tokio::test]
    async fn req_3_4_prepare_next_turn_before_model() {
        let n = Arc::new(AtomicUsize::new(0));
        let model = ScriptedModel::new(vec![vec![
            ModelEvent::ItemDone(item_text("ok")),
            ModelEvent::Completed {
                usage: Default::default(),
            },
        ]]);
        let (status, _) = run_with_hooks(
            model,
            vec![],
            Arc::new(AlwaysAllow),
            Arc::new(CountPrepare { n: n.clone() }),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(status, RunStatus::Done);
        assert_eq!(n.load(Ordering::SeqCst), 1);
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
        assert!(
            assembled
                .request
                .items
                .iter()
                .any(|i| matches!(i.kind, ItemKind::Notice { .. }))
        );
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

    fn session_with_finished_and_running(n_done: u32) -> WorkingSession {
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
        for i in 0..n_done {
            session.chats.push(Chat {
                id: format!("done-{i}"),
                session_id: "s".into(),
                seq: i + 1,
                title: None,
                status: ChatStatus::Done,
                input_tokens: 0,
                output_tokens: 0,
                created_at: 0,
                ended_at: None,
                items: vec![Item::text(Role::User, format!("old-{i}"))],
            });
        }
        session.chats.push(Chat {
            id: "running".into(),
            session_id: "s".into(),
            seq: n_done + 1,
            title: None,
            status: ChatStatus::Running,
            input_tokens: 0,
            output_tokens: 0,
            created_at: 0,
            ended_at: None,
            items: vec![Item::text(Role::User, "current-user")],
        });
        session
    }

    /// REQ-4.4 INV-5 extra_drop removes more finished chats and keeps the running chat.
    #[test]
    fn req_4_4_inv5_extra_drop_keeps_running() {
        let session = session_with_finished_and_running(2);
        let assembler = ContextAssembler::default();
        let first = assembler.assemble(&session, "sys".into(), vec![]);
        let retry = assembler.assemble_drop(&session, "sys".into(), vec![], 1);
        assert_eq!(first.dropped_chats, 0);
        assert_eq!(retry.dropped_chats, 1);
        assert!(
            retry
                .request
                .items
                .iter()
                .any(|i| matches!(&i.kind, ItemKind::Text { text } if text == "current-user"))
        );
        assert!(
            !retry
                .request
                .items
                .iter()
                .any(|i| matches!(&i.kind, ItemKind::Text { text } if text == "old-0"))
        );
        assert!(
            retry
                .request
                .items
                .iter()
                .any(|i| matches!(&i.kind, ItemKind::Notice { .. }))
        );
        assert!(session.chats.iter().all(|c| {
            !c.items
                .iter()
                .any(|i| matches!(i.kind, ItemKind::Notice { .. }))
        }));
    }
}
