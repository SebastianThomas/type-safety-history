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
        .route("/api/v1/work-versions", post(create_work_version))
        .route("/api/v1/work-relations", post(create_work_relation))
        .route("/api/v1/citations", post(create_citation))
        .route("/api/v1/formal-artifacts", post(create_artifact))
        .route(
            "/api/v1/formal-artifacts/{id}/verify",
            post(verify_artifact),
        )
        .route("/api/v1/proof-results/{id}/features", post(assign_features))
        .route(
            "/api/v1/proof-results/{id}/type-systems",
            post(assign_type_systems),
        )
        .route("/api/v1/research-annotations", post(create_annotation))
        .route(
            "/api/v1/works/{id}/scholarly-status",
            post(set_scholarly_status),
        )
        .route("/api/v1/works/{id}/research", get(get_work_research))
        .route("/api/v1/coverage", get(list_coverage))
        .route(
            "/api/v1/duplicate-suggestions",
            get(list_duplicate_suggestions),
        )
        .route(
            "/api/v1/duplicate-suggestions/{id}/resolve",
            post(resolve_duplicate),
        )
}

#[derive(Deserialize)]
struct CreateWorkVersion {
    work_id: Uuid,
    version_label: String,
    published_at: Option<String>,
    #[serde(default)]
    is_canonical: bool,
    #[serde(default)]
    actor: String,
    reason: String,
}

async fn create_work_version(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateWorkVersion>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let principal = require_curator(&state, &headers).await?;
    require_actor_reason(&input.actor, &input.reason)?;
    if input.version_label.trim().is_empty() {
        return Err(ApiError::BadRequest("version_label is required".into()));
    }
    let mut tx = state.db.begin().await?;
    if input.is_canonical {
        sqlx::query("UPDATE work_versions SET is_canonical = false WHERE work_id = $1")
            .bind(input.work_id)
            .execute(&mut *tx)
            .await?;
    }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO work_versions (work_id, version_label, published_at, is_canonical) \
         VALUES ($1, $2, $3::date, $4) RETURNING id",
    )
    .bind(input.work_id)
    .bind(input.version_label.trim())
    .bind(input.published_at)
    .bind(input.is_canonical)
    .fetch_one(&mut *tx)
    .await?;
    audit(
        &mut tx,
        principal.username(),
        "create_version",
        "work_version",
        id,
        json!({
            "work_id": input.work_id, "canonical": input.is_canonical
        }),
        &input.reason,
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))))
}

#[derive(Deserialize)]
struct CreateWorkRelation {
    from_work_id: Uuid,
    to_work_id: Uuid,
    relation_kind: String,
    evidence_source_id: Option<Uuid>,
    notes: Option<String>,
    #[serde(default)]
    actor: String,
    reason: String,
}

async fn create_work_relation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateWorkRelation>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let principal = require_curator(&state, &headers).await?;
    require_actor_reason(&input.actor, &input.reason)?;
    if !matches!(
        input.relation_kind.as_str(),
        "preprint_of"
            | "conference_version_of"
            | "journal_version_of"
            | "corrects"
            | "retracts"
            | "extends"
            | "mechanizes"
            | "other"
    ) {
        return Err(ApiError::BadRequest("invalid relation_kind".into()));
    }
    let mut tx = state.db.begin().await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO work_relations \
         (from_work_id, to_work_id, relation_kind, evidence_source_id, notes, created_by) \
         VALUES ($1, $2, $3::work_relation_kind, $4, $5, $6) RETURNING id",
    )
    .bind(input.from_work_id)
    .bind(input.to_work_id)
    .bind(&input.relation_kind)
    .bind(input.evidence_source_id)
    .bind(input.notes.as_deref().map(str::trim))
    .bind(principal.username())
    .fetch_one(&mut *tx)
    .await?;
    audit(
        &mut tx,
        principal.username(),
        "create_relation",
        "work_relation",
        id,
        json!({
            "from": input.from_work_id, "to": input.to_work_id, "kind": input.relation_kind
        }),
        &input.reason,
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))))
}

