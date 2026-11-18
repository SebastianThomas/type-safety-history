use std::path::{Component, Path as FilePath, PathBuf};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::{AppState, auth::require_curator, routes::ApiError};

pub(crate) fn router() -> Router<AppState> {
    Router::new().route("/api/v1/documents/{id}/takedown", post(take_down_document))
}

#[derive(Deserialize)]
struct TakedownRequest {
    #[serde(default, rename = "actor")]
    _actor: String,
    reason: String,
}

async fn take_down_document(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(document_id): Path<Uuid>,
    Json(input): Json<TakedownRequest>,
) -> Result<StatusCode, ApiError> {
    let principal = require_curator(&state, &headers).await?;
    if input.reason.trim().is_empty() {
        return Err(ApiError::BadRequest("reason is required".into()));
    }

    let mut tx = state.db.begin().await?;
    let document: (String, String, Option<String>) = sqlx::query_as(
        "SELECT d.object_key, d.status::text, de.zarr_object_key \
         FROM documents d LEFT JOIN document_embeddings de ON de.document_id = d.id \
         WHERE d.id = $1 FOR UPDATE OF d",
    )
    .bind(document_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;

    let object_path = safe_data_path(state.data_root.as_ref(), &document.0)?;
    let quarantine_path = state
        .data_root
        .join("tmp")
        .join(format!("takedown-{document_id}.quarantine"));
    let derived_path = state
        .data_root
        .join("derived")
        .join(document_id.to_string());
    let zarr_path = document
        .2
        .as_deref()
        .map(|key| safe_data_path(state.data_root.as_ref(), key))
        .transpose()?;

    if document.1 == "taken_down" {
        drop(tx);
        remove_if_present(&object_path).await?;
        remove_if_present(&quarantine_path).await?;
        remove_if_present(&derived_path).await?;
        if let Some(path) = zarr_path {
            remove_if_present(&path).await?;
        }
        return Ok(StatusCode::NO_CONTENT);
    }

    let moved = if tokio::fs::try_exists(&object_path).await? {
        if let Some(parent) = quarantine_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        remove_if_present(&quarantine_path).await?;
        tokio::fs::rename(&object_path, &quarantine_path).await?;
        true
    } else {
        false
    };

    let database_result: Result<(), sqlx::Error> = async {
        sqlx::query("DELETE FROM document_embeddings WHERE document_id = $1")
            .bind(document_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM citation_candidates WHERE source_document_id = $1")
            .bind(document_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE proof_results SET status = 'rejected' \
             WHERE source_document_id = $1 AND status = 'machine_candidate'",
        )
        .bind(document_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE jobs SET status = 'dead_letter', leased_until = NULL, \
             last_error = 'Document taken down by curator', updated_at = now() \
             WHERE job_type = 'extract_document' AND payload->>'document_id' = $1 \
             AND status IN ('queued', 'running', 'failed')",
        )
        .bind(document_id.to_string())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE documents SET status = 'taken_down', taken_down_at = now(), \
             taken_down_by = $2, takedown_reason = $3, ml_use_permitted = false, \
             redistribution_permitted = false WHERE id = $1",
        )
        .bind(document_id)
        .bind(principal.username())
        .bind(input.reason.trim())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO audit_log \
             (actor, action, entity_type, entity_id, after_value, reason) \
             VALUES ($1, 'takedown', 'document', $2, $3, $4)",
        )
        .bind(principal.username())
        .bind(document_id)
        .bind(json!({"status": "taken_down", "object_removed": moved}))
        .bind(input.reason.trim())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
    .await;

    if let Err(error) = database_result {
        if moved {
            let _ = tokio::fs::rename(&quarantine_path, &object_path).await;
        }
        return Err(error.into());
    }

    remove_if_present(&quarantine_path).await?;
    remove_if_present(&derived_path).await?;
    if let Some(path) = zarr_path {
        remove_if_present(&path).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

fn safe_data_path(root: &FilePath, key: &str) -> Result<PathBuf, ApiError> {
    let relative = FilePath::new(key);
    if relative.is_absolute()
        || key.is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ApiError::BadRequest("invalid stored object key".into()));
    }
    Ok(root.join(relative))
}

async fn remove_if_present(path: &FilePath) -> Result<(), ApiError> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.is_dir() {
        tokio::fs::remove_dir_all(path).await?;
    } else {
        tokio::fs::remove_file(path).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_paths_reject_parent_traversal() {
        assert!(safe_data_path(FilePath::new("/data"), "../secret").is_err());
        assert!(safe_data_path(FilePath::new("/data"), "/etc/passwd").is_err());
        assert_eq!(
            safe_data_path(FilePath::new("/data"), "objects/sha256/abc").unwrap(),
            FilePath::new("/data/objects/sha256/abc")
        );
    }
}
