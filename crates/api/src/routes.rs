use std::path::{Component, Path};

use axum::{
    Json, Router,
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::FromRow;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use uuid::Uuid;

use crate::AppState;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .route("/api/v1/works", get(list_works).post(create_work))
        .route("/api/v1/works/{id}", get(get_work))
        .route("/api/v1/documents/register", post(register_document))
}

async fn live() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

async fn ready(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.db)
        .await?;
    tokio::fs::metadata(state.data_root.as_ref()).await?;
    Ok(Json(json!({"status": "ready"})))
}

#[derive(Deserialize)]
struct Pagination {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
}

fn default_limit() -> i64 {
    50
}

#[derive(Debug, Serialize, FromRow)]
struct Work {
    id: Uuid,
    title: String,
    work_type: String,
    publication_year: Option<i32>,
    doi: Option<String>,
    review_status: String,
}

async fn list_works(
    State(state): State<AppState>,
    Query(page): Query<Pagination>,
) -> Result<Json<Vec<Work>>, ApiError> {
    let limit = page.limit.clamp(1, 200);
    let offset = page.offset.max(0);
    let works = sqlx::query_as::<_, Work>(
        "SELECT id, title, work_type::text AS work_type, publication_year, doi, \
         review_status::text AS review_status FROM works \
         ORDER BY publication_year NULLS LAST, title LIMIT $1 OFFSET $2",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(works))
}

async fn get_work(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<Uuid>,
) -> Result<Json<Work>, ApiError> {
    let work = sqlx::query_as::<_, Work>(
        "SELECT id, title, work_type::text AS work_type, publication_year, doi, \
         review_status::text AS review_status FROM works WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(ApiError::NotFound)?;
    Ok(Json(work))
}

#[derive(Deserialize)]
struct CreateWork {
    title: String,
    #[serde(default = "default_work_type")]
    work_type: String,
    publication_year: Option<i32>,
    doi: Option<String>,
    source_url: String,
    source_kind: String,
    reliability: String,
    rights_status: String,
    license_name: Option<String>,
    license_url: Option<String>,
}

fn default_work_type() -> String {
    "article".into()
}

async fn create_work(
    State(state): State<AppState>,
    Json(input): Json<CreateWork>,
) -> Result<(StatusCode, Json<Work>), ApiError> {
    if input.title.trim().is_empty() || input.source_url.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "title and source_url are required".into(),
        ));
    }
    let mut tx = state.db.begin().await?;
    let work = sqlx::query_as::<_, Work>(
        "INSERT INTO works (title, work_type, publication_year, doi) \
         VALUES ($1, $2::work_type, $3, $4) \
         RETURNING id, title, work_type::text AS work_type, publication_year, doi, \
         review_status::text AS review_status",
    )
    .bind(input.title.trim())
    .bind(&input.work_type)
    .bind(input.publication_year)
    .bind(input.doi.as_deref().map(normalize_doi))
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO sources \
         (work_id, url, source_kind, reliability, rights_status, license_name, license_url) \
         VALUES ($1, $2, $3::source_kind, $4::source_reliability, $5::rights_status, $6, $7)",
    )
    .bind(work.id)
    .bind(input.source_url)
    .bind(input.source_kind)
    .bind(input.reliability)
    .bind(input.rights_status)
    .bind(input.license_name)
    .bind(input.license_url)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(work)))
}

#[derive(Deserialize)]
struct RegisterDocument {
    work_id: Uuid,
    object_key: String,
    sha256: String,
    media_type: String,
    rights_status: String,
    ml_use_permitted: bool,
}

#[derive(Serialize)]
struct RegisteredDocument {
    document_id: Uuid,
    extraction_job_id: Option<Uuid>,
}