#[derive(Deserialize)]
struct CreateCitation {
    citing_work_id: Uuid,
    cited_work_id: Uuid,
    source_document_id: Option<Uuid>,
    evidence_span_id: Option<Uuid>,
    #[serde(default)]
    actor: String,
    reason: String,
}

async fn create_citation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateCitation>,
) -> Result<StatusCode, ApiError> {
    let principal = require_curator(&state, &headers).await?;
    require_actor_reason(&input.actor, &input.reason)?;
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "INSERT INTO citations \
         (citing_work_id, cited_work_id, source_document_id, evidence_span_id, status, \
          created_by, review_reason) \
         VALUES ($1, $2, $3, $4, 'curator_approved', $5, $6) \
         ON CONFLICT (citing_work_id, cited_work_id) DO UPDATE SET \
             source_document_id = EXCLUDED.source_document_id, \
             evidence_span_id = EXCLUDED.evidence_span_id, status = 'curator_approved', \
             created_by = EXCLUDED.created_by, review_reason = EXCLUDED.review_reason",
    )
    .bind(input.citing_work_id)
    .bind(input.cited_work_id)
    .bind(input.source_document_id)
    .bind(input.evidence_span_id)
    .bind(principal.username())
    .bind(input.reason.trim())
    .execute(&mut *tx)
    .await?;
    audit(
        &mut tx,
        principal.username(),
        "approve",
        "citation",
        input.citing_work_id,
        json!({
            "cited_work_id": input.cited_work_id
        }),
        &input.reason,
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct CreateArtifact {
    work_id: Uuid,
    artifact_kind: String,
    url: String,
    repository_commit: Option<String>,
    proof_assistant: Option<String>,
    proof_assistant_version: Option<String>,
    license_name: Option<String>,
    license_url: Option<String>,
    archived_url: Option<String>,
    sha256: Option<String>,
    #[serde(default)]
    actor: String,
    reason: String,
}

async fn create_artifact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateArtifact>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let principal = require_curator(&state, &headers).await?;
    require_actor_reason(&input.actor, &input.reason)?;
    if input.url.trim().is_empty()
        || !matches!(
            input.artifact_kind.as_str(),
            "source_repository"
                | "proof_source"
                | "archived_snapshot"
                | "build_manifest"
                | "build_log"
                | "supplement"
                | "other"
        )
    {
        return Err(ApiError::BadRequest("invalid artifact".into()));
    }
    validate_optional_sha256(input.sha256.as_deref())?;
    let mut tx = state.db.begin().await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO formal_artifacts \
         (work_id, artifact_kind, url, repository_commit, proof_assistant, \
          proof_assistant_version, license_name, license_url, archived_url, sha256) \
         VALUES ($1, $2::artifact_kind, $3, $4, $5, $6, $7, $8, $9, lower($10)) \
         RETURNING id",
    )
    .bind(input.work_id)
    .bind(&input.artifact_kind)
    .bind(input.url.trim())
    .bind(input.repository_commit)
    .bind(input.proof_assistant)
    .bind(input.proof_assistant_version)
    .bind(input.license_name)
    .bind(input.license_url)
    .bind(input.archived_url)
    .bind(input.sha256)
    .fetch_one(&mut *tx)
    .await?;
    audit(
        &mut tx,
        principal.username(),
        "create",
        "formal_artifact",
        id,
        json!({
            "work_id": input.work_id, "kind": input.artifact_kind
        }),
        &input.reason,
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))))
}

#[derive(Deserialize)]
struct VerifyArtifact {
    #[serde(default)]
    actor: String,
    notes: String,
}

