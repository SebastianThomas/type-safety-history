use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::HeaderMap,
    routing::get,
};
use reqwest::{Client, StatusCode, header, redirect::Policy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool};
use tokio::{io::AsyncWriteExt, net::lookup_host};
use tracing::{info, warn};
use url::Url;
use uuid::Uuid;

use crate::{AppState, auth::require_viewer, routes::ApiError};

const MAX_DOCUMENT_BYTES: u64 = 50 * 1024 * 1024;
const LEASE_SECONDS: i32 = 300;
const MAX_ATTEMPTS: i32 = 5;

pub(crate) fn router() -> Router<AppState> {
    Router::new().route("/api/v1/source-resolution", get(list_source_resolution))
}

#[derive(Deserialize)]
struct ResolutionQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
    status: Option<String>,
}

fn default_limit() -> i64 {
    50
}

#[derive(Serialize, FromRow)]
struct ResolutionItem {
    work_id: Uuid,
    title: String,
    doi: Option<String>,
    status: String,
    priority: i16,
    attempts: i32,
    result: Option<Value>,
    last_error: Option<String>,
    updated_at: String,
}

async fn list_source_resolution(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ResolutionQuery>,
) -> std::result::Result<Json<Vec<ResolutionItem>>, ApiError> {
    require_viewer(&state, &headers).await?;
    let allowed = [
        "queued",
        "running",
        "resolved",
        "no_doi",
        "no_open_source",
        "rights_review",
        "failed",
    ];
    if query
        .status
        .as_deref()
        .is_some_and(|status| !allowed.contains(&status))
    {
        return Err(ApiError::BadRequest(
            "invalid source-resolution status".into(),
        ));
    }
    let items = sqlx::query_as::<_, ResolutionItem>(
        "SELECT queue.work_id, work.title, work.doi, queue.status, queue.priority, \
         queue.attempts, queue.result, queue.last_error, queue.updated_at::text AS updated_at \
         FROM source_resolution_queue queue JOIN works work ON work.id = queue.work_id \
         WHERE ($1::text IS NULL OR queue.status = $1) \
         ORDER BY CASE queue.status WHEN 'rights_review' THEN 0 WHEN 'failed' THEN 1 \
                  WHEN 'queued' THEN 2 WHEN 'running' THEN 3 ELSE 4 END, \
                  queue.priority DESC, queue.updated_at DESC \
         LIMIT $2 OFFSET $3",
    )
    .bind(query.status)
    .bind(query.limit.clamp(1, 200))
    .bind(query.offset.max(0))
    .fetch_all(&state.db)
    .await?;
    Ok(Json(items))
}

#[derive(Clone)]
pub(crate) struct UnpaywallClient {
    client: Client,
    email: Arc<str>,
    user_agent: Arc<str>,
    limiter: Arc<tokio::sync::Mutex<tokio::time::Instant>>,
}

#[derive(Debug, Deserialize)]
struct UnpaywallRecord {
    best_oa_location: Option<OpenAccessLocation>,
    #[serde(default)]
    oa_locations: Vec<OpenAccessLocation>,
}

#[derive(Debug, Deserialize, Clone)]
struct OpenAccessLocation {
    url_for_pdf: Option<String>,
    url_for_landing_page: Option<String>,
    license: Option<String>,
    host_type: Option<String>,
    version: Option<String>,
}

#[derive(Debug, FromRow)]
struct ResolutionWork {
    work_id: Uuid,
    doi: Option<String>,
    attempts: i32,
}

struct DownloadedPdf {
    temporary_path: PathBuf,
    sha256: String,
    byte_size: u64,
    final_url: Url,
}

impl UnpaywallClient {
    pub(crate) fn new(email: String, user_agent: String) -> Result<Self> {
        anyhow::ensure!(
            email.contains('@'),
            "Unpaywall contact must be an email address"
        );
        let client = Client::builder()
            .user_agent(&user_agent)
            .timeout(Duration::from_secs(30))
            .redirect(Policy::none())
            .build()
            .context("build source-resolution HTTP client")?;
        Ok(Self {
            client,
            email: Arc::from(email),
            user_agent: Arc::from(user_agent),
            limiter: Arc::new(tokio::sync::Mutex::new(tokio::time::Instant::now())),
        })
    }

