use std::{
    env,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, postgres::PgPoolOptions};
use tokio::io::AsyncReadExt;
use uuid::Uuid;

#[derive(FromRow)]
struct DocumentObject {
    id: Uuid,
    object_key: String,
    sha256: String,
    status: String,
}

#[derive(FromRow)]
struct DerivedObject {
    owner_id: Uuid,
    object_key: String,
    sha256: Option<String>,
    kind: String,
}

pub(crate) async fn run() -> Result<()> {
    let database_url = env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
    let data_root = PathBuf::from(env::var("DATA_ROOT").unwrap_or_else(|_| "/data".into()));
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .context("connect to PostgreSQL")?;

    let documents = sqlx::query_as::<_, DocumentObject>(
        "SELECT id, object_key, sha256, status::text AS status FROM documents ORDER BY id",
    )
    .fetch_all(&pool)
    .await?;
    let derived = sqlx::query_as::<_, DerivedObject>(
        "SELECT document_id AS owner_id, zarr_object_key AS object_key, \
                NULL::text AS sha256, 'zarr'::text AS kind FROM document_embeddings \
         UNION ALL \
         SELECT id AS owner_id, object_key, sha256, 'parquet'::text AS kind \
         FROM exports WHERE status = 'succeeded'",
    )
    .fetch_all(&pool)
    .await?;

    let mut errors = Vec::new();
    let mut checked = 0_usize;
    for document in documents {
        let path = safe_data_path(&data_root, &document.object_key)?;
        if document.status == "taken_down" {
            if tokio::fs::try_exists(&path).await? {
                errors.push(format!(
                    "taken-down document {} still has object {}",
                    document.id, document.object_key
                ));
            }
            continue;
        }
        match sha256_file(&path).await {
            Ok(digest) if digest == document.sha256 => checked += 1,
            Ok(digest) => errors.push(format!(
                "document {} checksum mismatch: expected {}, found {}",
                document.id, document.sha256, digest
            )),
            Err(error) => errors.push(format!("document {} cannot be read: {error}", document.id)),
        }
    }
    for object in derived {
        let path = safe_data_path(&data_root, &object.object_key)?;
        if object.kind == "zarr" {
            if !tokio::fs::metadata(&path)
                .await
                .is_ok_and(|metadata| metadata.is_dir())
            {
                errors.push(format!(
                    "document {} is missing Zarr directory {}",
                    object.owner_id, object.object_key
                ));
            } else {
                checked += 1;
            }
        } else {
            match sha256_file(&path).await {
                Ok(digest) if object.sha256.as_deref() == Some(&digest) => checked += 1,
                Ok(digest) => errors.push(format!(
                    "export {} checksum mismatch: expected {}, found {}",
                    object.owner_id,
                    object.sha256.as_deref().unwrap_or("missing"),
                    digest
                )),
                Err(error) => errors.push(format!(
                    "export {} cannot be read: {error}",
                    object.owner_id
                )),
            }
        }
    }

    if !errors.is_empty() {
        for error in &errors {
            tracing::error!(%error, "storage integrity failure");
        }
        anyhow::bail!(
            "storage integrity check failed: {} error(s), {checked} object(s) verified",
            errors.len()
        );
    }
    tracing::info!(objects = checked, "storage integrity check passed");
    Ok(())
}

fn safe_data_path(root: &Path, key: &str) -> Result<PathBuf> {
    let relative = Path::new(key);
    anyhow::ensure!(
        !relative.is_absolute()
            && !key.is_empty()
            && relative
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "invalid stored object key: {key}"
    );
    Ok(root.join(relative))
}

async fn sha256_file(path: &Path) -> Result<String> {
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("open {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(hex::encode(digest.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_paths_are_confined() {
        assert!(safe_data_path(Path::new("/data"), "objects/a").is_ok());
        assert!(safe_data_path(Path::new("/data"), "../outside").is_err());
        assert!(safe_data_path(Path::new("/data"), "/outside").is_err());
    }
}
