//! End-to-end tests of the AnkiConnect client against a local wiremock server.
//!
//! These exercise the real HTTP transport, request shapes (action/version/params),
//! response parsing, duplicate detection and the offline-queue drain path. They are the
//! automated half of the M6 gate; the "real card in live Anki" half is manual QA.

use medialingual_native::anki::connect::AnkiConnect;
use medialingual_native::anki::media::MediaFile;
use medialingual_native::anki::note::PendingNote;
use medialingual_native::anki::queue::OfflineQueue;
use medialingual_native::app::{CoreEvent, RepaintHandle};
use medialingual_native::config::AnkiConfig;
use medialingual_native::error::AnkiError;
use medialingual_native::export::{ExportWorker, RetrySchedule};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config(url: String) -> AnkiConfig {
    AnkiConfig {
        url,
        connect_timeout_ms: 2_000,
        retry_interval_s: 10,
        ..AnkiConfig::default()
    }
}

/// Respond to every POST with a fixed result, letting the matcher assert the request body.
async fn mount_ok(server: &MockServer, action_body: Value, result: Value) {
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_json(action_body))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": result,
            "error": null,
        })))
        .mount(server)
        .await;
}

fn add_note_body(note_id_action_overrides: Value) -> Value {
    // The exact payload the client sends for `addNote` with the default config.
    json!({
        "action": "addNote",
        "version": 6,
        "params": {
            "note": {
                "deckName": "Japanese::Mining",
                "modelName": "Japanese Mining",
                "fields": { "Expression": "食べる" },
                "options": { "allowDuplicate": false, "duplicateScope": "deck" },
                "tags": ["medialingual"],
            }
        }
    })
    .merge(note_id_action_overrides)
}

trait MergeExt {
    fn merge(self, other: Value) -> Value;
}

impl MergeExt for Value {
    fn merge(mut self, other: Value) -> Value {
        let right = other.as_object().expect("object");
        for (key, value) in right {
            self.as_object_mut()
                .expect("object")
                .insert(key.clone(), value.clone());
        }
        self
    }
}

#[tokio::test]
async fn add_note_success_returns_the_note_id() {
    let server = MockServer::start().await;
    mount_ok(&server, add_note_body(json!({})), json!(123456)).await;
    let client = AnkiConnect::new(&config(server.uri())).expect("client");
    let fields = BTreeMap::from([("Expression".to_owned(), "食べる".to_owned())]);
    let id = client
        .add_note(
            "Japanese::Mining",
            "Japanese Mining",
            &fields,
            &["medialingual".to_owned()],
            false,
            "deck",
        )
        .await
        .expect("add note");
    assert_eq!(id, 123456);
}

#[tokio::test]
async fn duplicate_rejection_maps_to_duplicate_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": null,
            "error": "cannot create note because it is a duplicate",
        })))
        .mount(&server)
        .await;
    let client = AnkiConnect::new(&config(server.uri())).expect("client");
    let fields = BTreeMap::from([("Expression".to_owned(), "食べる".to_owned())]);
    let err = client
        .add_note(
            "Japanese::Mining",
            "Japanese Mining",
            &fields,
            &["medialingual".to_owned()],
            false,
            "deck",
        )
        .await
        .expect_err("should be duplicate");
    match err {
        AnkiError::Duplicate(scope) => assert_eq!(scope, "deck"),
        other => panic!("expected Duplicate, got {other:?}"),
    }
}

#[tokio::test]
async fn remote_error_passes_through() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": null,
            "error": "model not found",
        })))
        .mount(&server)
        .await;
    let client = AnkiConnect::new(&config(server.uri())).expect("client");
    let fields = BTreeMap::from([("Expression".to_owned(), "x".to_owned())]);
    let err = client
        .add_note("Default", "Nope", &fields, &[], false, "none")
        .await
        .expect_err("should fail");
    match err {
        AnkiError::Remote { code, message } => {
            assert_eq!(code, 6);
            assert_eq!(message, "model not found");
        }
        other => panic!("expected Remote, got {other:?}"),
    }
}

