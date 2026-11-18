use std::{env, path::Path};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;
use sqlx::postgres::PgPoolOptions;

#[derive(Deserialize)]
struct SeedWork {
    doi: String,
    title: String,
    publication_year: i32,
    #[serde(default = "default_work_type")]
    work_type: String,
    role: String,
    curation_note: String,
}

fn default_work_type() -> String {
    "article".into()
}

pub(crate) async fn run(path: &Path) -> Result<()> {
    let contents = tokio::fs::read(path)
        .await
        .with_context(|| format!("read seed file {}", path.display()))?;
    let seeds: Vec<SeedWork> = serde_json::from_slice(&contents).context("parse seed JSON")?;
    let database_url = env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .context("connect to PostgreSQL")?;
    sqlx::migrate!("../../migrations")
        .run(&pool)
        .await
        .context("run database migrations")?;

    let mut tx = pool.begin().await?;
    let mut imported = 0_u64;
    for seed in seeds {
        if seed.publication_year < 1800 || seed.publication_year > 2200 {
            anyhow::bail!("invalid publication year for {}", seed.doi);
        }
        let doi = normalize_doi(&seed.doi);
        if !doi.starts_with("10.") || !doi.contains('/') {
            anyhow::bail!("invalid DOI in seed: {}", seed.doi);
        }
        let candidate_id: uuid::Uuid = sqlx::query_scalar(
            r#"INSERT INTO discovery_candidates
               (provider, external_id, title, publication_year, work_type, raw_metadata)
               VALUES ('crossref', $1, $2, $3, $4::work_type, $5)
               ON CONFLICT (provider, external_id) DO UPDATE SET
                  title = EXCLUDED.title, publication_year = EXCLUDED.publication_year,
                  work_type = EXCLUDED.work_type, raw_metadata = EXCLUDED.raw_metadata
               RETURNING id"#,
        )
        .bind(&doi)
        .bind(seed.title.trim())
        .bind(seed.publication_year)
        .bind(&seed.work_type)
        .bind(json!({
            "seed_role": &seed.role,
            "curation_note": seed.curation_note,
            "source": path.display().to_string()
        }))
        .fetch_one(&mut *tx)
        .await
        .with_context(|| format!("upsert seed candidate {doi}"))?;
        sqlx::query(
            "INSERT INTO discovery_hits (candidate_id, query_text, from_year, until_year) \
             VALUES ($1, $2, $3, $3) ON CONFLICT DO NOTHING",
        )
        .bind(candidate_id)
        .bind(format!("curated-seed:{}", seed.role))
        .bind(seed.publication_year)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("insert seed discovery hit {doi}"))?;
        imported += 1;
    }
    sqlx::query(
        "INSERT INTO audit_log (actor, action, entity_type, after_value, reason) \
         VALUES ('seed-loader', 'load', 'discovery_candidate', $1, \
                 'Repeatable local curated seed import')",
    )
    .bind(json!({"records": imported, "file": path.display().to_string()}))
    .execute(&mut *tx)
    .await
    .context("audit seed load")?;
    tx.commit().await?;
    tracing::info!(records = imported, file = %path.display(), "seed candidates loaded");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_seed_doi() {
        assert_eq!(normalize_doi("https://doi.org/10.1/ABC"), "10.1/abc");
    }
}
