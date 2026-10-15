//! End-to-end test of *sentence* mining: a real subtitle file, the app's own gather step,
//! the real export worker, a mock AnkiConnect and a mock OpenAI-compatible LLM server.
//!
//! This is the whole user-facing path for `Ctrl+Alt+Shift+S` / the dashboard's "Export whole
//! line" button: cue → card → translation → note in Anki. It asserts on what Anki actually
//! received, so it fails if the line, its kana reading or the translation stop arriving.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use medialingual_native::app::App;
use medialingual_native::config::Config;
use medialingual_native::subs::load_path;
use serde_json::{json, Value};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LINE: &str = "私は学生です。";

async fn mount_anki(server: &MockServer) {
    // The model already exists, so `ensure_model` must not try to create it.
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_partial_json(json!({ "action": "modelNames" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": ["Japanese Mining (medialingual)"],
            "error": null,
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_partial_json(json!({ "action": "createDeck" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": 1,
            "error": null,
        })))
        .mount(server)
        .await;
    // Media: a screenshot may or may not be available (headless vs. a live desktop), so the
    // card's `Image`/`Audio` fields are not asserted — but the upload must be answerable, or
    // the export stops before `addNote` and the note is queued instead of delivered.
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_partial_json(json!({ "action": "storeMediaFile" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "stored",
            "error": null,
        })))
        .mount(server)
        .await;
}

/// Accept `addNote` and hand back a note id. The *contents* are asserted from the recorded
/// request instead of a matcher (the note carries media filenames with a random nonce, and a
/// matcher that silently fails to match would look like "Anki never answered").
async fn mount_add_note(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_partial_json(json!({ "action": "addNote" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": 42,
            "error": null,
        })))
        .mount(server)
        .await;
}

async fn mount_llm(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{ "message": { "content": "I am a student." } }],
        })))
        .mount(server)
        .await;
}

fn write_subtitle(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("ep.srt");
    std::fs::write(
        &path,
        format!("1\n00:00:01,000 --> 00:00:03,000\n{LINE}\n\n2\n00:00:09,000 --> 00:00:11,000\nこれは本です。\n"),
    )
    .expect("write srt");
    path
}

fn app_for(server_uri: &str, subtitle: &std::path::Path, dir: &std::path::Path) -> App {
    let mut config = Config::default();
    config.anki.url = server_uri.to_owned();
    config.explain.endpoint = format!("{server_uri}/v1/chat/completions");
    // Background queue retries stay off: this test asserts on one export, and a stray tick
    // hitting the mock would only add noise.
    config.anki.retry_interval_s = 0;
    let mut app = App::new(
        config,
        // A config path inside the test's own temp dir, so its offline queue (used when an
        // export fails) can never mix with another test's or a real session's.
        Some(dir.join("config.toml")),
        Vec::new(),
        medialingual_native::gui::fonts::FontSource::Embedded,
        egui::Context::default(),
    );
    app.track = Some(load_path(subtitle).expect("load subtitle track"));
    app.active_cue = Some(0);
    app
}

/// Compact, human-readable summary of what the server saw — request bodies hold base64
/// media, so never print them raw in a failure message.
async fn seen(server: &MockServer) -> String {
    received_actions(server)
        .await
        .iter()
        .map(|body| {
            body["action"]
                .as_str()
                .unwrap_or("chat-completions")
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join(", ")
}

async fn received_actions(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
        .collect()
}

async fn wait_for_add_note(server: &MockServer) -> Option<Value> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        for body in received_actions(server).await {
            if body["action"] == json!("addNote") {
                return Some(body);
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

/// The happy path: one hotkey, and a sentence card with a translation lands in Anki.
#[tokio::test]
async fn exporting_the_whole_line_creates_a_translated_sentence_card() {
    let server = MockServer::start().await;
    mount_anki(&server).await;
    mount_add_note(&server).await;
    mount_llm(&server).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let subtitle = write_subtitle(dir.path());
    let mut app = app_for(&server.uri(), &subtitle, dir.path());

    assert!(
        app.export_current_sentence(),
        "the export must be accepted by the worker"
    );

    let note = match wait_for_add_note(&server).await {
        Some(note) => note,
        None => panic!(
            "Anki must receive the sentence card; server saw [{}], queue: {:?}",
            seen(&server).await,
            app.queue_status()
                .rows
                .iter()
                .map(|row| row.preview.clone())
                .collect::<Vec<_>>()
        ),
    };
    let fields = &note["params"]["note"]["fields"];
    assert_eq!(fields["Expression"], json!(LINE), "got {fields}");
    assert_eq!(fields["Reading"], json!("わたしはがくせいです。"));
    assert_eq!(fields["Glossary"], json!("I am a student."));
    assert_eq!(fields["Sentence"], json!(LINE));
    assert_eq!(
        note["params"]["note"]["deckName"],
        json!("Japanese::Mining")
    );

    // The translation really came from the LLM endpoint: the ask must have gone out.
    let actions = received_actions(&server).await;
    let asked = actions
        .iter()
        .any(|body| body["messages"][1]["content"] == json!(format!("Sentence: {LINE}")));
    assert!(asked, "the local LLM must be asked for a translation");
    app.shutdown_workers();
}

/// A word card must not call the LLM at all — its glossary is the dictionary's business.
#[tokio::test]
async fn exporting_a_word_does_not_ask_the_llm() {
    let server = MockServer::start().await;
    mount_anki(&server).await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_partial_json(json!({ "action": "addNote" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": 7,
            "error": null,
        })))
        .mount(&server)
        .await;
    mount_llm(&server).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let subtitle = write_subtitle(dir.path());
    let mut app = app_for(&server.uri(), &subtitle, dir.path());

    assert!(app.export_current_card(), "the word export is accepted");
    let note = match wait_for_add_note(&server).await {
        Some(note) => note,
        None => panic!(
            "the word card must still reach Anki; server saw [{}], queue: {:?}",
            seen(&server).await,
            app.queue_status()
                .rows
                .iter()
                .map(|row| row.preview.clone())
                .collect::<Vec<_>>()
        ),
    };
    // The word card is the *hovered* token — with nothing hovered, the first content word of
    // the line. Its glossary comes from the local dictionary (which may or may not be built
    // on this machine), so only the expression is asserted here; the point of this test is
    // that the LLM is never asked.
    assert_eq!(note["params"]["note"]["fields"]["Expression"], json!("私"));

    let asked = received_actions(&server)
        .await
        .iter()
        .any(|body| body.get("messages").is_some());
    assert!(!asked, "a word card must not POST to the chat endpoint");
    app.shutdown_workers();
}

/// Nothing mineable must be a toast, not a panic or a silent no-op — no cue is active while
/// the video is paused between lines.
#[tokio::test]
async fn exporting_without_an_active_cue_reports_instead_of_failing() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let subtitle = write_subtitle(dir.path());
    let mut app = app_for(&server.uri(), &subtitle, dir.path());
    app.active_cue = None;

    assert!(!app.export_current_sentence(), "nothing is exported");
    assert!(
        app.toasts
            .iter()
            .any(|toast| toast.message.contains("no active cue")),
        "the user must be told why"
    );
    app.shutdown_workers();
}