    async fn lookup(&self, doi: &str) -> Result<Option<OpenAccessLocation>> {
        let mut last_request = self.limiter.lock().await;
        tokio::time::sleep_until(*last_request + Duration::from_millis(200)).await;
        let mut url = Url::parse("https://api.unpaywall.org/v2/")?;
        url.path_segments_mut()
            .map_err(|_| anyhow!("invalid Unpaywall base URL"))?
            .push(doi);
        url.query_pairs_mut().append_pair("email", &self.email);
        *last_request = tokio::time::Instant::now();
        let response = self.client.get(url).send().await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            bail!("Unpaywall returned HTTP {}", response.status());
        }
        let record: UnpaywallRecord = response.json().await?;
        let permitted_pdf = record
            .best_oa_location
            .iter()
            .chain(record.oa_locations.iter())
            .find(|location| {
                location.url_for_pdf.is_some()
                    && location
                        .license
                        .as_deref()
                        .is_some_and(license_is_automatically_permitted)
            })
            .cloned();
        Ok(permitted_pdf
            .or(record.best_oa_location)
            .or_else(|| record.oa_locations.into_iter().next()))
    }

    async fn download_pdf(
        &self,
        source_url: &str,
        temporary_path: PathBuf,
    ) -> Result<DownloadedPdf> {
        let mut url = Url::parse(source_url).context("invalid PDF URL")?;
        for _ in 0..=5 {
            let (host, address) = validated_public_address(&url).await?;
            let pinned_client = Client::builder()
                .user_agent(self.user_agent.as_ref())
                .timeout(Duration::from_secs(30))
                .redirect(Policy::none())
                .resolve(&host, address)
                .build()?;
            let mut response = pinned_client.get(url.clone()).send().await?;
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .context("redirect omitted Location")?
                    .to_str()?;
                url = url.join(location)?;
                continue;
            }
            if !response.status().is_success() {
                bail!("PDF host returned HTTP {}", response.status());
            }
            if let Some(length) = response.content_length()
                && length > MAX_DOCUMENT_BYTES
            {
                bail!("PDF exceeds the 50 MiB limit");
            }
            let content_type = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if !content_type.starts_with("application/pdf") {
                bail!("source did not return application/pdf");
            }

            let mut file = tokio::fs::File::create(&temporary_path).await?;
            let mut hasher = Sha256::new();
            let mut byte_size = 0_u64;
            let mut magic = Vec::with_capacity(5);
            while let Some(chunk) = response.chunk().await? {
                byte_size += chunk.len() as u64;
                if byte_size > MAX_DOCUMENT_BYTES {
                    drop(file);
                    let _ = tokio::fs::remove_file(&temporary_path).await;
                    bail!("PDF exceeds the 50 MiB limit");
                }
                if magic.len() < 5 {
                    magic.extend_from_slice(&chunk[..chunk.len().min(5 - magic.len())]);
                }
                hasher.update(&chunk);
                file.write_all(&chunk).await?;
            }
            file.flush().await?;
            if magic != b"%PDF-" {
                drop(file);
                let _ = tokio::fs::remove_file(&temporary_path).await;
                bail!("downloaded content is not a PDF");
            }
            return Ok(DownloadedPdf {
                temporary_path,
                sha256: hex::encode(hasher.finalize()),
                byte_size,
                final_url: url,
            });
        }
        bail!("too many PDF redirects")
    }
}

