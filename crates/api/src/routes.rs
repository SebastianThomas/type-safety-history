use std::path::{Component, Path};

use axum::{
    Json, Router,
    extract::{Multipart, Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::FromRow;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
        .route("/api/v1/documents/upload", post(upload_document))
        .route("/api/v1/imports/crossref", post(import_crossref))
        .route("/api/v1/discovery/crossref", post(discover_crossref))
        .route(
            "/api/v1/discovery/candidates",
            get(list_discovery_candidates),
        )
        .route(
            "/api/v1/discovery/candidates/{id}/reject",
            post(reject_discovery_candidate),
        )
        .route("/api/v1/proof-results/candidates", get(list_candidates))
        .route("/api/v1/proof-results/curated", post(create_curated_proof))
        .route("/api/v1/proof-results/{id}", get(get_proof_result))
        .route("/api/v1/proof-results/{id}/review", post(review_candidate))
        .route("/api/v1/timeline", get(timeline))
        .route("/api/v1/exports", post(create_export))
        .route("/api/v1/exports/{id}", get(get_export))
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
    rights_basis: String,
    #[serde(default)]
    ml_use_permitted: bool,
    #[serde(default)]
    redistribution_permitted: bool,
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
    validate_source_taxonomy(&input.source_kind, &input.reliability)?;
    validate_source_policy(
        &input.rights_status,
        &input.rights_basis,
        input.license_name.as_deref(),
        input.license_url.as_deref(),
        input.ml_use_permitted,
        input.redistribution_permitted,
    )?;
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
         (work_id, url, source_kind, reliability, rights_status, license_name, license_url, \
          rights_basis, ml_use_permitted, redistribution_permitted) \
         VALUES ($1, $2, $3::source_kind, $4::source_reliability, $5::rights_status, \
                 $6, $7, $8, $9, $10)",
    )
    .bind(work.id)
    .bind(input.source_url)
    .bind(input.source_kind)
    .bind(input.reliability)
    .bind(input.rights_status)
    .bind(input.license_name)
    .bind(input.license_url)
    .bind(input.rights_basis.trim())
    .bind(input.ml_use_permitted)
    .bind(input.redistribution_permitted)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(work)))
}

#[derive(Deserialize)]
struct CrossrefDiscoveryRequest {
    query: String,
    from_year: i32,
    until_year: i32,
    #[serde(default = "default_discovery_rows")]
    rows: u16,
}

fn default_discovery_rows() -> u16 {
    20
}

async fn discover_crossref(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CrossrefDiscoveryRequest>,
) -> Result<Json<Value>, ApiError> {
    require_curator(&state, &headers)?;
    let query = input.query.trim();
    if !(3..=200).contains(&query.len()) {
        return Err(ApiError::BadRequest(
            "query must contain 3-200 characters".into(),
        ));
    }
    if input.from_year < 1800
        || input.until_year > 2200
        || input.until_year < input.from_year
        || input.until_year - input.from_year > 100
    {
        return Err(ApiError::BadRequest(
            "year range must be valid and at most 100 years".into(),
        ));
    }
    if !(1..=100).contains(&input.rows) {
        return Err(ApiError::BadRequest(
            "rows must be between 1 and 100".into(),
        ));
    }
    let crossref = state.crossref.as_ref().ok_or(ApiError::NotConfigured(
        "CROSSREF_MAILTO must be configured",
    ))?;
    let discovered = crossref
        .search(
            query,
            Some(input.from_year),
            Some(input.until_year),
            input.rows,
        )
        .await?;
    let mut tx = state.db.begin().await?;
    let mut candidate_ids = Vec::with_capacity(discovered.len());
    for work in discovered {
        let candidate_id: Uuid = sqlx::query_scalar(
            "INSERT INTO discovery_candidates \
             (provider, external_id, title, publication_year, work_type, raw_metadata) \
             VALUES ('crossref', $1, $2, $3, $4::work_type, $5) \
             ON CONFLICT (provider, external_id) DO UPDATE SET \
                 title = EXCLUDED.title, publication_year = EXCLUDED.publication_year, \
                 work_type = EXCLUDED.work_type, raw_metadata = EXCLUDED.raw_metadata \
             RETURNING id",
        )
        .bind(&work.doi)
        .bind(&work.title)
        .bind(work.publication_year)
        .bind(work.work_type)
        .bind(work.selected_metadata)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO discovery_hits (candidate_id, query_text, from_year, until_year) \
             VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
        )
        .bind(candidate_id)
        .bind(query)
        .bind(input.from_year)
        .bind(input.until_year)
        .execute(&mut *tx)
        .await?;
        candidate_ids.push(candidate_id);
    }
    sqlx::query(
        "INSERT INTO audit_log (actor, action, entity_type, after_value, reason) \
         VALUES ('crossref-discovery', 'discover', 'discovery_candidate', $1, \
                 'Curator-requested Crossref bibliographic search')",
    )
    .bind(json!({
        "query": query,
        "from_year": input.from_year,
        "until_year": input.until_year,
        "result_count": candidate_ids.len()
    }))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "candidate_ids": candidate_ids,
        "result_count": candidate_ids.len()
    })))
}

