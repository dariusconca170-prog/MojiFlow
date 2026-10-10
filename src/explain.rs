//! Sentence explanations via a local OpenAI-compatible LLM (llama.cpp server).
//!
//! Dictionary lookups fail on slang, names, and rare senses (user review 2026-10-10).
//! Instead of bundling a model, the app POSTs the sentence to the user's own server —
//! by default the llama.cpp server at `127.0.0.1:8080` — and shows the answer in the
//! popover. Empty endpoint disables the button. Runs off the UI thread; results come
//! back as `CoreEvent::Explanation`.

use std::time::Duration;

use serde_json::{json, Value};

use crate::config::ExplainConfig;

const SYSTEM_PROMPT: &str = "You are a Japanese tutor for an English-speaking learner. \
    Explain the meaning and grammar of the given sentence simply, in English, \
    in under 120 words. Pay special attention to the focused word.";

#[derive(Debug, thiserror::Error)]
pub enum ExplainError {
    #[error("explain request failed: {0}")]
    Http(String),
    #[error("explain timed out after {0}s")]
    Timeout(u64),
    #[error("explain got an unreadable answer: {0}")]
    BadAnswer(String),
}

/// Everything one explanation request needs.
#[derive(Debug, Clone)]
pub struct ExplainRequest {
    pub endpoint: String,
    pub model: String,
    pub sentence: String,
    pub focus: String,
    pub max_tokens: u32,
    pub timeout_s: u64,
}

impl ExplainRequest {
    pub fn from_config(config: &ExplainConfig, sentence: String, focus: String) -> Option<Self> {
        if config.endpoint.trim().is_empty() {
            return None;
        }
        Some(Self {
            endpoint: config.endpoint.clone(),
            model: config.model.clone(),
            sentence,
            focus,
            max_tokens: config.max_tokens,
            timeout_s: config.timeout_s.max(1),
        })
    }

    fn user_prompt(&self) -> String {
        format!("Sentence: {}\nFocused word: {}", self.sentence, self.focus)
    }
}

/// The exact POST body (pure for tests).
pub fn request_body(req: &ExplainRequest) -> Value {
    json!({
        "model": req.model,
        "messages": [
            { "role": "system", "content": SYSTEM_PROMPT },
            { "role": "user", "content": req.user_prompt() },
        ],
        "max_tokens": req.max_tokens,
        "temperature": 0.3,
    })
}

/// Pull `choices[0].message.content` out of a chat-completions body (pure for tests).
pub fn parse_answer(body: &Value) -> Result<String, ExplainError> {
    let text = body
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or_default();
    if text.is_empty() {
        return Err(ExplainError::BadAnswer(
            format!("{body}").chars().take(200).collect(),
        ));
    }
    Ok(text.to_owned())
}

/// Blocking fetch: builds a current-thread runtime and POSTs once. Call off the UI.
pub fn fetch(req: &ExplainRequest) -> Result<String, ExplainError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| ExplainError::Http(err.to_string()))?;
    runtime.block_on(fetch_async(req))
}

async fn fetch_async(req: &ExplainRequest) -> Result<String, ExplainError> {
    let timeout = Duration::from_secs(req.timeout_s);
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout)
        .build()
        .map_err(|err| ExplainError::Http(err.to_string()))?;
    let response = client
        .post(&req.endpoint)
        .json(&request_body(req))
        .send()
        .await
        .map_err(|err| {
            if err.is_timeout() {
                ExplainError::Timeout(req.timeout_s)
            } else {
                ExplainError::Http(err.to_string())
            }
        })?;
    let body: Value = response
        .json()
        .await
        .map_err(|err| ExplainError::BadAnswer(err.to_string()))?;
    parse_answer(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ExplainRequest {
        ExplainRequest {
            endpoint: "http://127.0.0.1:8080/v1/chat/completions".to_owned(),
            model: String::new(),
            sentence: "こんにちは、世界！".to_owned(),
            focus: "世界 (せかい)".to_owned(),
            max_tokens: 256,
            timeout_s: 30,
        }
    }

    #[test]
    fn request_body_carries_system_sentence_and_focus() {
        let body = request_body(&request());
        assert_eq!(body["model"], json!(""));
        assert_eq!(body["max_tokens"], json!(256));
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert!(messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("Japanese tutor"));
        let user = messages[1]["content"].as_str().unwrap();
        assert!(user.contains("こんにちは、世界！"), "{user}");
        assert!(user.contains("世界 (せかい)"), "{user}");
    }

    #[test]
    fn parse_answer_reads_first_choice_content() {
        let body = json!({
            "choices": [{ "message": { "content": "  世界 means world.  " } }],
        });
        assert_eq!(parse_answer(&body).unwrap(), "世界 means world.");
        assert!(parse_answer(&json!({ "choices": [] })).is_err());
        assert!(parse_answer(&json!({})).is_err());
    }

    #[test]
    fn empty_endpoint_disables_requests() {
        let mut config = ExplainConfig::default();
        config.endpoint.clear();
        assert!(ExplainRequest::from_config(&config, "文".to_owned(), "文".to_owned()).is_none());
    }
}
