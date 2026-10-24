use faktor_site::config::Config;
use faktor_site::{app, AppState};
use std::error::Error;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .init();
    let command = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "serve".to_string());
    match command.as_str() {
        "serve" => serve().await?,
        "check" => check().await?,
        other => {
            eprintln!("unknown command {other:?}; use `serve` or `check`");
            std::process::exit(2);
        }
    }
    Ok(())
}

async fn serve() -> Result<(), Box<dyn Error>> {
    let config = Config::from_env()?;
    let bind = config.bind.clone();
    if !config.static_dir.join("index.html").exists() {
        tracing::warn!(
            static_dir = %config.static_dir.display(),
            "the Zola build output is missing; run `zola build` before serving"
        );
    }
    let state = AppState::build(config)?;
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "faktor site listening");
    axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await?;
    Ok(())
}

async fn check() -> Result<(), Box<dyn Error>> {
    let config = Config::from_env()?;
    config.ensure_directories()?;
    let state = AppState::build(config)?;
    let _guard = state.db.conn()?;
    println!("faktor-site check: configuration valid, SQLite schema ready");
    Ok(())
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
