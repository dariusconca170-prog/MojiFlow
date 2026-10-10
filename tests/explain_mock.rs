//! Mock-server test for the local-LLM explain fetch: a real HTTP round trip against
//! wiremock (request shape itself is unit-tested in `src/explain.rs`).

use medialingual_native::explain::{fetch, ExplainRequest};
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
    };
    let result = tokio::task::spawn_blocking(move || fetch(&req))
        .await
        .expect("thread");
    assert!(result.is_err());
}