async fn verify_artifact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<VerifyArtifact>,
) -> Result<StatusCode, ApiError> {
    let principal = require_curator(&state, &headers).await?;
    require_actor_reason(&input.actor, &input.notes)?;
    let mut tx = state.db.begin().await?;
    let updated = sqlx::query(
        "UPDATE formal_artifacts SET status = 'verified', verification_notes = $2, \
         verified_by = $3, verified_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(input.notes.trim())
    .bind(principal.username())
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    audit(
        &mut tx,
        principal.username(),
        "verify",
        "formal_artifact",
        id,
        json!({
            "status": "verified"
        }),
        &input.notes,
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct AssignFeatures {
    feature_slugs: Vec<String>,
    #[serde(default)]
    actor: String,
    reason: String,
}

async fn assign_features(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<AssignFeatures>,
) -> Result<StatusCode, ApiError> {
    let principal = require_curator(&state, &headers).await?;
    require_actor_reason(&input.actor, &input.reason)?;
    if input.feature_slugs.is_empty() {
        return Err(ApiError::BadRequest("feature_slugs is required".into()));
    }
    let mut tx = state.db.begin().await?;
    for slug in &input.feature_slugs {
        let inserted = sqlx::query(
            "INSERT INTO proof_result_features (proof_result_id, feature_id, created_by) \
             SELECT $1, id, $3 FROM language_features WHERE slug = $2 \
             ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(slug)
        .bind(principal.username())
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM language_features WHERE slug = $1)",
            )
            .bind(slug)
            .fetch_one(&mut *tx)
            .await?;
            if !exists {
                return Err(ApiError::BadRequest(format!("unknown feature: {slug}")));
            }
        }
    }
    audit(
        &mut tx,
        principal.username(),
        "assign_features",
        "proof_result",
        id,
        json!({
            "features": input.feature_slugs
        }),
        &input.reason,
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct AssignTypeSystems {
    type_systems: Vec<String>,
    #[serde(default)]
    actor: String,
    reason: String,
}

async fn assign_type_systems(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<AssignTypeSystems>,
) -> Result<StatusCode, ApiError> {
    let principal = require_curator(&state, &headers).await?;
    require_actor_reason(&input.actor, &input.reason)?;
    if input.type_systems.is_empty() {
        return Err(ApiError::BadRequest("type_systems is required".into()));
    }
    let mut tx = state.db.begin().await?;
    for name in &input.type_systems {
        if name.trim().is_empty() {
            return Err(ApiError::BadRequest("empty type-system name".into()));
        }
        let type_system_id: Uuid = sqlx::query_scalar(
            "INSERT INTO type_systems (name) VALUES ($1) \
             ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name RETURNING id",
        )
        .bind(name.trim())
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO proof_result_type_systems \
             (proof_result_id, type_system_id, created_by) VALUES ($1, $2, $3) \
             ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(type_system_id)
        .bind(principal.username())
        .execute(&mut *tx)
        .await?;
    }
    audit(
        &mut tx,
        principal.username(),
        "assign_type_systems",
        "proof_result",
        id,
        json!({
            "type_systems": input.type_systems
        }),
        &input.reason,
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct CreateAnnotation {
    work_id: Option<Uuid>,
    proof_result_id: Option<Uuid>,
    annotation_kind: String,
    summary: String,
    evidence_source_id: Option<Uuid>,
    evidence_document_span_id: Option<Uuid>,
    #[serde(default)]
    actor: String,
    reason: String,
}

async fn create_annotation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateAnnotation>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let principal = require_curator(&state, &headers).await?;
    require_actor_reason(&input.actor, &input.reason)?;
    if input.work_id.is_some() == input.proof_result_id.is_some()
        || input.summary.trim().is_empty()
        || !matches!(
            input.annotation_kind.as_str(),
            "curator_note"
                | "correction"
                | "retraction"
                | "counterexample"
                | "dispute"
                | "priority_claim"
                | "other"
        )
    {
        return Err(ApiError::BadRequest("invalid research annotation".into()));
    }
    let mut tx = state.db.begin().await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO research_annotations \
         (work_id, proof_result_id, annotation_kind, summary, evidence_source_id, \
          evidence_document_span_id, created_by) \
         VALUES ($1, $2, $3::annotation_kind, $4, $5, $6, $7) RETURNING id",
    )
    .bind(input.work_id)
    .bind(input.proof_result_id)
    .bind(&input.annotation_kind)
    .bind(input.summary.trim())
    .bind(input.evidence_source_id)
    .bind(input.evidence_document_span_id)
    .bind(principal.username())
    .fetch_one(&mut *tx)
    .await?;
    audit(
        &mut tx,
        principal.username(),
        "create",
        "research_annotation",
        id,
        json!({
            "kind": input.annotation_kind
        }),
        &input.reason,
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))))
}

