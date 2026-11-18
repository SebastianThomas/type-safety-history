use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::FromRow;
use uuid::Uuid;

use crate::{AppState, routes::ApiError};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/auth/me", get(me))
        .route("/api/v1/admin/users", get(list_users).post(create_user))
        .route("/api/v1/admin/users/{id}/tokens", post(create_token))
        .route("/api/v1/admin/users/{id}/status", post(set_user_status))
        .route("/api/v1/admin/tokens/{id}/revoke", post(revoke_token))
}

#[derive(Clone, Debug, Serialize, FromRow)]
pub(crate) struct Principal {
    user_id: Option<Uuid>,
    username: String,
    display_name: String,
    role: String,
    bootstrap: bool,
}

impl Principal {
    pub(crate) fn username(&self) -> &str {
        &self.username
    }
}

pub(crate) async fn require_curator(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Principal, ApiError> {
    let principal = authenticate(state, headers).await?;
    if matches!(principal.role.as_str(), "curator" | "admin") {
        Ok(principal)
    } else {
        Err(ApiError::Forbidden)
    }
}

async fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<Principal, ApiError> {
    let principal = authenticate(state, headers).await?;
    if principal.role == "admin" {
        Ok(principal)
    } else {
        Err(ApiError::Forbidden)
    }
}

async fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Principal, ApiError> {
    let supplied = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty())
        .ok_or(ApiError::Unauthorized)?;
    if state
        .curator_token
        .as_deref()
        .is_some_and(|configured| supplied.as_bytes() == configured.as_bytes())
    {
        return Ok(Principal {
            user_id: None,
            username: "bootstrap".into(),
            display_name: "Bootstrap administrator".into(),
            role: "admin".into(),
            bootstrap: true,
        });
    }
    let digest = format!("{:x}", Sha256::digest(supplied.as_bytes()));
    let principal = sqlx::query_as::<_, Principal>(
        "SELECT u.id AS user_id, u.username, u.display_name, u.role::text AS role, \
         false AS bootstrap FROM curator_api_tokens token \
         JOIN curator_users u ON u.id = token.user_id \
         WHERE token.token_sha256 = $1 AND token.revoked_at IS NULL AND u.active \
         AND (token.expires_at IS NULL OR token.expires_at > now())",
    )
    .bind(&digest)
    .fetch_optional(&state.db)
    .await?
    .ok_or(ApiError::Unauthorized)?;
    sqlx::query("UPDATE curator_api_tokens SET last_used_at = now() WHERE token_sha256 = $1")
        .bind(digest)
        .execute(&state.db)
        .await?;
    Ok(principal)
}

async fn me(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Principal>, ApiError> {
    Ok(Json(authenticate(&state, &headers).await?))
}

#[derive(Serialize, FromRow)]
struct UserRecord {
    id: Uuid,
    username: String,
    display_name: String,
    role: String,
    active: bool,
    created_at: String,
}

async fn list_users(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<UserRecord>>, ApiError> {
    require_admin(&state, &headers).await?;
    let users = sqlx::query_as::<_, UserRecord>(
        "SELECT id, username, display_name, role::text AS role, active, \
         created_at::text AS created_at FROM curator_users ORDER BY username",
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(users))
}

#[derive(Deserialize)]
struct CreateUser {
    username: String,
    display_name: String,
    #[serde(default = "default_role")]
    role: String,
}

fn default_role() -> String {
    "curator".into()
}

async fn create_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateUser>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let administrator = require_admin(&state, &headers).await?;
    if !matches!(input.role.as_str(), "viewer" | "curator" | "admin") {
        return Err(ApiError::BadRequest("invalid role".into()));
    }
    let username = input.username.trim().to_ascii_lowercase();
    if input.display_name.trim().is_empty() {
        return Err(ApiError::BadRequest("display_name is required".into()));
    }
    let mut tx = state.db.begin().await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO curator_users (username, display_name, role) \
         VALUES ($1, $2, $3::curator_role) RETURNING id",
    )
    .bind(&username)
    .bind(input.display_name.trim())
    .bind(&input.role)
    .fetch_one(&mut *tx)
    .await?;
    audit_admin(
        &mut tx,
        &administrator,
        "create_user",
        "curator_user",
        id,
        json!({"username": username, "role": input.role}),
        "Administrator created curator account",
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))))
}

