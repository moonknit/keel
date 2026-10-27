//! LLM settings from the process environment. The host loads `.env` first.

use crate::error::{KeelError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmSettings {
    pub model: String,
    pub base_url: String,
    pub reasoning_effort: String,
    pub max_tokens: u32,
    pub temperature: String,
}

impl LlmSettings {
    /// REQ-3.8: CLI flags later overlay this. Env is the process source of truth.
    pub fn from_env() -> Result<Self> {
        let model = std::env::var("LLM_MODEL")
            .map_err(|_| KeelError::Msg("LLM_MODEL is required".into()))?;
        Ok(Self {
            model,
            base_url: std::env::var("LLM_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".into()),
            reasoning_effort: std::env::var("LLM_REASONING_EFFORT")
                .unwrap_or_else(|_| "low".into()),
            max_tokens: std::env::var("LLM_MAX_TOKENS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(8192),
            temperature: std::env::var("LLM_TEMPERATURE").unwrap_or_else(|_| "0.0".into()),
        })
    }

    pub fn api_key() -> Result<String> {
        std::env::var("OPENAI_API_KEY")
            .map_err(|_| KeelError::Msg("OPENAI_API_KEY is required".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// REQ-3.8 reads model and defaults from the environment.
    #[test]
    fn req_3_8_from_env_defaults() {
        // Isolated by overwriting and restoring.
        let keys = [
            "LLM_MODEL",
            "LLM_BASE_URL",
            "LLM_REASONING_EFFORT",
            "LLM_MAX_TOKENS",
            "LLM_TEMPERATURE",
        ];
        let saved: Vec<(String, Option<String>)> = keys
            .iter()
            .map(|k| ((*k).to_string(), std::env::var(k).ok()))
            .collect();
        unsafe {
            std::env::set_var("LLM_MODEL", "gpt-6-luna");
            std::env::remove_var("LLM_BASE_URL");
            std::env::remove_var("LLM_REASONING_EFFORT");
            std::env::remove_var("LLM_MAX_TOKENS");
            std::env::remove_var("LLM_TEMPERATURE");
        }
        let s = LlmSettings::from_env().expect("settings");
        assert_eq!(s.model, "gpt-6-luna");
        assert_eq!(s.base_url, "https://api.openai.com/v1");
        assert_eq!(s.reasoning_effort, "low");
        assert_eq!(s.max_tokens, 8192);
        assert_eq!(s.temperature, "0.0");
        for (k, v) in saved {
            unsafe {
                match v {
                    Some(val) => std::env::set_var(&k, val),
                    None => std::env::remove_var(&k),
                }
            }
        }
    }
}