#[derive(Deserialize)]
struct DiscoveryPagination {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
    #[serde(default = "default_discovery_status")]
    status: String,
}

fn default_discovery_status() -> String {
    "pending".into()
}

#[derive(Serialize, FromRow)]
struct DiscoveryCandidate {
    id: Uuid,
    provider: String,
    external_id: String,
    title: String,
    publication_year: Option<i32>,
    work_type: String,
    status: String,
    work_id: Option<Uuid>,
    discovered_at: String,
    hit_count: i64,
}

async fn list_discovery_candidates(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(page): Query<DiscoveryPagination>,
) -> Result<Json<Vec<DiscoveryCandidate>>, ApiError> {
    require_curator(&state, &headers)?;
    if !matches!(page.status.as_str(), "pending" | "imported" | "rejected") {
        return Err(ApiError::BadRequest("invalid discovery status".into()));
    }
    let candidates = sqlx::query_as::<_, DiscoveryCandidate>(
        "SELECT dc.id, dc.provider, dc.external_id, dc.title, dc.publication_year, \
         dc.work_type::text AS work_type, dc.status::text AS status, dc.work_id, \
         dc.discovered_at::text AS discovered_at, count(dh.id) AS hit_count \
         FROM discovery_candidates dc LEFT JOIN discovery_hits dh ON dh.candidate_id = dc.id \
         WHERE dc.status = $1::discovery_status GROUP BY dc.id \
         ORDER BY dc.discovered_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(&page.status)
    .bind(page.limit.clamp(1, 200))
    .bind(page.offset.max(0))
    .fetch_all(&state.db)
    .await?;
    Ok(Json(candidates))
}

#[derive(Deserialize)]
struct RejectDiscoveryCandidate {
    actor: String,
    reason: String,
}

async fn reject_discovery_candidate(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<Uuid>,
    Json(input): Json<RejectDiscoveryCandidate>,
) -> Result<StatusCode, ApiError> {
    require_curator(&state, &headers)?;
    if input.actor.trim().is_empty() || input.reason.trim().is_empty() {
        return Err(ApiError::BadRequest("actor and reason are required".into()));
    }
    let mut tx = state.db.begin().await?;
    let updated = sqlx::query(
        "UPDATE discovery_candidates SET status = 'rejected', review_reason = $2, \
         reviewed_at = now(), work_id = NULL WHERE id = $1 AND status = 'pending'",
    )
    .bind(id)
    .bind(input.reason.trim())
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    sqlx::query(
        "INSERT INTO audit_log \
         (actor, action, entity_type, entity_id, after_value, reason) \
         VALUES ($1, 'reject', 'discovery_candidate', $2, $3, $4)",
    )
    .bind(input.actor.trim())
    .bind(id)
    .bind(json!({"status": "rejected"}))
    .bind(input.reason.trim())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct RegisterDocument {
    work_id: Uuid,
    source_id: Uuid,
    object_key: String,
    sha256: String,
    media_type: String,
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
    if input.media_type != "application/pdf" || !file_starts_with_pdf_magic(&path).await? {
        return Err(ApiError::BadRequest(
            "only valid application/pdf documents are accepted".into(),
        ));
    }
    let source_policy = load_source_policy(&state, input.work_id, input.source_id).await?;
    ensure_document_storage_permitted(&source_policy)?;
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
         (id, work_id, source_id, object_key, sha256, media_type, byte_size, rights_status, \
          ml_use_permitted, redistribution_permitted) \
         VALUES ($1, $2, $3, $4, lower($5), $6, $7, $8::rights_status, $9, $10)",
    )
    .bind(document_id)
    .bind(input.work_id)
    .bind(input.source_id)
    .bind(&input.object_key)
    .bind(&actual_sha256)
    .bind(&input.media_type)
    .bind(metadata.len() as i64)
    .bind(&source_policy.rights_status)
    .bind(source_policy.ml_use_permitted)
    .bind(source_policy.redistribution_permitted)
    .execute(&mut *tx)
    .await?;

    let extraction_job_id = if source_policy.ml_use_permitted {
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

const MAX_DOCUMENT_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Deserialize)]
struct UploadMetadata {
    work_id: Uuid,
    source_url: String,
    source_kind: String,
    reliability: String,
    rights_status: String,
    license_name: Option<String>,
    license_url: Option<String>,
    terms_url: Option<String>,
    rights_basis: String,
    #[serde(default)]
    ml_use_permitted: bool,
    #[serde(default)]
    redistribution_permitted: bool,
}

struct UploadedFile {
    temporary_path: std::path::PathBuf,
    sha256: String,
    byte_size: u64,
}

async fn upload_document(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<RegisteredDocument>), ApiError> {
    require_curator(&state, &headers)?;
    let mut metadata: Option<UploadMetadata> = None;
    let mut uploaded: Option<UploadedFile> = None;

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|error| ApiError::BadRequest(format!("invalid multipart body: {error}")))?
    {
        match field.name() {
            Some("metadata") => {
                if metadata.is_some() || uploaded.is_some() {
                    return Err(ApiError::BadRequest(
                        "metadata must appear once and before file".into(),
                    ));
                }
                let text = field.text().await.map_err(|error| {
                    ApiError::BadRequest(format!("invalid metadata field: {error}"))
                })?;
                let parsed: UploadMetadata = serde_json::from_str(&text).map_err(|error| {
                    ApiError::BadRequest(format!("invalid metadata JSON: {error}"))
                })?;
                validate_upload_metadata(&parsed)?;
                metadata = Some(parsed);
            }
            Some("file") => {
                if metadata.is_none() || uploaded.is_some() {
                    return Err(ApiError::BadRequest(
                        "file must appear once and after metadata".into(),
                    ));
                }
                if field.content_type() != Some("application/pdf") {
                    return Err(ApiError::BadRequest(
                        "only application/pdf uploads are accepted".into(),
                    ));
                }
                let temporary_path = state
                    .data_root
                    .join("tmp")
                    .join(format!("upload-{}.tmp", Uuid::now_v7()));
                let mut file = tokio::fs::File::create(&temporary_path).await?;
                let mut hasher = Sha256::new();
                let mut byte_size = 0_u64;
                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|error| ApiError::BadRequest(format!("invalid file field: {error}")))?
                {
                    byte_size += chunk.len() as u64;
                    if byte_size > MAX_DOCUMENT_BYTES {
                        let _ = tokio::fs::remove_file(&temporary_path).await;
                        return Err(ApiError::BadRequest(
                            "PDF exceeds the 50 MiB upload limit".into(),
                        ));
                    }
                    hasher.update(&chunk);
                    file.write_all(&chunk).await?;
                }
                file.flush().await?;
                drop(file);
                if byte_size < 5 || !file_starts_with_pdf_magic(&temporary_path).await? {
                    let _ = tokio::fs::remove_file(&temporary_path).await;
                    return Err(ApiError::BadRequest("uploaded file is not a PDF".into()));
                }
                uploaded = Some(UploadedFile {
                    temporary_path,
                    sha256: format!("{:x}", hasher.finalize()),
                    byte_size,
                });
            }
            Some(name) => {
                return Err(ApiError::BadRequest(format!(
                    "unexpected multipart field: {name}"
                )));
            }
            None => return Err(ApiError::BadRequest("unnamed multipart field".into())),
        }
    }

    let metadata = metadata.ok_or_else(|| ApiError::BadRequest("metadata is required".into()))?;
    let uploaded = uploaded.ok_or_else(|| ApiError::BadRequest("file is required".into()))?;
    let object_key = format!(
        "objects/sha256/{}/{}/{}",
        &uploaded.sha256[0..2],
        &uploaded.sha256[2..4],
        uploaded.sha256
    );
    let destination = state.data_root.join(&object_key);
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if tokio::fs::try_exists(&destination).await? {
        tokio::fs::remove_file(&uploaded.temporary_path).await?;
    } else {
        tokio::fs::rename(&uploaded.temporary_path, &destination).await?;
    }

    let mut tx = state.db.begin().await?;
    let source_id: Uuid = sqlx::query_scalar(
        "INSERT INTO sources \
         (work_id, url, source_kind, reliability, rights_status, license_name, license_url, \
          terms_url, rights_basis, ml_use_permitted, redistribution_permitted) \
         VALUES ($1, $2, $3::source_kind, $4::source_reliability, $5::rights_status, \
                 $6, $7, $8, $9, $10, $11) RETURNING id",
    )
    .bind(metadata.work_id)
    .bind(metadata.source_url.trim())
    .bind(&metadata.source_kind)
    .bind(&metadata.reliability)
    .bind(&metadata.rights_status)
    .bind(metadata.license_name)
    .bind(metadata.license_url)
    .bind(metadata.terms_url)
    .bind(metadata.rights_basis.trim())
    .bind(metadata.ml_use_permitted)
    .bind(metadata.redistribution_permitted)
    .fetch_one(&mut *tx)
    .await?;
    let document_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO documents \
         (id, work_id, source_id, object_key, sha256, media_type, byte_size, rights_status, \
          ml_use_permitted, redistribution_permitted) \
         VALUES ($1, $2, $3, $4, $5, 'application/pdf', $6, $7::rights_status, $8, $9)",
    )
    .bind(document_id)
    .bind(metadata.work_id)
    .bind(source_id)
    .bind(&object_key)
    .bind(&uploaded.sha256)
    .bind(uploaded.byte_size as i64)
    .bind(&metadata.rights_status)
    .bind(metadata.ml_use_permitted)
    .bind(metadata.redistribution_permitted)
    .execute(&mut *tx)
    .await?;
    let extraction_job_id = if metadata.ml_use_permitted {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO jobs (id, job_type, idempotency_key, payload) \
             VALUES ($1, 'extract_document', $2, $3)",
        )
        .bind(id)
        .bind(format!("extract:{document_id}:{}", uploaded.sha256))
        .bind(json!({"document_id": document_id, "object_key": object_key}))
        .execute(&mut *tx)
        .await?;
        Some(id)
    } else {
        None
    };
    sqlx::query(
        "INSERT INTO audit_log (actor, action, entity_type, entity_id, after_value, reason) \
         VALUES ('curator-api', 'upload_document', 'document', $1, $2, $3)",
    )
    .bind(document_id)
    .bind(json!({"sha256": uploaded.sha256, "source_id": source_id}))
    .bind(metadata.rights_basis.trim())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(RegisteredDocument {
            document_id,
            extraction_job_id,
        }),
    ))
}