#[derive(Deserialize)]
struct CreateToken {
    label: String,
    expires_at: Option<String>,
}

async fn create_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(user_id): Path<Uuid>,
    Json(input): Json<CreateToken>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let administrator = require_admin(&state, &headers).await?;
    if input.label.trim().is_empty() {
        return Err(ApiError::BadRequest("label is required".into()));
    }
    let mut random = [0_u8; 32];
    rand::rng().fill_bytes(&mut random);
    let secret = format!(
        "tsh_{}",
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let digest = format!("{:x}", Sha256::digest(secret.as_bytes()));
    let prefix = secret.chars().take(12).collect::<String>();
    let mut tx = state.db.begin().await?;
    let token_id: Uuid = sqlx::query_scalar(
        "INSERT INTO curator_api_tokens \
         (user_id, label, token_prefix, token_sha256, expires_at) \
         VALUES ($1, $2, $3, $4, $5::timestamptz) RETURNING id",
    )
    .bind(user_id)
    .bind(input.label.trim())
    .bind(&prefix)
    .bind(digest)
    .bind(input.expires_at)
    .fetch_one(&mut *tx)
    .await?;
    audit_admin(
        &mut tx,
        &administrator,
        "create_token",
        "curator_api_token",
        token_id,
        json!({"user_id": user_id, "label": input.label, "token_prefix": prefix}),
        "Administrator issued API token",
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": token_id,
            "token": secret,
            "token_prefix": prefix,
            "warning": "This token is shown once and is stored only as a SHA-256 digest."
        })),
    ))
}

#[derive(Deserialize)]
struct SetUserStatus {
    active: bool,
    reason: String,
}

async fn set_user_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(user_id): Path<Uuid>,
    Json(input): Json<SetUserStatus>,
) -> Result<StatusCode, ApiError> {
    let administrator = require_admin(&state, &headers).await?;
    if input.reason.trim().is_empty() {
        return Err(ApiError::BadRequest("reason is required".into()));
    }
    let mut tx = state.db.begin().await?;
    let updated =
        sqlx::query("UPDATE curator_users SET active = $2, updated_at = now() WHERE id = $1")
            .bind(user_id)
            .bind(input.active)
            .execute(&mut *tx)
            .await?;
    if updated.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    audit_admin(
        &mut tx,
        &administrator,
        "set_status",
        "curator_user",
        user_id,
        json!({"active": input.active}),
        input.reason.trim(),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct RevokeToken {
    reason: String,
}

async fn revoke_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token_id): Path<Uuid>,
    Json(input): Json<RevokeToken>,
) -> Result<StatusCode, ApiError> {
    let administrator = require_admin(&state, &headers).await?;
    if input.reason.trim().is_empty() {
        return Err(ApiError::BadRequest("reason is required".into()));
    }
    let mut tx = state.db.begin().await?;
    let updated = sqlx::query(
        "UPDATE curator_api_tokens SET revoked_at = now() \
         WHERE id = $1 AND revoked_at IS NULL",
    )
    .bind(token_id)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    audit_admin(
        &mut tx,
        &administrator,
        "revoke",
        "curator_api_token",
        token_id,
        json!({"revoked": true}),
        input.reason.trim(),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn audit_admin(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    administrator: &Principal,
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
    .bind(&administrator.username)
    .bind(action)
    .bind(entity_type)
    .bind(entity_id)
    .bind(after_value)
    .bind(reason)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
