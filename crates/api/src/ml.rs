use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::FromRow;
use uuid::Uuid;

use crate::{AppState, auth::require_curator, routes::ApiError};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/documents/{id}/similarities",
            get(document_similarities),
        )
        .route("/api/v1/citation-candidates", get(citation_candidates))
        .route(
            "/api/v1/citation-candidates/{id}/resolve",
            post(resolve_citation_candidate),
        )
        .route(
            "/api/v1/entity-resolution-suggestions",
            get(entity_resolution_suggestions),
        )
        .route(
            "/api/v1/entity-resolution-suggestions/{id}/resolve",
            post(resolve_entity_suggestion),
        )
        .route("/api/v1/ml/labels", post(create_label))
        .route(
            "/api/v1/ml/evaluations",
            get(list_evaluations).post(queue_evaluation),
        )
        .route("/api/v1/ml/jobs/{kind}", post(queue_ml_job))
}

#[derive(Serialize, FromRow)]
struct DocumentSimilarity {
    document_id: Uuid,
    score: f32,
    algorithm: String,
}

async fn document_similarities(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<DocumentSimilarity>>, ApiError> {
    require_curator(&state, &headers).await?;
    let rows = sqlx::query_as::<_, DocumentSimilarity>(
        "SELECT CASE WHEN left_document_id = $1 THEN right_document_id \
                ELSE left_document_id END AS document_id, score, algorithm \
         FROM document_similarities \
         WHERE left_document_id = $1 OR right_document_id = $1 \
         ORDER BY score DESC",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
struct Page {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
}

fn default_limit() -> i64 {
    50
}

#[derive(Serialize, FromRow)]
struct CitationCandidate {
    id: Uuid,
    citing_work_id: Uuid,
    source_document_id: Uuid,
    page: i32,
    cited_doi: String,
    context_sha256: String,
    status: String,
}

async fn citation_candidates(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(page): Query<Page>,
) -> Result<Json<Vec<CitationCandidate>>, ApiError> {
    require_curator(&state, &headers).await?;
    let rows = sqlx::query_as::<_, CitationCandidate>(
        "SELECT id, citing_work_id, source_document_id, page, cited_doi, \
         context_sha256, status::text AS status FROM citation_candidates \
         WHERE status = 'machine_candidate' ORDER BY created_at, id LIMIT $1 OFFSET $2",
    )
    .bind(page.limit.clamp(1, 200))
    .bind(page.offset.max(0))
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
struct Resolution {
    decision: String,
    #[serde(default, rename = "actor")]
    _actor: String,
    reason: String,
}

async fn resolve_citation_candidate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<Resolution>,
) -> Result<StatusCode, ApiError> {
    let principal = require_curator(&state, &headers).await?;
    validate_resolution(&input)?;
    let mut tx = state.db.begin().await?;
    let row: (Uuid, Uuid, String) = sqlx::query_as(
        "SELECT citing_work_id, source_document_id, cited_doi \
         FROM citation_candidates WHERE id = $1 AND status = 'machine_candidate' FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;
    let status = if input.decision == "accept" {
        let cited_work_id: Uuid = sqlx::query_scalar("SELECT id FROM works WHERE doi = $1")
            .bind(&row.2)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| ApiError::BadRequest("cited DOI has not been imported".into()))?;
        if cited_work_id == row.0 {
            return Err(ApiError::BadRequest("a work cannot cite itself".into()));
        }
        sqlx::query(
            "INSERT INTO citations \
             (citing_work_id, cited_work_id, source_document_id, status, created_by, review_reason) \
             VALUES ($1, $2, $3, 'curator_approved', $4, $5) \
             ON CONFLICT (citing_work_id, cited_work_id) DO UPDATE SET \
                status = 'curator_approved', source_document_id = EXCLUDED.source_document_id, \
                created_by = EXCLUDED.created_by, review_reason = EXCLUDED.review_reason",
        )
        .bind(row.0)
        .bind(cited_work_id)
        .bind(row.1)
        .bind(principal.username())
        .bind(input.reason.trim())
        .execute(&mut *tx)
        .await?;
        "curator_approved"
    } else {
        "rejected"
    };
    sqlx::query("UPDATE citation_candidates SET status = $2::assertion_status WHERE id = $1")
        .bind(id)
        .bind(status)
        .execute(&mut *tx)
        .await?;
    audit_resolution(
        &mut tx,
        principal.username(),
        &input,
        "citation_candidate",
        id,
        status,
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize, FromRow)]
struct EntitySuggestion {
    id: Uuid,
    discovery_candidate_id: Uuid,
    work_id: Uuid,
    score: f32,
    method: String,
    rationale: Value,
}

async fn entity_resolution_suggestions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(page): Query<Page>,
) -> Result<Json<Vec<EntitySuggestion>>, ApiError> {
    require_curator(&state, &headers).await?;
    let rows = sqlx::query_as::<_, EntitySuggestion>(
        "SELECT id, discovery_candidate_id, work_id, score, method, rationale \
         FROM entity_resolution_suggestions WHERE status = 'pending' \
         ORDER BY score DESC LIMIT $1 OFFSET $2",
    )
    .bind(page.limit.clamp(1, 200))
    .bind(page.offset.max(0))
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

async fn resolve_entity_suggestion(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<Resolution>,
) -> Result<StatusCode, ApiError> {
    let principal = require_curator(&state, &headers).await?;
    validate_resolution(&input)?;
    let status = if input.decision == "accept" {
        "accepted"
    } else {
        "rejected"
    };
    let mut tx = state.db.begin().await?;
    let suggestion: (Uuid, Uuid) = sqlx::query_as(
        "UPDATE entity_resolution_suggestions SET status = $2::resolution_status, \
         resolved_by = $3, resolution_reason = $4, resolved_at = now() \
         WHERE id = $1 AND status = 'pending' \
         RETURNING discovery_candidate_id, work_id",
    )
    .bind(id)
    .bind(status)
    .bind(principal.username())
    .bind(input.reason.trim())
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;
    if status == "accepted" {
        sqlx::query(
            "UPDATE discovery_candidates SET status = 'imported', work_id = $2, \
             reviewed_at = now(), review_reason = $3 WHERE id = $1 AND status = 'pending'",
        )
        .bind(suggestion.0)
        .bind(suggestion.1)
        .bind(input.reason.trim())
        .execute(&mut *tx)
        .await?;
    }
    audit_resolution(
        &mut tx,
        principal.username(),
        &input,
        "entity_resolution_suggestion",
        id,
        status,
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

fn validate_resolution(input: &Resolution) -> Result<(), ApiError> {
    if !matches!(input.decision.as_str(), "accept" | "reject") {
        return Err(ApiError::BadRequest(
            "decision must be accept or reject".into(),
        ));
    }
    if input.reason.trim().is_empty() {
        return Err(ApiError::BadRequest("reason is required".into()));
    }
    Ok(())
}

async fn audit_resolution(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: &str,
    input: &Resolution,
    entity_type: &str,
    id: Uuid,
    status: &str,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO audit_log \
         (actor, action, entity_type, entity_id, after_value, reason) \
         VALUES ($1, 'resolve', $2, $3, $4, $5)",
    )
    .bind(actor)
    .bind(entity_type)
    .bind(id)
    .bind(json!({"status": status}))
    .bind(input.reason.trim())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[derive(Deserialize)]
struct CreateLabel {
    proof_result_id: Uuid,
    expected_result_kind: String,
    is_relevant: bool,
    #[serde(default, rename = "labelled_by")]
    _labelled_by: String,
    notes: Option<String>,
}

async fn create_label(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateLabel>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let principal = require_curator(&state, &headers).await?;
    if input.expected_result_kind.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "expected_result_kind is required".into(),
        ));
    }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO ml_labels \
         (proof_result_id, expected_result_kind, is_relevant, labelled_by, notes) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (proof_result_id, labelled_by) DO UPDATE SET \
            expected_result_kind = EXCLUDED.expected_result_kind, \
            is_relevant = EXCLUDED.is_relevant, notes = EXCLUDED.notes \
         RETURNING id",
    )
    .bind(input.proof_result_id)
    .bind(input.expected_result_kind.trim())
    .bind(input.is_relevant)
    .bind(principal.username())
    .bind(input.notes)
    .fetch_one(&state.db)
    .await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))))
}