fn validate_upload_metadata(input: &UploadMetadata) -> Result<(), ApiError> {
    if input.source_url.trim().is_empty() {
        return Err(ApiError::BadRequest("source_url is required".into()));
    }
    validate_source_taxonomy(&input.source_kind, &input.reliability)?;
    validate_source_policy(
        &input.rights_status,
        &input.rights_basis,
        input.license_name.as_deref(),
        input.license_url.as_deref(),
        input.ml_use_permitted,
        input.redistribution_permitted,
    )?;
    if !matches!(
        input.rights_status.as_str(),
        "open_redistributable" | "internal_research_copy"
    ) {
        return Err(ApiError::BadRequest(
            "rights_status does not permit retaining a local document".into(),
        ));
    }
    Ok(())
}

fn validate_source_policy(
    rights_status: &str,
    rights_basis: &str,
    license_name: Option<&str>,
    license_url: Option<&str>,
    ml_use_permitted: bool,
    redistribution_permitted: bool,
) -> Result<(), ApiError> {
    if !matches!(
        rights_status,
        "open_redistributable"
            | "open_metadata_only"
            | "internal_research_copy"
            | "link_only"
            | "unknown_requires_review"
            | "prohibited"
    ) {
        return Err(ApiError::BadRequest("invalid rights_status".into()));
    }
    if rights_basis.trim().len() < 12 {
        return Err(ApiError::BadRequest(
            "a specific rights_basis is required".into(),
        ));
    }
    if ml_use_permitted
        && !matches!(
            rights_status,
            "open_redistributable" | "internal_research_copy"
        )
    {
        return Err(ApiError::BadRequest(
            "the selected rights status does not permit ML processing".into(),
        ));
    }
    if redistribution_permitted
        && !matches!(rights_status, "open_redistributable" | "open_metadata_only")
    {
        return Err(ApiError::BadRequest(
            "the selected rights status does not permit redistribution".into(),
        ));
    }
    if rights_status == "open_redistributable"
        && (license_name.is_none_or(str::is_empty) || license_url.is_none_or(str::is_empty))
    {
        return Err(ApiError::BadRequest(
            "redistributable content requires license_name and license_url".into(),
        ));
    }
    Ok(())
}