#[tokio::test]
async fn unreachable_endpoint_maps_to_unreachable() {
    // Port 1 is not listening; the short connect timeout keeps this fast.
    let client = AnkiConnect::new(&config("http://127.0.0.1:1".to_owned())).expect("client");
    let fields = BTreeMap::from([("Expression".to_owned(), "x".to_owned())]);
    let err = client
        .add_note("Default", "Nope", &fields, &[], false, "none")
        .await
        .expect_err("should fail");
    match err {
        AnkiError::Unreachable { url, .. } => assert_eq!(url, "http://127.0.0.1:1"),
        other => panic!("expected Unreachable, got {other:?}"),
    }
}

#[tokio::test]
async fn malformed_body_maps_to_malformed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;
    let client = AnkiConnect::new(&config(server.uri())).expect("client");
    let err = client.model_names().await.expect_err("should fail");
    assert!(matches!(err, AnkiError::Malformed(_)), "got {err:?}");
}

#[tokio::test]
async fn model_names_parses_the_list() {
    let server = MockServer::start().await;
    mount_ok(
        &server,
        json!({ "action": "modelNames", "version": 6, "params": {} }),
        json!(["Basic", "Japanese Mining"]),
    )
    .await;
    let client = AnkiConnect::new(&config(server.uri())).expect("client");
    assert_eq!(
        client.model_names().await.expect("names"),
        ["Basic", "Japanese Mining"]
    );
}

#[tokio::test]
async fn ensure_model_skips_creation_when_model_exists() {
    let server = MockServer::start().await;
    mount_ok(
        &server,
        json!({ "action": "modelNames", "version": 6, "params": {} }),
        json!(["Japanese Mining"]),
    )
    .await;
    let client = AnkiConnect::new(&config(server.uri())).expect("client");
    let fields = vec!["Expression".to_owned(), "Reading".to_owned()];
    client
        .ensure_model("Japanese Mining", &fields)
        .await
        .expect("already exists is fine");
}

#[tokio::test]
async fn create_deck_returns_the_deck_id() {
    let server = MockServer::start().await;
    mount_ok(
        &server,
        json!({ "action": "createDeck", "version": 6, "params": { "deck": "Japanese::Mining" } }),
        json!(1791636127086_i64),
    )
    .await;
    let client = AnkiConnect::new(&config(server.uri())).expect("client");
    assert_eq!(
        client
            .create_deck("Japanese::Mining")
            .await
            .expect("deck id"),
        1791636127086
    );
}

#[tokio::test]
async fn ensure_model_creates_missing_model_with_card_templates() {
    let server = MockServer::start().await;
    mount_ok(
        &server,
        json!({ "action": "modelNames", "version": 6, "params": {} }),
        json!([]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_json(json!({
            "action": "createModel",
            "version": 6,
            "params": {
                "modelName": "Japanese Mining",
                "inOrderFields": ["Expression", "Reading"],
                "cardTemplates": [{
                    "Name": "Japanese Mining Card 1",
                    "Front": "{{Expression}}",
                    "Back": "{{Expression}}<br>{{Reading}}<br>",
                }],
                "isCloze": false,
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": 1493925591393_i64,
            "error": null,
        })))
        .mount(&server)
        .await;
    let client = AnkiConnect::new(&config(server.uri())).expect("client");
    let fields = vec!["Expression".to_owned(), "Reading".to_owned()];
    client
        .ensure_model("Japanese Mining", &fields)
        .await
        .expect("created");
}

#[tokio::test]
async fn store_media_file_posts_base64_and_filename() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_json(json!({
            "action": "storeMediaFile",
            "version": 6,
            "params": { "filename": "clip.mp3", "data": "AQID" },
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "clip.mp3",
            "error": null,
        })))
        .mount(&server)
        .await;
    let client = AnkiConnect::new(&config(server.uri())).expect("client");
    client
        .store_media_file("clip.mp3", "AQID")
        .await
        .expect("stored");
}