pub(crate) async fn run_source_resolver(state: AppState) {
    let Some(client) = state.unpaywall.clone() else {
        warn!("automatic source resolution disabled: CROSSREF_MAILTO is not configured");
        return;
    };
    if let Err(error) = cleanup_stale_temporary_files(state.data_root.as_ref()).await {
        warn!(%error, "could not clean stale source-resolution temporary files");
    }
    info!("automatic licence-gated source resolver started");
    loop {
        match claim_work(&state.db).await {
            Ok(Some(work)) => {
                if let Err(error) = resolve_work(&state, &client, &work).await {
                    warn!(work_id = %work.work_id, %error, "source resolution failed");
                    if let Err(persist_error) = fail_work(&state.db, &work, &error).await {
                        warn!(work_id = %work.work_id, %persist_error, "could not persist source-resolution failure");
                    }
                }
            }
            Ok(None) => tokio::time::sleep(Duration::from_secs(5)).await,
            Err(error) => {
                warn!(%error, "could not claim source-resolution work");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn cleanup_stale_temporary_files(data_root: &std::path::Path) -> Result<()> {
    let mut entries = match tokio::fs::read_dir(data_root.join("tmp")).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("source-resolution-") {
            continue;
        }
        let metadata = entry.metadata().await?;
        let old_enough = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs((LEASE_SECONDS as u64) * 2));
        if old_enough && metadata.is_file() {
            tokio::fs::remove_file(entry.path()).await?;
        }
    }
    Ok(())
}

async fn claim_work(db: &PgPool) -> Result<Option<ResolutionWork>> {
    Ok(sqlx::query_as::<_, ResolutionWork>(
        "WITH candidate AS ( \
             SELECT queue.work_id FROM source_resolution_queue queue \
             WHERE queue.attempts < $1 AND ( \
                 (queue.status IN ( \
                     'queued', 'failed', 'no_doi', 'no_open_source', 'rights_review' \
                  ) AND queue.available_at <= now()) OR \
                 (queue.status = 'running' AND queue.lease_until < now()) \
             ) ORDER BY queue.priority DESC, queue.created_at \
             FOR UPDATE SKIP LOCKED LIMIT 1 \
         ) \
         UPDATE source_resolution_queue queue \
         SET status = 'running', attempts = attempts + 1, \
             lease_until = now() + make_interval(secs => $2), updated_at = now() \
         FROM candidate JOIN works work ON work.id = candidate.work_id \
         WHERE queue.work_id = candidate.work_id \
         RETURNING queue.work_id, work.doi, queue.attempts",
    )
    .bind(MAX_ATTEMPTS)
    .bind(LEASE_SECONDS)
    .fetch_optional(db)
    .await?)
}

async fn resolve_work(
    state: &AppState,
    client: &UnpaywallClient,
    work: &ResolutionWork,
) -> Result<()> {
    if document_exists(&state.db, work.work_id).await? {
        return finish_queue(
            &state.db,
            work.work_id,
            "resolved",
            json!({"existing": true}),
        )
        .await;
    }
    let Some(doi) = work.doi.as_deref() else {
        return finish_queue(&state.db, work.work_id, "no_doi", json!({})).await;
    };
    let Some(location) = client.lookup(doi).await? else {
        return finish_queue(
            &state.db,
            work.work_id,
            "no_open_source",
            json!({"provider": "unpaywall", "doi": doi}),
        )
        .await;
    };
    let Some(license) = location.license.as_deref().map(str::to_ascii_lowercase) else {
        return rights_review(&state.db, work.work_id, doi, &location, "missing licence").await;
    };
    if !license_is_automatically_permitted(&license) {
        return rights_review(
            &state.db,
            work.work_id,
            doi,
            &location,
            "licence is not on the automatic allowlist",
        )
        .await;
    }
    let Some(pdf_url) = location.url_for_pdf.as_deref() else {
        return rights_review(&state.db, work.work_id, doi, &location, "no direct PDF URL").await;
    };

    let temporary_path = state
        .data_root
        .join("tmp")
        .join(format!("source-resolution-{}.tmp", Uuid::now_v7()));
    let downloaded = client.download_pdf(pdf_url, temporary_path).await?;
    persist_download(state, work, doi, &license, &location, downloaded).await
}

async fn persist_download(
    state: &AppState,
    work: &ResolutionWork,
    doi: &str,
    license: &str,
    location: &OpenAccessLocation,
    downloaded: DownloadedPdf,
) -> Result<()> {
    let object_key = format!(
        "objects/sha256/{}/{}/{}",
        &downloaded.sha256[0..2],
        &downloaded.sha256[2..4],
        downloaded.sha256
    );
    let destination = state.data_root.join(&object_key);
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let destination_existed = tokio::fs::try_exists(&destination).await?;
    if destination_existed {
        tokio::fs::remove_file(&downloaded.temporary_path).await?;
    } else {
        tokio::fs::rename(&downloaded.temporary_path, &destination).await?;
    }

    let persistence = async {
        let mut tx = state.db.begin().await?;
        let source_url = location
            .url_for_landing_page
            .as_deref()
            .unwrap_or(downloaded.final_url.as_str());
        let source_kind = if location.host_type.as_deref() == Some("publisher") {
            "publisher"
        } else {
            "institutional_repository"
        };
        let source_id: Uuid = sqlx::query_scalar(
            "INSERT INTO sources \
             (work_id, url, source_kind, reliability, rights_status, license_name, \
              license_url, terms_url, rights_basis, ml_use_permitted, \
              redistribution_permitted, raw_metadata) \
             VALUES ($1, $2, $3::source_kind, 'primary', 'open_redistributable', $4, \
                     $5, \
                     'https://unpaywall.org/legal/terms-of-service', $6, true, false, $7) \
             ON CONFLICT (url, work_id) DO UPDATE SET raw_metadata = EXCLUDED.raw_metadata, \
                 acquired_at = now() RETURNING id",
        )
        .bind(work.work_id)
        .bind(source_url)
        .bind(source_kind)
        .bind(license)
        .bind(license_reference_url(license))
        .bind(format!(
            "Unpaywall best OA location reports explicit licence {license}; automated policy excludes implied and unknown licences."
        ))
        .bind(json!({
            "provider": "unpaywall",
            "doi": doi,
            "pdf_url": downloaded.final_url.as_str(),
            "host_type": location.host_type,
            "version": location.version,
            "license": license
        }))
        .fetch_one(&mut *tx)
        .await?;
        let document_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO documents \
             (id, work_id, source_id, object_key, sha256, media_type, byte_size, \
              rights_status, ml_use_permitted, redistribution_permitted) \
             VALUES ($1, $2, $3, $4, $5, 'application/pdf', $6, \
                     'open_redistributable', true, false)",
        )
        .bind(document_id)
        .bind(work.work_id)
        .bind(source_id)
        .bind(&object_key)
        .bind(&downloaded.sha256)
        .bind(downloaded.byte_size as i64)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO jobs (job_type, idempotency_key, payload) \
             VALUES ('extract_document', $1, $2) ON CONFLICT (idempotency_key) DO NOTHING",
        )
        .bind(format!("extract:{document_id}:{}", downloaded.sha256))
        .bind(json!({"document_id": document_id, "object_key": object_key}))
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE source_resolution_queue SET status = 'resolved', provider = 'unpaywall', \
             result = $2, lease_until = NULL, last_error = NULL, updated_at = now() \
             WHERE work_id = $1",
        )
        .bind(work.work_id)
        .bind(json!({
            "document_id": document_id,
            "source_id": source_id,
            "sha256": downloaded.sha256,
            "license": license
        }))
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO audit_log \
             (actor, action, entity_type, entity_id, after_value, reason) \
             VALUES ('source-resolver:unpaywall', 'acquire_document', 'document', $1, $2, $3)",
        )
        .bind(document_id)
        .bind(json!({"work_id": work.work_id, "doi": doi, "license": license}))
        .bind("Explicitly licensed OA PDF acquired for local processing")
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Result::<()>::Ok(())
    }
    .await;
    if persistence.is_err() && !destination_existed {
        let _ = tokio::fs::remove_file(&destination).await;
    }
    persistence
}

