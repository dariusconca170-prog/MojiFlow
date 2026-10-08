//! The AnkiConnect HTTP client.
//!
//! Thin JSON transport + the handful of actions M6 needs (`modelNames`, `createModel`,
//! `storeMediaFile`, `addNote`). Every request is `{"action", "version": 6, "params"}` and
//! every response is `{"result": ..., "error": <string|null>}` — errors carry no HTTP status,
//! so the client maps them onto the typed [`AnkiError`] family at the API boundary.

use std::time::Duration;

use serde_json::{json, Value};

use crate::config::AnkiConfig;
use crate::error::AnkiError;

/// AnkiConnect protocol version spoken by this client.
pub const ANKICONNECT_VERSION: i64 = 6;

/// How the error message and HTTP failures are classified.
pub fn is_duplicate_error(message: &str) -> bool {
    message.to_ascii_lowercase().contains("duplicate")
}

/// A configured, ready-to-use AnkiConnect endpoint.
pub struct AnkiConnect {
    url: String,
    client: reqwest::Client,
}

impl AnkiConnect {
    pub fn new(config: &AnkiConfig) -> Result<Self, AnkiError> {
        let url = config.url.clone();
        let timeout = Duration::from_millis(config.connect_timeout_ms.max(1));
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout)
            .build()
            .map_err(|err| AnkiError::Unreachable {
                url: url.clone(),
                reason: err.to_string(),
            })?;
        Ok(Self { url, client })
    }

    /// Send `action` and return the JSON `result` field; non-null `error` becomes
    /// [`AnkiError::Remote`], transport failure becomes [`AnkiError::Unreachable`].
    pub async fn request(&self, action: &str, params: Value) -> Result<Value, AnkiError> {
        let payload = json!({
            "action": action,
            "version": ANKICONNECT_VERSION,
            "params": params,
        });
        let url = self.url.clone();
        let response = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|err| AnkiError::Unreachable {
                url: url.clone(),
                reason: err.to_string(),
            })?;
        let body: Value = response
            .json()
            .await
            .map_err(|err| AnkiError::Malformed(format!("failed to parse response body: {err}")))?;
        let object = body.as_object().ok_or_else(|| {
            AnkiError::Malformed(format!("response is not a JSON object: {body}"))
        })?;
        let result = object.get("result").cloned().unwrap_or(Value::Null);
        if let Some(message) = object.get("error").and_then(|e| e.as_str()) {
            return Err(AnkiError::Remote {
                code: ANKICONNECT_VERSION,
                message: message.to_owned(),
            });
        }
        Ok(result)
    }

    /// List all model (note type) names.
    pub async fn model_names(&self) -> Result<Vec<String>, AnkiError> {
        let result = self.request("modelNames", json!({})).await?;
        serde_json::from_value(result).map_err(|err| AnkiError::Malformed(err.to_string()))
    }

    /// Create the note model if it does not exist yet. The card front shows the first field;
    /// the back shows every field stacked with `<br>`.
    pub async fn ensure_model(&self, model: &str, fields: &[String]) -> Result<(), AnkiError> {
        let names = self.model_names().await?;
        if names.iter().any(|name| name == model) {
            return Ok(());
        }
        let front = fields
            .first()
            .cloned()
            .unwrap_or_else(|| "Expression".to_owned());
        let back = fields
            .iter()
            .map(|field| format!("{{{{{field}}}}}<br>"))
            .collect::<String>();
        let params = json!({
            "modelName": model,
            "inOrderFields": fields,
            "cardTemplates": [{
                "Name": format!("{model} Card 1"),
                "Front": format!("{{{{{front}}}}}"),
                "Back": back,
            }],
            "isCloze": false,
        });
        self.request("createModel", params).await?;
        Ok(())
    }

    /// Upload a media file (base64-encoded bytes) into Anki's media folder.
    pub async fn store_media_file(
        &self,
        filename: &str,
        data_base64: &str,
    ) -> Result<(), AnkiError> {
        let params = json!({
            "filename": filename,
            "data": data_base64,
        });
        self.request("storeMediaFile", params).await?;
        Ok(())
    }

    /// Add one note. `allow_duplicate`/`duplicate_scope` control duplicate handling at the
    /// Anki level; a duplicate rejection is mapped to [`AnkiError::Duplicate`], everything
    /// else is the raw remote error.
    pub async fn add_note(
        &self,
        deck: &str,
        model: &str,
        fields: &std::collections::BTreeMap<String, String>,
        tags: &[String],
        allow_duplicate: bool,
        duplicate_scope: &str,
    ) -> Result<i64, AnkiError> {
        let fields_map: serde_json::Map<String, Value> = fields
            .iter()
            .map(|(name, value)| (name.clone(), Value::String(value.clone())))
            .collect();
        let params = json!({
            "note": {
                "deckName": deck,
                "modelName": model,
                "fields": fields_map,
                "options": {
                    "allowDuplicate": allow_duplicate,
                    "duplicateScope": duplicate_scope,
                },
                "tags": tags,
            }
        });
        match self.request("addNote", params).await {
            Ok(result) => {
                serde_json::from_value(result).map_err(|err| AnkiError::Malformed(err.to_string()))
            }
            Err(AnkiError::Remote { message, .. }) if is_duplicate_error(&message) => {
                Err(AnkiError::Duplicate(duplicate_scope.to_owned()))
            }
            Err(err) => Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_detection_is_case_insensitive_substring() {
        assert!(is_duplicate_error(
            "cannot create note because it is a duplicate"
        ));
        assert!(is_duplicate_error("DUPLICATE note"));
        assert!(!is_duplicate_error("model not found"));
    }
}