fn validate_source_taxonomy(source_kind: &str, reliability: &str) -> Result<(), ApiError> {
    if !matches!(
        source_kind,
        "publisher"
            | "proceedings"
            | "doi_registry"
            | "institutional_repository"
            | "author_page"
            | "preprint_repository"
            | "bibliography"
            | "artifact_repository"
            | "curator_upload"
            | "other"
    ) {
        return Err(ApiError::BadRequest("invalid source_kind".into()));
    }
    if !matches!(
        reliability,
        "primary" | "authoritative_metadata" | "corroborating" | "discovery_only"
    ) {
        return Err(ApiError::BadRequest("invalid reliability".into()));
    }
    Ok(())
}

#[derive(FromRow)]
struct SourcePolicy {
    rights_status: String,
    ml_use_permitted: bool,
    redistribution_permitted: bool,
}

async fn load_source_policy(
    state: &AppState,
    work_id: Uuid,
    source_id: Uuid,
) -> Result<SourcePolicy, ApiError> {
    sqlx::query_as::<_, SourcePolicy>(
        "SELECT rights_status::text AS rights_status, ml_use_permitted, \
         redistribution_permitted FROM sources WHERE id = $1 AND work_id = $2",
    )
    .bind(source_id)
    .bind(work_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(ApiError::NotFound)
}

fn ensure_document_storage_permitted(policy: &SourcePolicy) -> Result<(), ApiError> {
    if !matches!(
        policy.rights_status.as_str(),
        "open_redistributable" | "internal_research_copy"
    ) {
        return Err(ApiError::BadRequest(
            "source policy does not permit retaining a local document".into(),
        ));
    }
    Ok(())
}

async fn file_starts_with_pdf_magic(path: &Path) -> Result<bool, ApiError> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut magic = [0_u8; 5];
    file.read_exact(&mut magic).await?;
    Ok(&magic == b"%PDF-")
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
        let mut tx = state.db.begin().await?;
        let linked = sqlx::query(
            "UPDATE discovery_candidates SET status = 'imported', work_id = $1, \
             reviewed_at = now(), review_reason = 'Linked to an existing canonical work' \
             WHERE provider = 'crossref' AND external_id = $2 AND status = 'pending'",
        )
        .bind(work.id)
        .bind(&doi)
        .execute(&mut *tx)
        .await?;
        if linked.rows_affected() > 0 {
            sqlx::query(
                "INSERT INTO audit_log \
                 (actor, action, entity_type, entity_id, after_value, reason) \
                 VALUES ('crossref-adapter', 'link_existing', 'work', $1, $2, \
                         'Pending discovery candidate matched cached DOI')",
            )
            .bind(work.id)
            .bind(json!({"doi": doi}))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
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
         (work_id, url, source_kind, reliability, rights_status, terms_url, raw_metadata, \
          rights_basis, ml_use_permitted, redistribution_permitted) \
         VALUES ($1, $2, 'doi_registry', 'authoritative_metadata', 'open_metadata_only', \
                 'https://www.crossref.org/documentation/retrieve-metadata/rest-api/', $3, \
                 'Crossref REST API metadata reuse; full text is not acquired.', false, true) \
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
    sqlx::query(
        "UPDATE discovery_candidates SET status = 'imported', work_id = $1, \
         reviewed_at = now(), review_reason = 'Imported through curator DOI lookup' \
         WHERE provider = 'crossref' AND external_id = $2",
    )
    .bind(work.id)
    .bind(&imported.doi)
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

#[derive(Serialize, FromRow)]
struct ProofResultDetail {
    id: Uuid,
    work_id: Uuid,
    source_document_id: Option<Uuid>,
    theorem_name: Option<String>,
    result_kind: String,
    proof_method: Option<String>,
    statement_summary: String,
    status: String,
}

#[derive(Serialize, FromRow)]
struct EvidenceDetail {
    page_start: Option<i32>,
    page_end: Option<i32>,
    char_start: Option<i32>,
    char_end: Option<i32>,
    content_sha256: Option<String>,
    paraphrase: String,
    created_by: String,
}

async fn get_proof_result(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_curator(&state, &headers)?;
    let result = sqlx::query_as::<_, ProofResultDetail>(
        "SELECT id, work_id, source_document_id, theorem_name, result_kind, proof_method, \
         statement_summary, status::text AS status FROM proof_results WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(ApiError::NotFound)?;
    let evidence = sqlx::query_as::<_, EvidenceDetail>(
        "SELECT ds.page_start, ds.page_end, ds.char_start, ds.char_end, ds.content_sha256, \
         e.paraphrase, e.created_by FROM evidence e \
         JOIN document_spans ds ON ds.id = e.document_span_id \
         WHERE e.proof_result_id = $1 ORDER BY ds.page_start, ds.char_start",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({"proof_result": result, "evidence": evidence})))
}

#[derive(Deserialize)]
struct CreateCuratedProof {
    work_id: Uuid,
    document_id: Uuid,
    theorem_name: Option<String>,
    result_kind: String,
    proof_method: Option<String>,
    statement_summary: String,
    obligations: Vec<String>,
    page_start: i32,
    page_end: Option<i32>,
    char_start: Option<i32>,
    char_end: Option<i32>,
    content_sha256: Option<String>,
    evidence_paraphrase: String,
    actor: String,
    reason: String,
}

async fn create_curated_proof(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateCuratedProof>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    require_curator(&state, &headers)?;
    validate_curated_proof(&input)?;
    let mut tx = state.db.begin().await?;
    let reliable_document_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM documents d JOIN sources s ON s.id = d.source_id \
         WHERE d.id = $1 AND d.work_id = $2 AND s.reliability = 'primary')",
    )
    .bind(input.document_id)
    .bind(input.work_id)
    .fetch_one(&mut *tx)
    .await?;
    if !reliable_document_exists {
        return Err(ApiError::BadRequest(
            "curated proof evidence must use a primary source document for this work".into(),
        ));
    }
    let proof_result_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO proof_results \
         (id, work_id, theorem_name, result_kind, proof_method, statement_summary, status, \
          source_document_id) \
         VALUES ($1, $2, $3, $4, $5, $6, 'curator_approved', $7)",
    )
    .bind(proof_result_id)
    .bind(input.work_id)
    .bind(input.theorem_name.as_deref().map(str::trim))
    .bind(&input.result_kind)
    .bind(input.proof_method.as_deref().map(str::trim))
    .bind(input.statement_summary.trim())
    .bind(input.document_id)
    .execute(&mut *tx)
    .await?;
    for obligation in &input.obligations {
        sqlx::query(
            "INSERT INTO proof_obligations (proof_result_id, obligation_kind) VALUES ($1, $2)",
        )
        .bind(proof_result_id)
        .bind(obligation)
        .execute(&mut *tx)
        .await?;
    }
    let span_id: Uuid = sqlx::query_scalar(
        "INSERT INTO document_spans \
         (document_id, page_start, page_end, char_start, char_end, content_sha256) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(input.document_id)
    .bind(input.page_start)
    .bind(input.page_end.unwrap_or(input.page_start))
    .bind(input.char_start)
    .bind(input.char_end)
    .bind(input.content_sha256.as_deref().map(str::to_ascii_lowercase))
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO evidence \
         (proof_result_id, document_span_id, paraphrase, created_by) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(proof_result_id)
    .bind(span_id)
    .bind(input.evidence_paraphrase.trim())
    .bind(input.actor.trim())
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO audit_log \
         (actor, action, entity_type, entity_id, after_value, reason) \
         VALUES ($1, 'create_curated_proof', 'proof_result', $2, $3, $4)",
    )
    .bind(input.actor.trim())
    .bind(proof_result_id)
    .bind(json!({"status": "curator_approved", "document_id": input.document_id}))
    .bind(input.reason.trim())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"proof_result_id": proof_result_id})),
    ))
}

