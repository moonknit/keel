//! Previous tool output for `/last` (REQ-3.22).
//! Kept out of the rustyline loop so tests can drive Store without a TTY.

use keel_core::{ItemKind, WorkingSession};

/// Full body of the most recent `ToolResult` in the session, newest chat last.
pub fn last_tool_output(session: &WorkingSession) -> Option<&str> {
    session
        .chats
        .iter()
        .rev()
        .flat_map(|c| c.items.iter().rev())
        .find_map(|item| match &item.kind {
            ItemKind::ToolResult { output, .. } => Some(output.as_str()),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_core::traits::Store;
    use keel_core::{Chat, ChatStatus, Item, Role, SessionMeta, WorkingSession};
    use keel_store::SqliteStore;

    fn empty_meta() -> SessionMeta {
        SessionMeta {
            id: "s".into(),
            title: None,
            cwd: "/ws".into(),
            model: "m".into(),
            lock_pid: None,
            created_at: 0,
            updated_at: 0,
            archived: false,
        }
    }

    fn chat(id: &str, seq: u32, items: Vec<Item>) -> Chat {
        Chat {
            id: id.into(),
            session_id: "s".into(),
            seq,
            title: None,
            status: ChatStatus::Done,
            input_tokens: 0,
            output_tokens: 0,
            created_at: 0,
            ended_at: None,
            items,
        }
    }

    fn session(chats: Vec<Chat>) -> WorkingSession {
        WorkingSession {
            meta: empty_meta(),
            chats,
            read_roots: Vec::new(),
            allow: Vec::new(),
        }
    }

    /// REQ-3.22 `/last` is the previous tool body, not a one-line summary.
    #[test]
    fn req_3_22_last_is_full_tool_output_not_summary() {
        let body = "hello\nworld\nmore lines";
        let ws = session(vec![chat(
            "c1",
            1,
            vec![
                Item::text(Role::User, "run"),
                Item::tool_result("call-1", body, false),
                Item::text(Role::Assistant, "done"),
            ],
        )]);
        let got = last_tool_output(&ws).expect("tool result");
        assert_eq!(got, body);
        assert!(got.contains('\n'));
        assert!(
            !got.contains("ok=") && !got.contains("ms "),
            "must not be the ToolEnd one-line summary: {got}"
        );
    }

    /// REQ-3.22 no tool_result yet means `/last` stays empty (caller keeps prior).
    #[test]
    fn req_3_22_last_none_without_tool_result() {
        let ws = session(vec![chat(
            "c1",
            1,
            vec![Item::text(Role::Assistant, "just text")],
        )]);
        assert_eq!(last_tool_output(&ws), None);
    }

    /// REQ-3.22 a stored tool_result is recovered as the full body after write-through.
    #[tokio::test]
    async fn req_3_22_last_reads_stored_tool_result_body() {
        let store = SqliteStore::open_memory().unwrap();
        let ws = store.create_session("/ws", "m").await.unwrap();
        let first = store.begin_chat(&ws.meta.id, Some("one")).await.unwrap();
        store
            .append(&first.id, Item::tool_result("c1", "old\nbody", false))
            .await
            .unwrap();
        store
            .end_chat(&first.id, ChatStatus::Done, Default::default())
            .await
            .unwrap();

        let second = store.begin_chat(&ws.meta.id, Some("two")).await.unwrap();
        let body = "line1\nline2\nline3";
        store
            .append(&second.id, Item::tool_result("c2", body, false))
            .await
            .unwrap();
        store
            .append(&second.id, Item::text(Role::Assistant, "ok"))
            .await
            .unwrap();

        let opened = store.open_session(&ws.meta.id).await.unwrap();
        assert_eq!(last_tool_output(&opened), Some(body));
    }
}