async fn document_exists(db: &PgPool, work_id: Uuid) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM documents WHERE work_id = $1 AND status = 'active')",
    )
    .bind(work_id)
    .fetch_one(db)
    .await?)
}

async fn rights_review(
    db: &PgPool,
    work_id: Uuid,
    doi: &str,
    location: &OpenAccessLocation,
    reason: &str,
) -> Result<()> {
    finish_queue(
        db,
        work_id,
        "rights_review",
        json!({
            "provider": "unpaywall",
            "doi": doi,
            "reason": reason,
            "license": location.license,
            "landing_page_url": location.url_for_landing_page,
            "pdf_url": location.url_for_pdf,
            "host_type": location.host_type,
            "version": location.version
        }),
    )
    .await
}

async fn finish_queue(db: &PgPool, work_id: Uuid, status: &str, result: Value) -> Result<()> {
    sqlx::query(
        "UPDATE source_resolution_queue SET status = $2, provider = 'unpaywall', \
         result = $3, attempts = 0, \
         available_at = CASE WHEN $2 IN ('no_doi', 'no_open_source', 'rights_review') \
                             THEN now() + interval '30 days' ELSE available_at END, \
         lease_until = NULL, last_error = NULL, updated_at = now() \
         WHERE work_id = $1",
    )
    .bind(work_id)
    .bind(status)
    .bind(result)
    .execute(db)
    .await?;
    Ok(())
}