#[derive(Deserialize)]
struct SetScholarlyStatus {
    status: String,
    #[serde(default)]
    actor: String,
    reason: String,
}

async fn set_scholarly_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<SetScholarlyStatus>,
) -> Result<StatusCode, ApiError> {
    let principal = require_curator(&state, &headers).await?;
    require_actor_reason(&input.actor, &input.reason)?;
    if !matches!(
        input.status.as_str(),
        "active" | "corrected" | "retracted" | "disputed"
    ) {
        return Err(ApiError::BadRequest("invalid scholarly status".into()));
    }
    let mut tx = state.db.begin().await?;
    let updated = sqlx::query(
        "UPDATE works SET scholarly_status = $2::scholarly_status, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(&input.status)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    audit(
        &mut tx,
        principal.username(),
        "set_status",
        "work",
        id,
        json!({
            "scholarly_status": input.status
        }),
        &input.reason,
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_work_research(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_curator(&state, &headers).await?;
    let work: Option<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id', id, 'title', title, 'doi', doi, \
         'publication_year', publication_year, 'review_status', review_status, \
         'scholarly_status', scholarly_status) FROM works WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    let work = work.ok_or(ApiError::NotFound)?;
    let versions: Value = json_rows(&state, "SELECT to_jsonb(v) FROM work_versions v WHERE work_id = $1 ORDER BY published_at NULLS LAST", id).await?;
    let relations: Value = json_rows(&state, "SELECT to_jsonb(r) FROM work_relations r WHERE from_work_id = $1 OR to_work_id = $1 ORDER BY created_at", id).await?;
    let artifacts: Value = json_rows(
        &state,
        "SELECT to_jsonb(a) FROM formal_artifacts a WHERE work_id = $1 ORDER BY created_at",
        id,
    )
    .await?;
    let annotations: Value = json_rows(
        &state,
        "SELECT to_jsonb(a) FROM research_annotations a WHERE work_id = $1 ORDER BY created_at",
        id,
    )
    .await?;
    let citations_out: Value = json_rows(
        &state,
        "SELECT to_jsonb(c) FROM citations c WHERE citing_work_id = $1",
        id,
    )
    .await?;
    let citations_in: Value = json_rows(
        &state,
        "SELECT to_jsonb(c) FROM citations c WHERE cited_work_id = $1",
        id,
    )
    .await?;
    Ok(Json(json!({
        "work": work,
        "versions": versions,
        "relations": relations,
        "formal_artifacts": artifacts,
        "annotations": annotations,
        "citations_out": citations_out,
        "citations_in": citations_in
    })))
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
struct CoverageRecord {
    id: Uuid,
    provider: String,
    query_text: String,
    from_year: i32,
    until_year: i32,
    requested_rows: i32,
    result_count: i32,
    started_at: String,
    finished_at: String,
}

async fn list_coverage(
    State(state): State<AppState>,
    Query(page): Query<Page>,
) -> Result<Json<Vec<CoverageRecord>>, ApiError> {
    let rows = sqlx::query_as::<_, CoverageRecord>(
        "SELECT id, provider, query_text, from_year, until_year, requested_rows, result_count, \
         started_at::text AS started_at, finished_at::text AS finished_at \
         FROM coverage_runs ORDER BY started_at DESC LIMIT $1 OFFSET $2",
    )
    .bind(page.limit.clamp(1, 200))
    .bind(page.offset.max(0))
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

#[derive(Serialize, FromRow)]
struct DuplicateSuggestion {
    id: Uuid,
    left_work_id: Uuid,
    right_work_id: Uuid,
    score: f64,
    method: String,
    rationale: Value,
    status: String,
}

async fn list_duplicate_suggestions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(page): Query<Page>,
) -> Result<Json<Vec<DuplicateSuggestion>>, ApiError> {
    require_curator(&state, &headers).await?;
    let rows = sqlx::query_as::<_, DuplicateSuggestion>(
        "SELECT id, left_work_id, right_work_id, score, method, rationale, \
         status::text AS status FROM duplicate_suggestions WHERE status = 'pending' \
         ORDER BY score DESC LIMIT $1 OFFSET $2",
    )
    .bind(page.limit.clamp(1, 200))
    .bind(page.offset.max(0))
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
struct ResolveDuplicate {
    decision: String,
    canonical_work_id: Option<Uuid>,
    #[serde(default)]
    actor: String,
    reason: String,
}

async fn resolve_duplicate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<ResolveDuplicate>,
) -> Result<StatusCode, ApiError> {
    let principal = require_curator(&state, &headers).await?;
    require_actor_reason(&input.actor, &input.reason)?;
    let mut tx = state.db.begin().await?;
    let suggestion: (Uuid, Uuid) = sqlx::query_as(
        "SELECT left_work_id, right_work_id FROM duplicate_suggestions \
         WHERE id = $1 AND status = 'pending' FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;
    let mut merged_duplicate = None;
    let (status, canonical) = match input.decision.as_str() {
        "accept" => {
            let canonical = input
                .canonical_work_id
                .ok_or_else(|| ApiError::BadRequest("canonical_work_id is required".into()))?;
            if canonical != suggestion.0 && canonical != suggestion.1 {
                return Err(ApiError::BadRequest(
                    "canonical_work_id must be one of the suggested works".into(),
                ));
            }
            let duplicate = if canonical == suggestion.0 {
                suggestion.1
            } else {
                suggestion.0
            };
            merge_work(&mut tx, duplicate, canonical, principal.username()).await?;
            merged_duplicate = Some(duplicate);
            ("accepted", Some(canonical))
        }
        "reject" => ("rejected", None),
        _ => return Err(ApiError::BadRequest("invalid decision".into())),
    };
    let updated = sqlx::query(
        "UPDATE duplicate_suggestions SET status = $2::resolution_status, \
         canonical_work_id = $3, resolved_by = $4, resolution_reason = $5, resolved_at = now() \
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(id)
    .bind(status)
    .bind(canonical)
    .bind(principal.username())
    .bind(input.reason.trim())
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    if let Some(duplicate) = merged_duplicate {
        sqlx::query(
            "UPDATE duplicate_suggestions SET status = 'rejected', resolved_by = $3, \
             resolution_reason = 'Superseded because one work was merged', resolved_at = now() \
             WHERE id <> $1 AND status = 'pending' \
             AND (left_work_id = $2 OR right_work_id = $2)",
        )
        .bind(id)
        .bind(duplicate)
        .bind(principal.username())
        .execute(&mut *tx)
        .await?;
    }
    audit(
        &mut tx,
        principal.username(),
        "resolve",
        "duplicate_suggestion",
        id,
        json!({
            "status": status, "canonical_work_id": canonical
        }),
        &input.reason,
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn merge_work(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    duplicate: Uuid,
    canonical: Uuid,
    actor: &str,
) -> Result<(), ApiError> {
    let available: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM works WHERE id = $1 AND merged_into_work_id IS NULL)",
    )
    .bind(canonical)
    .fetch_one(&mut **tx)
    .await?;
    if !available {
        return Err(ApiError::BadRequest(
            "canonical work is already merged".into(),
        ));
    }

    sqlx::query(
        "DELETE FROM work_versions d USING work_versions c \
         WHERE d.work_id = $1 AND c.work_id = $2 AND d.version_label = c.version_label",
    )
    .bind(duplicate)
    .bind(canonical)
    .execute(&mut **tx)
    .await?;
    let canonical_has_version: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM work_versions WHERE work_id = $1 AND is_canonical)",
    )
    .bind(canonical)
    .fetch_one(&mut **tx)
    .await?;
    if canonical_has_version {
        sqlx::query("UPDATE work_versions SET is_canonical = false WHERE work_id = $1")
            .bind(duplicate)
            .execute(&mut **tx)
            .await?;
    }
    sqlx::query("UPDATE work_versions SET work_id = $2 WHERE work_id = $1")
        .bind(duplicate)
        .bind(canonical)
        .execute(&mut **tx)
        .await?;

    sqlx::query(
        "WITH missing AS ( \
            SELECT wa.person_id, row_number() OVER (ORDER BY wa.position, wa.person_id) AS offset \
            FROM work_authors wa WHERE wa.work_id = $1 \
            AND NOT EXISTS (SELECT 1 FROM work_authors c \
                            WHERE c.work_id = $2 AND c.person_id = wa.person_id) \
         ), base AS (SELECT COALESCE(max(position), -1) AS position FROM work_authors WHERE work_id = $2) \
         INSERT INTO work_authors (work_id, person_id, position) \
         SELECT $2, missing.person_id, base.position + missing.offset::integer \
         FROM missing CROSS JOIN base",
    )
    .bind(duplicate)
    .bind(canonical)
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM work_authors WHERE work_id = $1")
        .bind(duplicate)
        .execute(&mut **tx)
        .await?;

    let sources: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, url FROM sources WHERE work_id = $1")
            .bind(duplicate)
            .fetch_all(&mut **tx)
            .await?;
    for (source_id, url) in sources {
        let existing: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM sources WHERE work_id = $1 AND url = $2")
                .bind(canonical)
                .bind(&url)
                .fetch_optional(&mut **tx)
                .await?;
        if let Some(existing_id) = existing {
            sqlx::query("UPDATE documents SET source_id = $2 WHERE source_id = $1")
                .bind(source_id)
                .bind(existing_id)
                .execute(&mut **tx)
                .await?;
            sqlx::query(
                "UPDATE work_relations SET evidence_source_id = $2 WHERE evidence_source_id = $1",
            )
            .bind(source_id)
            .bind(existing_id)
            .execute(&mut **tx)
            .await?;
            sqlx::query("UPDATE research_annotations SET evidence_source_id = $2 WHERE evidence_source_id = $1")
                .bind(source_id)
                .bind(existing_id)
                .execute(&mut **tx)
                .await?;
            sqlx::query("DELETE FROM sources WHERE id = $1")
                .bind(source_id)
                .execute(&mut **tx)
                .await?;
        } else {
            sqlx::query("UPDATE sources SET work_id = $2 WHERE id = $1")
                .bind(source_id)
                .bind(canonical)
                .execute(&mut **tx)
                .await?;
        }
    }

    sqlx::query("UPDATE documents SET work_id = $2 WHERE work_id = $1")
        .bind(duplicate)
        .bind(canonical)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE proof_results SET work_id = $2 WHERE work_id = $1")
        .bind(duplicate)
        .bind(canonical)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE research_annotations SET work_id = $2 WHERE work_id = $1")
        .bind(duplicate)
        .bind(canonical)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE discovery_candidates SET work_id = $2 WHERE work_id = $1")
        .bind(duplicate)
        .bind(canonical)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE citation_candidates SET citing_work_id = $2 WHERE citing_work_id = $1")
        .bind(duplicate)
        .bind(canonical)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "DELETE FROM entity_resolution_suggestions d USING entity_resolution_suggestions c \
         WHERE d.work_id = $1 AND c.work_id = $2 \
         AND d.discovery_candidate_id = c.discovery_candidate_id AND d.method = c.method",
    )
    .bind(duplicate)
    .bind(canonical)
    .execute(&mut **tx)
    .await?;
    sqlx::query("UPDATE entity_resolution_suggestions SET work_id = $2 WHERE work_id = $1")
        .bind(duplicate)
        .bind(canonical)
        .execute(&mut **tx)
        .await?;

    sqlx::query(
        "DELETE FROM formal_artifacts d USING formal_artifacts c \
         WHERE d.work_id = $1 AND c.work_id = $2 AND d.url = c.url",
    )
    .bind(duplicate)
    .bind(canonical)
    .execute(&mut **tx)
    .await?;
    sqlx::query("UPDATE formal_artifacts SET work_id = $2 WHERE work_id = $1")
        .bind(duplicate)
        .bind(canonical)
        .execute(&mut **tx)
        .await?;

    sqlx::query(
        "INSERT INTO citations \
            (citing_work_id, cited_work_id, source_document_id, status, evidence_span_id, \
             created_by, review_reason) \
         SELECT CASE WHEN citing_work_id = $1 THEN $2 ELSE citing_work_id END, \
                CASE WHEN cited_work_id = $1 THEN $2 ELSE cited_work_id END, \
                source_document_id, status, evidence_span_id, created_by, review_reason \
         FROM citations WHERE (citing_work_id = $1 OR cited_work_id = $1) \
         AND CASE WHEN citing_work_id = $1 THEN $2 ELSE citing_work_id END <> \
             CASE WHEN cited_work_id = $1 THEN $2 ELSE cited_work_id END \
         ON CONFLICT (citing_work_id, cited_work_id) DO NOTHING",
    )
    .bind(duplicate)
    .bind(canonical)
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM citations WHERE citing_work_id = $1 OR cited_work_id = $1")
        .bind(duplicate)
        .execute(&mut **tx)
        .await?;

    sqlx::query(
        "INSERT INTO work_relations \
            (from_work_id, to_work_id, relation_kind, evidence_source_id, notes, status, created_by) \
         SELECT CASE WHEN from_work_id = $1 THEN $2 ELSE from_work_id END, \
                CASE WHEN to_work_id = $1 THEN $2 ELSE to_work_id END, \
                relation_kind, evidence_source_id, notes, status, created_by \
         FROM work_relations WHERE (from_work_id = $1 OR to_work_id = $1) \
         AND CASE WHEN from_work_id = $1 THEN $2 ELSE from_work_id END <> \
             CASE WHEN to_work_id = $1 THEN $2 ELSE to_work_id END \
         ON CONFLICT (from_work_id, to_work_id, relation_kind) DO NOTHING",
    )
    .bind(duplicate)
    .bind(canonical)
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM work_relations WHERE from_work_id = $1 OR to_work_id = $1")
        .bind(duplicate)
        .execute(&mut **tx)
        .await?;

    sqlx::query(
        "UPDATE works SET merged_into_work_id = $2, merged_at = now(), merged_by = $3 \
         WHERE id = $1 AND merged_into_work_id IS NULL",
    )
    .bind(duplicate)
    .bind(canonical)
    .bind(actor)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn json_rows(state: &AppState, query: &str, id: Uuid) -> Result<Value, ApiError> {
    let rows: Vec<Value> = sqlx::query_scalar(query)
        .bind(id)
        .fetch_all(&state.db)
        .await?;
    Ok(Value::Array(rows))
}

async fn audit(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: &str,
    action: &str,
    entity_type: &str,
    entity_id: Uuid,
    after_value: Value,
    reason: &str,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO audit_log \
         (actor, action, entity_type, entity_id, after_value, reason) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(actor.trim())
    .bind(action)
    .bind(entity_type)
    .bind(entity_id)
    .bind(after_value)
    .bind(reason.trim())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn require_actor_reason(_actor: &str, reason: &str) -> Result<(), ApiError> {
    if reason.trim().is_empty() {
        return Err(ApiError::BadRequest("reason is required".into()));
    }
    Ok(())
}

fn validate_optional_sha256(value: Option<&str>) -> Result<(), ApiError> {
    if value.is_some_and(|digest| {
        digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err(ApiError::BadRequest("invalid sha256".into()));
    }
    Ok(())
}
