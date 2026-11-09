use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use gym_mcp::{
    api,
    auth::{self, Auth},
    db::App,
    tools::GymServer,
};
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
    let n = num
        .trim()
        .parse::<u64>()
        .with_context(|| format!("bad duration {s:?}"))?;
    let secs = n
        .checked_mul(mult)
        .with_context(|| format!("duration {s:?} too large"))?;
    // Larger values break the `now - stale_after` timestamp arithmetic in close_stale.
    anyhow::ensure!(
        secs <= MAX_STALE_AFTER_SECS,
        "WORKOUT_STALE_AFTER must be at most 365 days, got {s:?}"
    );
    Ok(Duration::from_secs(secs))
}

const MAX_STALE_AFTER_SECS: u64 = 365 * 24 * 3600;

/// PUBLIC_URL must be a bare origin: the well-known metadata routes are only served at the root.
fn parse_public_url(s: &str) -> anyhow::Result<String> {
    let url = reqwest::Url::parse(s).with_context(|| format!("PUBLIC_URL {s:?} is not a URL"))?;
    anyhow::ensure!(
        url.path() == "/" && url.query().is_none() && url.fragment().is_none(),
        "PUBLIC_URL must be an origin only, with no path, query or fragment, got {s:?}"
    );
    Ok(s.trim_end_matches('/').to_owned())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(env_or("RUST_LOG", "info"))
        .init();

    let public_url = parse_public_url(&env("PUBLIC_URL")?)?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(10)
        .connect(&env("DATABASE_URL")?)
        .await?;
    sqlx::migrate!().run(&pool).await?;
    let app = App {
        pool,
        default_timezone: env_or("DEFAULT_TIMEZONE", "Europe/Amsterdam"),
        stale_after: parse_duration(&env_or("WORKOUT_STALE_AFTER", "3h"))?,
    };

    // MCP tokens have `aud` = PUBLIC_URL, app tokens PUBLIC_URL/api, so neither works on the other's endpoint.
    let api_url = format!("{public_url}/api");
    let (auth, api_auth) = match std::env::var("DEV_SUB") {
        Ok(sub) => {
            tracing::warn!("DEV_SUB is set: authentication is DISABLED, every request acts as {sub:?}");
            (Auth::dev(sub.clone(), public_url.clone()), Auth::dev(sub, api_url))
        }
        Err(_) => {
            let issuer = env("OIDC_ISSUER")?;
            (
                Auth::discover(issuer.clone(), public_url.clone()).await?,
                Auth::discover(issuer, api_url).await?,
            )
        }
    };

    let host = public_url
        .split("://")
        .nth(1)
        .unwrap_or(&public_url)
        .split('/')
        .next()
        .unwrap_or_default()
        .to_owned();
    let mcp = StreamableHttpService::new(
        {
            let app = app.clone();
            move || Ok(GymServer::new(app.clone()))
        },
        Arc::new(NeverSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_allowed_hosts([host.as_str(), "localhost", "127.0.0.1"]),
    );

    let router = auth::router(
        axum::Router::new().fallback_service(mcp),
        Arc::new(auth),
        api::router(app),
        Arc::new(api_auth),
    );

    let bind = env_or("BIND_ADDR", "0.0.0.0:8080");
    tracing::info!("listening on {bind}, MCP at {public_url}/mcp");
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    // Containers stop with SIGTERM; as PID 1 an unhandled one is ignored until SIGKILL.
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_urls() {
        assert_eq!(
            parse_public_url("https://gym.example.com/").unwrap(),
            "https://gym.example.com"
        );
        assert_eq!(
            parse_public_url("http://localhost:8099").unwrap(),
            "http://localhost:8099"
        );
        for bad in [
            "https://host/gym",
            "https://host/gym/",
            "https://host/?a=1",
            "https://host/#x",
            "host",
        ] {
            assert!(parse_public_url(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("3h").unwrap(), Duration::from_secs(3 * 3600));
        assert_eq!(parse_duration("90m").unwrap(), Duration::from_secs(90 * 60));
        assert_eq!(parse_duration("45s").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_duration("10").unwrap(), Duration::from_secs(10));
        for bad in ["", "h", "-1h", "3d", "1.5h"] {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
        let err = parse_duration("5124095576030432h").unwrap_err().to_string();
        assert!(err.contains("too large"), "{err}");
        assert_eq!(parse_duration("8760h").unwrap(), Duration::from_secs(365 * 24 * 3600));
        for bad in ["8761h", "31536001", "9300000000000000"] {
            let err = parse_duration(bad).unwrap_err().to_string();
            assert!(err.contains("at most 365 days"), "{bad:?}: {err}");
        }
    }
}