fn validate_curated_proof(input: &CreateCuratedProof) -> Result<(), ApiError> {
    if !matches!(
        input.result_kind.as_str(),
        "progress"
            | "preservation"
            | "progress_and_preservation"
            | "type_soundness"
            | "subject_reduction"
            | "semantic_soundness"
            | "logical_relation_soundness"
            | "other"
    ) {
        return Err(ApiError::BadRequest("invalid result_kind".into()));
    }
    if input.proof_method.as_deref().is_some_and(|method| {
        !matches!(
            method,
            "syntactic"
                | "semantic"
                | "logical_relations"
                | "reducibility"
                | "step_indexed"
                | "mechanized"
                | "mixed"
                | "other"
        )
    }) {
        return Err(ApiError::BadRequest("invalid proof_method".into()));
    }
    if input.statement_summary.trim().is_empty() || input.statement_summary.len() > 1000 {
        return Err(ApiError::BadRequest(
            "statement_summary must contain 1-1000 characters".into(),
        ));
    }
    if input.evidence_paraphrase.trim().is_empty() || input.evidence_paraphrase.len() > 500 {
        return Err(ApiError::BadRequest(
            "evidence_paraphrase must contain 1-500 characters".into(),
        ));
    }
    if input.actor.trim().is_empty() || input.reason.trim().is_empty() {
        return Err(ApiError::BadRequest("actor and reason are required".into()));
    }
    let allowed_obligations = [
        "progress",
        "preservation",
        "type_soundness",
        "canonical_forms",
        "substitution",
        "weakening",
        "other",
    ];
    if input.obligations.is_empty()
        || input
            .obligations
            .iter()
            .any(|value| !allowed_obligations.contains(&value.as_str()))
        || input
            .obligations
            .iter()
            .enumerate()
            .any(|(index, value)| input.obligations[..index].contains(value))
    {
        return Err(ApiError::BadRequest("invalid proof obligations".into()));
    }
    if input.page_start < 1 || input.page_end.is_some_and(|page| page < input.page_start) {
        return Err(ApiError::BadRequest("invalid page range".into()));
    }
    if input.char_start.is_some_and(|offset| offset < 0)
        || input.char_end.is_some_and(|offset| offset < 0)
        || matches!((input.char_start, input.char_end), (Some(start), Some(end)) if end < start)
    {
        return Err(ApiError::BadRequest("invalid character range".into()));
    }
    if input.content_sha256.as_ref().is_some_and(|digest| {
        digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err(ApiError::BadRequest("invalid content_sha256".into()));
    }
    Ok(())
}

#[derive(Serialize, FromRow)]
struct TimelineEntry {
    work_id: Uuid,
    title: String,
    publication_year: Option<i32>,
    doi: Option<String>,
    proof_result_id: Uuid,
    theorem_name: Option<String>,
    result_kind: String,
    proof_method: Option<String>,
    statement_summary: String,
}

async fn timeline(
    State(state): State<AppState>,
    Query(page): Query<Pagination>,
) -> Result<Json<Vec<TimelineEntry>>, ApiError> {
    let entries = sqlx::query_as::<_, TimelineEntry>(
        "SELECT w.id AS work_id, w.title, w.publication_year, w.doi, \
         pr.id AS proof_result_id, pr.theorem_name, pr.result_kind, pr.proof_method, \
         pr.statement_summary FROM proof_results pr JOIN works w ON w.id = pr.work_id \
         WHERE pr.status = 'curator_approved' \
         ORDER BY w.publication_year NULLS LAST, w.title, pr.id LIMIT $1 OFFSET $2",
    )
    .bind(page.limit.clamp(1, 200))
    .bind(page.offset.max(0))
    .fetch_all(&state.db)
    .await?;
    Ok(Json(entries))
}

#[derive(Deserialize)]
struct CreateExport {
    requested_by: String,
}

#[derive(Serialize, FromRow)]
struct ExportRecord {
    id: Uuid,
    export_kind: String,
    status: String,
    object_key: Option<String>,
    sha256: Option<String>,
    row_count: Option<i64>,
    requested_by: String,
    error: Option<String>,
    created_at: String,
    finished_at: Option<String>,
}

async fn create_export(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateExport>,
) -> Result<(StatusCode, Json<ExportRecord>), ApiError> {
    require_curator(&state, &headers)?;
    if input.requested_by.trim().is_empty() {
        return Err(ApiError::BadRequest("requested_by is required".into()));
    }
    let mut tx = state.db.begin().await?;
    let record = sqlx::query_as::<_, ExportRecord>(
        "INSERT INTO exports (export_kind, requested_by) \
         VALUES ('approved_proofs_parquet', $1) \
         RETURNING id, export_kind, status::text AS status, object_key, sha256, row_count, \
         requested_by, error, created_at::text AS created_at, finished_at::text AS finished_at",
    )
    .bind(input.requested_by.trim())
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO jobs (job_type, idempotency_key, payload) \
         VALUES ('export_approved_proofs_parquet', $1, $2)",
    )
    .bind(format!("export:approved-proofs:parquet:{}", record.id))
    .bind(json!({"export_id": record.id}))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((StatusCode::ACCEPTED, Json(record)))
}

async fn get_export(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<Uuid>,
) -> Result<Json<ExportRecord>, ApiError> {
    require_curator(&state, &headers)?;
    let record = sqlx::query_as::<_, ExportRecord>(
        "SELECT id, export_kind, status::text AS status, object_key, sha256, row_count, \
         requested_by, error, created_at::text AS created_at, finished_at::text AS finished_at \
         FROM exports WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(ApiError::NotFound)?;
    Ok(Json(record))
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

    #[test]
    fn rights_policy_fails_closed() {
        assert!(
            validate_source_policy(
                "open_metadata_only",
                "Crossref metadata only",
                None,
                None,
                true,
                true,
            )
            .is_err()
        );
        assert!(
            validate_source_policy(
                "open_redistributable",
                "Publisher page states CC BY 4.0",
                Some("CC BY 4.0"),
                Some("https://creativecommons.org/licenses/by/4.0/"),
                true,
                true,
            )
            .is_ok()
        );
    }
}
