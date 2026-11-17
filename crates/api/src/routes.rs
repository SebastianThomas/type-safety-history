use std::path::{Component, Path};

use axum::{
    Json, Router,
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
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
use crate::crossref::CrossrefError;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .route("/api/v1/works", get(list_works).post(create_work))
        .route("/api/v1/works/{id}", get(get_work))
        .route("/api/v1/documents/register", post(register_document))
        .route("/api/v1/imports/crossref", post(import_crossref))
        .route("/api/v1/proof-results/candidates", get(list_candidates))
        .route("/api/v1/proof-results/{id}/review", post(review_candidate))
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
    headers: HeaderMap,
    Json(input): Json<CreateWork>,
) -> Result<(StatusCode, Json<Work>), ApiError> {
    require_curator(&state, &headers)?;
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
    headers: HeaderMap,
    Json(input): Json<RegisterDocument>,
) -> Result<(StatusCode, Json<RegisteredDocument>), ApiError> {
    require_curator(&state, &headers)?;
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

#[derive(Deserialize)]
struct CrossrefImportRequest {
    doi: String,
    #[serde(default)]
    refresh: bool,
}

#[derive(Serialize)]
struct CrossrefImportResponse {
    work: Work,
    cached: bool,
}

async fn import_crossref(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CrossrefImportRequest>,
) -> Result<(StatusCode, Json<CrossrefImportResponse>), ApiError> {
    require_curator(&state, &headers)?;
    let doi = normalize_doi(&input.doi);
    if !doi.starts_with("10.") || !doi.contains('/') || doi.len() > 255 {
        return Err(ApiError::BadRequest("invalid DOI".into()));
    }
    let existing = find_work_by_doi(&state, &doi).await?;
    if let Some(work) = existing.filter(|_| !input.refresh) {
        return Ok((
            StatusCode::OK,
            Json(CrossrefImportResponse { work, cached: true }),
        ));
    }

    let crossref = state.crossref.as_ref().ok_or(ApiError::NotConfigured(
        "CROSSREF_MAILTO must be configured",
    ))?;
    let imported = crossref.fetch(&doi).await?;
    if imported.doi != doi {
        return Err(ApiError::Upstream(
            "Crossref returned a different DOI".into(),
        ));
    }

    let mut tx = state.db.begin().await?;
    let inserted = sqlx::query_as::<_, Work>(
        "INSERT INTO works (title, work_type, publication_year, doi) \
         VALUES ($1, $2::work_type, $3, $4) \
         ON CONFLICT (doi) DO NOTHING \
         RETURNING id, title, work_type::text AS work_type, publication_year, doi, \
         review_status::text AS review_status",
    )
    .bind(&imported.title)
    .bind(imported.work_type)
    .bind(imported.publication_year)
    .bind(&imported.doi)
    .fetch_optional(&mut *tx)
    .await?;
    let is_new = inserted.is_some();
    let work = match inserted {
        Some(work) => work,
        None => {
            sqlx::query_as::<_, Work>(
                "SELECT id, title, work_type::text AS work_type, publication_year, doi, \
             review_status::text AS review_status FROM works WHERE doi = $1",
            )
            .bind(&imported.doi)
            .fetch_one(&mut *tx)
            .await?
        }
    };

    let source_url = format!("https://api.crossref.org/v1/works/{}", imported.doi);
    sqlx::query(
        "INSERT INTO sources \
         (work_id, url, source_kind, reliability, rights_status, terms_url, raw_metadata) \
         VALUES ($1, $2, 'doi_registry', 'authoritative_metadata', 'open_metadata_only', \
                 'https://www.crossref.org/documentation/retrieve-metadata/rest-api/', $3) \
         ON CONFLICT (url, work_id) DO UPDATE SET \
             raw_metadata = EXCLUDED.raw_metadata, acquired_at = now()",
    )
    .bind(work.id)
    .bind(source_url)
    .bind(imported.selected_metadata)
    .execute(&mut *tx)
    .await?;

    if is_new {
        for (position, author) in imported.authors.iter().enumerate() {
            let display_name = [author.given.as_deref(), author.family.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            if display_name.is_empty() {
                continue;
            }
            let orcid = author.orcid.as_deref().map(normalize_orcid);
            let person_id: Uuid = sqlx::query_scalar(
                "INSERT INTO people (display_name, orcid) VALUES ($1, $2) \
                 ON CONFLICT (orcid) DO UPDATE SET display_name = EXCLUDED.display_name \
                 RETURNING id",
            )
            .bind(display_name)
            .bind(orcid)
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO work_authors (work_id, person_id, position) VALUES ($1, $2, $3)",
            )
            .bind(work.id)
            .bind(person_id)
            .bind(position as i32)
            .execute(&mut *tx)
            .await?;
        }
    }
    sqlx::query(
        "INSERT INTO audit_log (actor, action, entity_type, entity_id, after_value, reason) \
         VALUES ('crossref-adapter', 'import_metadata', 'work', $1, $2, \
                 'Curator-requested Crossref DOI lookup')",
    )
    .bind(work.id)
    .bind(json!({"doi": imported.doi, "new_work": is_new}))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok((
        if is_new {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(CrossrefImportResponse {
            work,
            cached: false,
        }),
    ))
}

async fn find_work_by_doi(state: &AppState, doi: &str) -> Result<Option<Work>, ApiError> {
    Ok(sqlx::query_as::<_, Work>(
        "SELECT id, title, work_type::text AS work_type, publication_year, doi, \
         review_status::text AS review_status FROM works WHERE doi = $1",
    )
    .bind(doi)
    .fetch_optional(&state.db)
    .await?)
}

#[derive(Serialize, FromRow)]
struct ProofCandidate {
    id: Uuid,
    work_id: Uuid,
    theorem_name: Option<String>,
    result_kind: String,
    proof_method: Option<String>,
    statement_summary: String,
    created_at: String,
}

async fn list_candidates(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(page): Query<Pagination>,
) -> Result<Json<Vec<ProofCandidate>>, ApiError> {
    require_curator(&state, &headers)?;
    let candidates = sqlx::query_as::<_, ProofCandidate>(
        "SELECT id, work_id, theorem_name, result_kind, proof_method, statement_summary, \
         created_at::text AS created_at \
         FROM proof_results WHERE status = 'machine_candidate' \
         ORDER BY created_at LIMIT $1 OFFSET $2",
    )
    .bind(page.limit.clamp(1, 200))
    .bind(page.offset.max(0))
    .fetch_all(&state.db)
    .await?;
    Ok(Json(candidates))
}

#[derive(Deserialize)]
struct ReviewCandidate {
    decision: String,
    actor: String,
    reason: String,
}

async fn review_candidate(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<Uuid>,
    Json(input): Json<ReviewCandidate>,
) -> Result<StatusCode, ApiError> {
    require_curator(&state, &headers)?;
    let status = match input.decision.as_str() {
        "approve" => "curator_approved",
        "reject" => "rejected",
        "dispute" => "disputed",
        _ => return Err(ApiError::BadRequest("invalid review decision".into())),
    };
    if input.actor.trim().is_empty() || input.reason.trim().is_empty() {
        return Err(ApiError::BadRequest("actor and reason are required".into()));
    }
    let mut tx = state.db.begin().await?;
    let previous: String =
        sqlx::query_scalar("SELECT status::text FROM proof_results WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(ApiError::NotFound)?;
    sqlx::query("UPDATE proof_results SET status = $2::assertion_status WHERE id = $1")
        .bind(id)
        .bind(status)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO audit_log \
         (actor, action, entity_type, entity_id, before_value, after_value, reason) \
         VALUES ($1, 'review', 'proof_result', $2, $3, $4, $5)",
    )
    .bind(input.actor.trim())
    .bind(id)
    .bind(json!({"status": previous}))
    .bind(json!({"status": status}))
    .bind(input.reason.trim())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

fn require_curator(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let configured = state
        .curator_token
        .as_deref()
        .ok_or(ApiError::NotConfigured("CURATOR_TOKEN must be configured"))?;
    let supplied = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or(ApiError::Unauthorized)?;
    if supplied.as_bytes() != configured.as_bytes() {
        return Err(ApiError::Unauthorized);
    }
    Ok(())
}

fn normalize_orcid(orcid: &str) -> String {
    orcid
        .trim()
        .trim_start_matches("https://orcid.org/")
        .trim_start_matches("http://orcid.org/")
        .to_ascii_uppercase()
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
    #[error("unauthorized")]
    Unauthorized,
    #[error("service is not configured: {0}")]
    NotConfigured(&'static str),
    #[error("upstream service error: {0}")]
    Upstream(String),
    #[error("Crossref error")]
    Crossref(#[from] CrossrefError),
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
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".into()),
            Self::NotConfigured(message) => (StatusCode::SERVICE_UNAVAILABLE, message.into()),
            Self::Upstream(message) => (StatusCode::BAD_GATEWAY, message),
            Self::Crossref(CrossrefError::NotFound) => (
                StatusCode::NOT_FOUND,
                "DOI is not registered with Crossref".into(),
            ),
            Self::Crossref(CrossrefError::RateLimited) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Crossref rate limit reached; retry later".into(),
            ),
            Self::Crossref(error) => {
                tracing::error!(%error, "Crossref request failed");
                (StatusCode::BAD_GATEWAY, "Crossref request failed".into())
            }
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
