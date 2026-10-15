//! Mock-server test for the local-LLM explain fetch: a real HTTP round trip against
//! wiremock (request shape itself is unit-tested in `src/explain.rs`).

use medialingual_native::explain::{fetch, ExplainRequest, ExplainTask};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn explain_posts_chat_completions_and_returns_content() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{ "message": { "content": "世界 means 'world'." } }],
        })))
        .mount(&server)
        .await;
    let req = ExplainRequest {
        endpoint: format!("{}/v1/chat/completions", server.uri()),
        model: "test-model".to_owned(),
        sentence: "こんにちは、世界！".to_owned(),
        focus: "世界 (せかい)".to_owned(),
        max_tokens: 64,
        timeout_s: 5,
        task: ExplainTask::Explain,
    };
    let answer = tokio::task::spawn_blocking(move || fetch(&req))
        .await
        .expect("thread")
        .expect("answer");
    assert_eq!(answer, "世界 means 'world'.");
}

#[tokio::test]
async fn explain_surfaces_server_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [],
        })))
        .mount(&server)
        .await;
    let req = ExplainRequest {
        endpoint: format!("{}/v1/chat/completions", server.uri()),
        model: String::new(),
        sentence: "文".to_owned(),
        focus: "文".to_owned(),
        max_tokens: 64,
        timeout_s: 5,
        task: ExplainTask::Explain,
    };
    let result = tokio::task::spawn_blocking(move || fetch(&req))
        .await
        .expect("thread");
    assert!(result.is_err());
}

/// The sentence-card path: same endpoint, but the system prompt must ask for a translation
/// (a tutor-style explanation in a flashcard's glossary field would be wrong). The request is
/// asserted from what the server *received*, not from a matcher's guess about it.
#[tokio::test]
async fn translation_posts_its_own_system_prompt() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{ "message": { "content": "I am a student." } }],
        })))
        .mount(&server)
        .await;

    let config = medialingual_native::config::ExplainConfig {
        endpoint: format!("{}/v1/chat/completions", server.uri()),
        ..Default::default()
    };
    let req = ExplainRequest::translation(&config, "私は学生です。".to_owned()).expect("request");
    let answer = tokio::task::spawn_blocking(move || fetch(&req))
        .await
        .expect("thread")
        .expect("answer");
    assert_eq!(answer, "I am a student.");

    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 1, "exactly one call");
    let body: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("request body is JSON");
    let messages = body["messages"].as_array().expect("messages");
    let system = messages[0]["content"].as_str().expect("system prompt");
    assert!(
        system.to_lowercase().contains("translate"),
        "the translation task must reach the server: {system}"
    );
    assert!(
        !system.contains("tutor"),
        "the explain prompt must not be reused: {system}"
    );
    assert_eq!(messages[1]["content"], json!("Sentence: 私は学生です。"));
}
