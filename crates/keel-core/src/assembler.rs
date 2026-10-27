use crate::traits::ContextSource;
use crate::types::{Assembled, ChatStatus, Item, ItemKind, ModelRequest, Role, ToolSpec, WorkingSession};

pub struct ContextAssembler {
    pub context_limit: u32,
}

impl Default for ContextAssembler {
    fn default() -> Self {
        Self {
            context_limit: 200_000,
        }
    }
}

fn estimate_tokens(items: &[Item]) -> u32 {
    items
        .iter()
        .map(|item| match &item.kind {
            ItemKind::Text { text } | ItemKind::Notice { text } | ItemKind::ToolResult { output: text, .. } => {
                (text.len() as u32 / 4).max(1)
            }
            ItemKind::ToolCall { args, .. } => (args.to_string().len() as u32 / 4).max(1),
            _ => 8,
        })
        .sum()
}

impl ContextSource for ContextAssembler {
    fn assemble(&self, session: &WorkingSession, system: String, tools: Vec<ToolSpec>) -> Assembled {
        let budget = (self.context_limit as f32 * 0.8) as u32;
        let mut kept: Vec<&crate::types::Chat> = Vec::new();
        let mut dropped = 0usize;
        for chat in session.chats.iter().rev() {
            if chat.status == ChatStatus::Running {
                kept.push(chat);
                continue;
            }
            let mut trial = kept.clone();
            trial.push(chat);
            let tokens: u32 = trial.iter().map(|c| estimate_tokens(&c.items)).sum();
            if tokens > budget && !kept.is_empty() {
                dropped += 1;
                continue;
            }
            kept.push(chat);
        }
        kept.reverse();
        let mut items = Vec::new();
        if dropped > 0 {
            items.push(Item::notice(format!("이전 {dropped}개 chat 생략")));
        }
        for chat in kept {
            items.extend(chat.items.iter().cloned());
        }
        Assembled {
            request: ModelRequest {
                system,
                items,
                tools,
            },
            dropped_chats: dropped,
        }
    }
}

pub fn text_of(item: &Item) -> Option<&str> {
    match &item.kind {
        ItemKind::Text { text } => Some(text),
        _ => None,
    }
}

pub fn is_user_text(item: &Item) -> bool {
    item.role == Role::User && matches!(item.kind, ItemKind::Text { .. })
}
