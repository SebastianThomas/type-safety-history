mod auth;
mod crossref;
mod integrity;
mod lifecycle;
mod ml;
mod openalex;
mod portal;
mod research;
mod routes;
mod seed;
mod stats;

use std::{
    env,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use axum::{Router, extract::DefaultBodyLimit};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tokio::signal;
use tower_http::{catch_panic::CatchPanicLayer, trace::TraceLayer};
use tracing::info;

#[derive(Clone)]
pub(crate) struct AppState {
    pub db: PgPool,
    pub data_root: Arc<PathBuf>,
    pub curator_token: Option<Arc<str>>,
    pub crossref: Option<crossref::CrossrefClient>,
    pub openalex: Option<openalex::OpenAlexClient>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut arguments = env::args().skip(1);
    match arguments.next().as_deref() {
        Some("healthcheck") => return healthcheck().await,
        Some("seed") => {
            init_tracing();
            let path = arguments
                .next()
                .context("usage: api seed <seed-file.json>")?;
            anyhow::ensure!(
                arguments.next().is_none(),
                "usage: api seed <seed-file.json>"
            );
            return seed::run(Path::new(&path)).await;
        }
        Some("verify-storage") => {
            init_tracing();
            anyhow::ensure!(arguments.next().is_none(), "usage: api verify-storage");
            return integrity::run().await;
        }
        Some("migrate") => {
            init_tracing();
            anyhow::ensure!(arguments.next().is_none(), "usage: api migrate");
            return migrate().await;
        }
        Some(command) => anyhow::bail!("unknown command: {command}"),
        None => {}
    }
    init_tracing();

    let database_url = env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
    let data_root = PathBuf::from(env::var("DATA_ROOT").unwrap_or_else(|_| "/data".into()));
    create_shared_directory(&data_root.join("objects")).await?;
    create_shared_directory(&data_root.join("derived")).await?;
    create_shared_directory(&data_root.join("tmp")).await?;
    create_shared_directory(&data_root.join("zarr")).await?;
    create_shared_directory(&data_root.join("parquet")).await?;
    create_shared_directory(&data_root.join("models")).await?;

    let db = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&database_url)
        .await
        .context("connect to PostgreSQL")?;

    sqlx::migrate!("../../migrations")
        .run(&db)
        .await
        .context("run database migrations")?;

    let curator_token = env::var("CURATOR_TOKEN")
        .ok()
        .filter(|token| !token.trim().is_empty())
        .map(Arc::<str>::from);
    let crossref = match env::var("CROSSREF_MAILTO") {
        Ok(mailto) if !mailto.trim().is_empty() => Some(crossref::CrossrefClient::new(
            mailto,
            env::var("CROSSREF_USER_AGENT").unwrap_or_else(|_| {
                "type-safety-history/0.1 (+https://type-safety-history.sthomas.ch)".into()
            }),
        )?),
        _ => None,
    };
    let openalex = match env::var("OPENALEX_API_KEY") {
        Ok(api_key) if !api_key.trim().is_empty() => Some(openalex::OpenAlexClient::new(
            api_key,
            env::var("OPENALEX_USER_AGENT").unwrap_or_else(|_| {
                "type-safety-history/0.1 (+https://type-safety-history.sthomas.ch)".into()
            }),
        )?),
        _ => None,
    };
    let state = AppState {
        db,
        data_root: Arc::new(data_root),
        curator_token,
        crossref,
        openalex,
    };
    let discovery_scheduler = tokio::spawn(routes::run_discovery_scheduler(state.clone()));
    let app = Router::new()
        .merge(routes::router())
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .layer(CatchPanicLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let port = env::var("PORT")
        .unwrap_or_else(|_| "8080".into())
        .parse::<u16>()
        .context("PORT must be a valid TCP port")?;
    let address = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(address).await?;
    info!(%address, "API listening");
    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    discovery_scheduler.abort();
    serve_result?;
    Ok(())
}

async fn create_shared_directory(path: &Path) -> Result<()> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) => anyhow::ensure!(metadata.is_dir(), "{} is not a directory", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tokio::fs::create_dir_all(path).await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o2770)).await?;
            }
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

async fn migrate() -> Result<()> {
    let database_url = env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
    let db = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&database_url)
        .await
        .context("connect to PostgreSQL")?;
    sqlx::migrate!("../../migrations")
        .run(&db)
        .await
        .context("run database migrations")?;
    Ok(())
}

async fn healthcheck() -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let port = env::var("PORT").unwrap_or_else(|_| "8080".into());
    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .context("connect to local API")?;
    stream
        .write_all(b"GET /health/ready HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await?;
    let mut response = Vec::with_capacity(512);
    stream.take(4096).read_to_end(&mut response).await?;
    let status_line = response
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    anyhow::ensure!(status_line.starts_with(b"HTTP/1.1 200"), "API is not ready");
    Ok(())
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "type_safety_history_api=info,tower_http=info".into());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .json()
        .init();
}

async fn shutdown_signal() {
    let ctrl_c = async { signal::ctrl_c().await.expect("install Ctrl-C handler") };
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
}