#[derive(Serialize, FromRow)]
struct Evaluation {
    id: Uuid,
    pipeline_version: String,
    labelled_examples: i32,
    true_positives: i32,
    false_positives: i32,
    false_negatives: i32,
    precision: Option<f64>,
    recall: Option<f64>,
    f1: Option<f64>,
    details: Value,
    created_at: String,
}

async fn list_evaluations(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(page): Query<Page>,
) -> Result<Json<Vec<Evaluation>>, ApiError> {
    require_curator(&state, &headers).await?;
    let rows = sqlx::query_as::<_, Evaluation>(
        "SELECT id, pipeline_version, labelled_examples, true_positives, false_positives, \
         false_negatives, precision, recall, f1, details, created_at::text AS created_at \
         FROM ml_evaluations ORDER BY created_at DESC LIMIT $1 OFFSET $2",
    )
    .bind(page.limit.clamp(1, 200))
    .bind(page.offset.max(0))
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

async fn queue_evaluation(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    require_curator(&state, &headers).await?;
    queue_job(&state, "evaluate_local_classifier").await
}

async fn queue_ml_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(kind): Path<String>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    require_curator(&state, &headers).await?;
    let job_type = match kind.as_str() {
        "entity-resolution" => "resolve_discovery_entities",
        "duplicate-detection" => "resolve_work_duplicates",
        _ => return Err(ApiError::NotFound),
    };
    queue_job(&state, job_type).await
}

async fn queue_job(
    state: &AppState,
    job_type: &str,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO jobs (id, job_type, idempotency_key, payload) VALUES ($1, $2, $3, '{}')",
    )
    .bind(id)
    .bind(job_type)
    .bind(format!("{job_type}:{id}"))
    .execute(&state.db)
    .await?;
    Ok((StatusCode::ACCEPTED, Json(json!({"job_id": id}))))
}
