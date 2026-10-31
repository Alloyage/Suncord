use crate::http::{page_account, signed_in, App};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

/// Files uploaded by the page for the next send or edit, kept per account.
#[derive(Default)]
pub struct Uploads {
    next: AtomicU64,
    files: Mutex<HashMap<String, Upload>>,
}

struct Upload {
    account: String,
    name: String,
    bytes: Bytes,
}

impl Uploads {
    fn store(&self, account: String, name: String, bytes: Bytes) -> Result<String, StatusCode> {
        let mut files = self.files.lock().unwrap();
        if bytes.len() > 10 * 1024 * 1024
            || files.len() >= 40
            || files.values().map(|file| file.bytes.len()).sum::<usize>() + bytes.len()
                > 40 * 1024 * 1024
        {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed).to_string();
        files.insert(
            id.clone(),
            Upload {
                account,
                name,
                bytes,
            },
        );
        Ok(id)
    }

    /// Names and contents of the account's uploads.
    pub fn contents(
        &self,
        account: &str,
        ids: &[String],
    ) -> Result<Vec<(String, Vec<u8>)>, String> {
        let files = self.files.lock().unwrap();
        ids.iter()
            .map(|id| {
                let file = files
                    .get(id)
                    .filter(|file| file.account == account)
                    .ok_or("Attachment expired")?;
                Ok((file.name.clone(), file.bytes.to_vec()))
            })
            .collect()
    }

    pub fn remove(&self, ids: &[String]) {
        let mut files = self.files.lock().unwrap();
        for id in ids {
            files.remove(id);
        }
    }

    pub fn retain(&self, keep: impl Fn(&str) -> bool) {
        self.files
            .lock()
            .unwrap()
            .retain(|_, file| keep(&file.account));
    }
}

#[derive(Deserialize)]
pub struct UploadName {
    name: String,
}
pub async fn upload(
    State(app): State<Arc<App>>,
    Query(query): Query<UploadName>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let _gate = app.session.gate.read().await;
    if app.session.closed() {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    if !signed_in(&app, &headers) {
        return Err(StatusCode::CONFLICT);
    }
    let name = std::path::Path::new(&query.name)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && query.name.len() <= 255)
        .ok_or(StatusCode::BAD_REQUEST)?;
    let id = app
        .uploads
        .store(page_account(&headers).to_owned(), name.to_owned(), bytes)?;
    Ok(Json(serde_json::json!({"id": id})))
}

pub async fn remove_upload(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> StatusCode {
    let _gate = app.session.gate.read().await;
    if app.session.closed() {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    if !signed_in(&app, &headers) {
        return StatusCode::CONFLICT;
    }
    let mut files = app.uploads.files.lock().unwrap();
    if files
        .get(&id)
        .is_some_and(|file| file.account == page_account(&headers))
    {
        files.remove(&id);
    }
    StatusCode::NO_CONTENT
}