#[tokio::test]
async fn offline_queue_pushes_rendered_notes_and_drains_into_anki() {
    let dir = tempfile::tempdir().expect("tempdir");
    let queue = OfflineQueue::new(dir.path().to_owned());

    let note = PendingNote {
        deck: "Japanese::Mining".to_owned(),
        model: "Japanese Mining".to_owned(),
        tags: vec!["medialingual".to_owned()],
        fields: BTreeMap::from([("Expression".to_owned(), "食べる".to_owned())]),
        media: vec![MediaFile::from_bytes("clip.mp3".to_owned(), vec![1, 2, 3])],
    };
    queue.push(&note).expect("push");
    assert_eq!(queue.pending_count(), 1);

    let server = MockServer::start().await;
    let received_files = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_json(json!({
            "action": "storeMediaFile",
            "version": 6,
            "params": { "filename": "clip.mp3", "data": "AQID" },
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "clip.mp3",
            "error": null,
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_json(add_note_body(json!({}))))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": 42,
            "error": null,
        })))
        .mount(&server)
        .await;

    let uri = server.uri();
    let exported = queue
        .drain(|pending| {
            let uri = uri.clone();
            let received_files = received_files.clone();
            async move {
                let client = AnkiConnect::new(&config(uri)).expect("client");
                for file in &pending.media {
                    received_files
                        .lock()
                        .expect("lock")
                        .push(file.filename.clone());
                    client
                        .store_media_file(&file.filename, &file.data_base64)
                        .await?;
                }
                client
                    .add_note(
                        &pending.deck,
                        &pending.model,
                        &pending.fields,
                        &pending.tags,
                        false,
                        "deck",
                    )
                    .await
                    .map(|_| ())
            }
        })
        .await
        .expect("drain");
    assert_eq!(exported, 1);
    assert_eq!(*received_files.lock().expect("lock"), vec!["clip.mp3"]);
    assert_eq!(queue.pending_count(), 0);
}

