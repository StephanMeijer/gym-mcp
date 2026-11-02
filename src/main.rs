use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::{Router, middleware, routing::get};
use gym_mcp::{auth::{self, Auth}, db::App, tools::GymServer};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
};

fn env(name: &str) -> anyhow::Result<String> {
    std::env::var(name).with_context(|| format!("{name} is required"))
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

/// "3h", "90m", "45s" or plain seconds.
fn parse_duration(s: &str) -> anyhow::Result<Duration> {
    let (num, mult) = match s.chars().last() {
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('s') => (&s[..s.len() - 1], 1),
        _ => (s, 1),
    };
    Ok(Duration::from_secs(num.trim().parse::<u64>().with_context(|| format!("bad duration {s:?}"))? * mult))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(env_or("RUST_LOG", "info")).init();

    let public_url = env("PUBLIC_URL")?.trim_end_matches('/').to_owned();
    let pool = sqlx::postgres::PgPoolOptions::new().max_connections(10).connect(&env("DATABASE_URL")?).await?;
    sqlx::migrate!().run(&pool).await?;
    let app = App {
        pool,
        default_timezone: env_or("DEFAULT_TIMEZONE", "Europe/Amsterdam"),
        stale_after: parse_duration(&env_or("WORKOUT_STALE_AFTER", "3h"))?,
    };

    let auth = Arc::new(match std::env::var("DEV_SUB") {
        Ok(sub) => {
            tracing::warn!("DEV_SUB is set: authentication is DISABLED, every request acts as {sub:?}");
            Auth::dev(sub, public_url.clone())
        }
        Err(_) => Auth::discover(env("OIDC_ISSUER")?, public_url.clone()).await?,
    });

    let host = public_url.split("://").nth(1).unwrap_or(&public_url).split('/').next().unwrap_or_default().to_owned();
    let mcp = StreamableHttpService::new(
        move || Ok(GymServer::new(app.clone())),
        Arc::new(NeverSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_allowed_hosts([host.as_str(), "localhost", "127.0.0.1"]),
    );

    let router = Router::new()
        .nest_service("/mcp", mcp)
        .layer(middleware::from_fn_with_state(auth.clone(), auth::require_bearer))
        .route("/.well-known/oauth-protected-resource", get(auth::metadata))
        .route("/.well-known/oauth-protected-resource/mcp", get(auth::metadata))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(auth);

    let bind = env_or("BIND_ADDR", "0.0.0.0:8080");
    tracing::info!("listening on {bind}, MCP at {public_url}/mcp");
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, router).with_graceful_shutdown(async { let _ = tokio::signal::ctrl_c().await; }).await?;
    Ok(())
}