async fn fail_work(db: &PgPool, work: &ResolutionWork, error: &anyhow::Error) -> Result<()> {
    let delay = (work.attempts * work.attempts * 15).min(900);
    sqlx::query(
        "UPDATE source_resolution_queue SET status = 'failed', last_error = $2, \
         available_at = now() + make_interval(secs => $3), lease_until = NULL, \
         updated_at = now() WHERE work_id = $1",
    )
    .bind(work.work_id)
    .bind(error.to_string().chars().take(2000).collect::<String>())
    .bind(delay)
    .execute(db)
    .await?;
    Ok(())
}

fn license_is_automatically_permitted(license: &str) -> bool {
    let normalized = license.trim().to_ascii_lowercase();
    (normalized == "cc0" || normalized == "public-domain" || normalized.starts_with("cc-by"))
        && !normalized.contains("-nd")
}

fn license_reference_url(license: &str) -> &'static str {
    match license.trim().to_ascii_lowercase().as_str() {
        "cc0" => "https://creativecommons.org/publicdomain/zero/1.0/",
        "public-domain" => "https://creativecommons.org/publicdomain/mark/1.0/",
        _ => "https://creativecommons.org/licenses/",
    }
}

async fn validated_public_address(url: &Url) -> Result<(String, SocketAddr)> {
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        bail!("PDF URL must be credential-free HTTPS");
    }
    let host = url.host_str().context("PDF URL has no host")?;
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".local") {
        bail!("PDF URL host is not public");
    }
    let port = url.port_or_known_default().unwrap_or(443);
    let addresses = lookup_host((host, port)).await?.collect::<Vec<_>>();
    if addresses.is_empty() {
        bail!("PDF URL did not resolve");
    }
    for address in &addresses {
        if !is_public_ip(address.ip()) {
            bail!("PDF URL resolved to a non-public address");
        }
    }
    Ok((host.to_owned(), addresses[0]))
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_broadcast()
                || ip.is_documentation()
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19)))
        }
        IpAddr::V6(ip) => {
            let octets = ip.octets();
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || (octets[0] & 0xfe == 0xfc)
                || (octets[0] == 0xfe && octets[1] & 0xc0 == 0x80))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_license_policy_is_explicit_and_conservative() {
        assert!(license_is_automatically_permitted("cc-by"));
        assert!(license_is_automatically_permitted("CC-BY-NC-SA"));
        assert!(license_is_automatically_permitted("cc0"));
        assert!(!license_is_automatically_permitted("cc-by-nd"));
        assert!(!license_is_automatically_permitted("implied-oa"));
        assert!(!license_is_automatically_permitted("elsevier-specific"));
    }

    #[test]
    fn private_networks_are_rejected() {
        assert!(!is_public_ip("127.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("10.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("169.254.1.1".parse().unwrap()));
        assert!(!is_public_ip("::1".parse().unwrap()));
        assert!(is_public_ip("1.1.1.1".parse().unwrap()));
    }
}