/// `anki.retry_interval_s` used to be inert: a note queued while Anki was down only left
/// after *another* successful export. The worker must now drain it on its own cadence —
/// here with no export of its own at all (the case of notes left by a previous session).
#[tokio::test]
async fn export_worker_retries_the_offline_queue_on_its_own() {
    let dir = tempfile::tempdir().expect("tempdir");
    let queue = OfflineQueue::new(dir.path().to_owned());
    queue
        .push(&PendingNote {
            deck: "Japanese::Mining".to_owned(),
            model: "Japanese Mining".to_owned(),
            tags: vec!["medialingual".to_owned()],
            fields: BTreeMap::from([("Expression".to_owned(), "食べる".to_owned())]),
            media: vec![],
        })
        .expect("push");
    assert_eq!(queue.pending_count(), 1);

    let server = MockServer::start().await;
    // The full `export_note` path runs: model check, deck creation, then the note.
    mount_ok(
        &server,
        json!({ "action": "modelNames", "version": 6, "params": {} }),
        json!(["Japanese Mining"]),
    )
    .await;
    mount_ok(
        &server,
        json!({ "action": "createDeck", "version": 6, "params": { "deck": "Japanese::Mining" } }),
        json!(1),
    )
    .await;
    mount_ok(&server, add_note_body(json!({})), json!(42)).await;

    // The config model must match both the queued note and the modelNames result, or
    // ensure_model would try to create a model and the drain would never reach addNote.
    let mut anki = config(server.uri());
    anki.model = "Japanese Mining".to_owned();
    anki.retry_interval_s = 1;
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut worker = ExportWorker::spawn(
        tx,
        Arc::new(RepaintHandle::new(egui::Context::default())),
        RetrySchedule {
            queue_dir: dir.path().to_owned(),
            anki,
        },
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    while queue.pending_count() > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    worker.stop();
    worker.join();

    assert_eq!(
        queue.pending_count(),
        0,
        "the worker must drain queued notes without a fresh export"
    );
    let mut reported = false;
    while let Ok(event) = rx.try_recv() {
        if let CoreEvent::Status { text, .. } = event {
            if text.contains("drained 1 queued note") {
                reported = true;
            }
        }
    }
    assert!(reported, "the drain must be reported to the UI");
}

/// The dashboard's "Retry now" button: an explicit drain must work even with background
/// retries switched *off*, which is what makes it different from waiting for the next tick.
#[tokio::test]
async fn export_worker_drains_on_an_explicit_request_with_retries_disabled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let queue = OfflineQueue::new(dir.path().to_owned());
    queue
        .push(&PendingNote {
            deck: "Japanese::Mining".to_owned(),
            model: "Japanese Mining".to_owned(),
            tags: vec!["medialingual".to_owned()],
            fields: BTreeMap::from([("Expression".to_owned(), "食べる".to_owned())]),
            media: vec![],
        })
        .expect("push");

    let server = MockServer::start().await;
    mount_ok(
        &server,
        json!({ "action": "modelNames", "version": 6, "params": {} }),
        json!(["Japanese Mining"]),
    )
    .await;
    mount_ok(
        &server,
        json!({ "action": "createDeck", "version": 6, "params": { "deck": "Japanese::Mining" } }),
        json!(1),
    )
    .await;
    mount_ok(&server, add_note_body(json!({})), json!(42)).await;

    let mut anki = config(server.uri());
    anki.model = "Japanese Mining".to_owned();
    // Background retries off: only the explicit request can drain the queue.
    anki.retry_interval_s = 0;
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut worker = ExportWorker::spawn(
        tx,
        Arc::new(RepaintHandle::new(egui::Context::default())),
        RetrySchedule {
            queue_dir: dir.path().to_owned(),
            anki,
        },
    );

    // Give the worker a moment to be idle, then ask it to drain.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(worker.retry_now(), "the worker accepts a retry request");

    let deadline = Instant::now() + Duration::from_secs(10);
    while queue.pending_count() > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    worker.stop();
    worker.join();

    assert_eq!(queue.pending_count(), 0, "Retry now must drain the queue");
    let mut reported = false;
    while let Ok(event) = rx.try_recv() {
        if let CoreEvent::Status { text, .. } = event {
            if text.contains("drained 1 queued note") {
                reported = true;
            }
        }
    }
    assert!(reported, "the explicit retry must report what it did");

    // And with an empty queue it says so instead of going silent.
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut worker = ExportWorker::spawn(
        tx,
        Arc::new(RepaintHandle::new(egui::Context::default())),
        RetrySchedule {
            queue_dir: dir.path().to_owned(),
            anki: config("http://127.0.0.1:1".to_owned()),
        },
    );
    assert!(worker.retry_now());
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut empty_note = None;
    while Instant::now() < deadline && empty_note.is_none() {
        while let Ok(event) = rx.try_recv() {
            if let CoreEvent::Status { text, .. } = event {
                empty_note = Some(text);
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    worker.stop();
    worker.join();
    assert_eq!(
        empty_note.as_deref(),
        Some("nothing queued offline"),
        "an explicit retry on an empty queue answers"
    );
}

/// The other half of the rule: `retry_interval_s = 0` switches background retries off, so
/// the queue is left alone until the next real export.
#[tokio::test]
async fn export_worker_leaves_the_queue_alone_when_retries_are_disabled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let queue = OfflineQueue::new(dir.path().to_owned());
    queue
        .push(&PendingNote {
            deck: "Japanese::Mining".to_owned(),
            model: "Japanese Mining".to_owned(),
            tags: vec![],
            fields: BTreeMap::from([("Expression".to_owned(), "食べる".to_owned())]),
            media: vec![],
        })
        .expect("push");

    // No mocks mounted at all: any attempt to reach Anki would simply fail, so the point
    // is that the worker never tries.
    let mut anki = config("http://127.0.0.1:1".to_owned());
    anki.retry_interval_s = 0;
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut worker = ExportWorker::spawn(
        tx,
        Arc::new(RepaintHandle::new(egui::Context::default())),
        RetrySchedule {
            queue_dir: dir.path().to_owned(),
            anki,
        },
    );
    tokio::time::sleep(Duration::from_millis(700)).await;
    worker.stop();
    worker.join();
    assert_eq!(
        queue.pending_count(),
        1,
        "retries off means the note stays queued"
    );
}