async fn register_document(
    State(state): State<AppState>,
    Json(input): Json<RegisterDocument>,
) -> Result<(StatusCode, Json<RegisteredDocument>), ApiError> {
    validate_object_key(&input.object_key)?;
    if input.sha256.len() != 64 || !input.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError::BadRequest(
            "sha256 must be 64 hexadecimal characters".into(),
        ));
    }
    let path = state.data_root.join(&input.object_key);
    let metadata = tokio::fs::metadata(&path).await.map_err(|_| {
        ApiError::BadRequest("object_key does not identify an existing local file".into())
    })?;
    if !metadata.is_file() {
        return Err(ApiError::BadRequest(
            "object_key must identify a regular file".into(),
        ));
    }
    if !matches!(
        input.rights_status.as_str(),
        "open_redistributable" | "internal_research_copy"
    ) {
        return Err(ApiError::BadRequest(
            "rights_status does not permit retaining a local document".into(),
        ));
    }
    let actual_sha256 = sha256_file(&path).await?;
    if !actual_sha256.eq_ignore_ascii_case(&input.sha256) {
        return Err(ApiError::BadRequest(
            "sha256 does not match the local object".into(),
        ));
    }

    let mut tx = state.db.begin().await?;
    let document_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO documents \
         (id, work_id, object_key, sha256, media_type, byte_size, rights_status, ml_use_permitted) \
         VALUES ($1, $2, $3, lower($4), $5, $6, $7::rights_status, $8)",
    )
    .bind(document_id)
    .bind(input.work_id)
    .bind(&input.object_key)
    .bind(&actual_sha256)
    .bind(&input.media_type)
    .bind(metadata.len() as i64)
    .bind(&input.rights_status)
    .bind(input.ml_use_permitted)
    .execute(&mut *tx)
    .await?;

    let extraction_job_id = if input.ml_use_permitted {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO jobs (id, job_type, idempotency_key, payload) \
             VALUES ($1, 'extract_document', $2, $3)",
        )
        .bind(id)
        .bind(format!("extract:{}:{}", document_id, actual_sha256))
        .bind(json!({"document_id": document_id, "object_key": input.object_key}))
        .execute(&mut *tx)
        .await?;
        Some(id)
    } else {
        None
    };
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(RegisteredDocument {
            document_id,
            extraction_job_id,
        }),
    ))
}

async fn sha256_file(path: &Path) -> Result<String, ApiError> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn validate_object_key(key: &str) -> Result<(), ApiError> {
    let path = Path::new(key);
    if path.is_absolute()
        || key.is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(ApiError::BadRequest(
            "object_key must be a relative normalized path".into(),
        ));
    }
    Ok(())
}

fn normalize_doi(doi: &str) -> String {
    let normalized = doi.trim().to_ascii_lowercase();
    normalized
        .strip_prefix("https://doi.org/")
        .or_else(|| normalized.strip_prefix("http://doi.org/"))
        .unwrap_or(&normalized)
        .to_owned()
}

#[derive(Debug, Error)]
enum ApiError {
    #[error("not found")]
    NotFound,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("database error")]
    Database(#[from] sqlx::Error),
    #[error("storage error")]
    Storage(#[from] std::io::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            Self::Database(error) => {
                tracing::error!(%error, "database request failed");
                if let Some(db_error) = error.as_database_error() {
                    if db_error.is_unique_violation() || db_error.is_check_violation() {
                        (
                            StatusCode::CONFLICT,
                            "record conflicts with existing data or policy".into(),
                        )
                    } else {
                        (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
                    }
                } else {
                    (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
                }
            }
            Self::Storage(error) => {
                tracing::error!(%error, "storage request failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (status, Json(json!({"error": message}))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_escaping_object_keys() {
        assert!(validate_object_key("../secret").is_err());
        assert!(validate_object_key("/etc/passwd").is_err());
        assert!(validate_object_key("objects/ab/file.pdf").is_ok());
    }

    #[test]
    fn doi_is_normalized() {
        assert_eq!(normalize_doi(" HTTPS://doi.org/10.1/ABC "), "10.1/abc");
        assert_eq!(normalize_doi("https://doi.org/10.1/ABC"), "10.1/abc");
    }
}
