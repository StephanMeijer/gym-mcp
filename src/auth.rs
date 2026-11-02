//! OAuth resource server: Authelia issues JWT access tokens, we validate them against its JWKS.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::RwLock;

/// The authenticated user, put in request extensions for the tool handlers.
#[derive(Clone, Debug)]
pub struct Owner(pub String);

pub struct Auth {
    pub issuer: String,
    /// PUBLIC_URL: the `resource` in the metadata and the required `aud`.
    pub resource: String,
    jwks_uri: String,
    keys: RwLock<(JwkSet, Instant)>,
    http: reqwest::Client,
    /// Local development only: skip auth and act as this user.
    pub dev_sub: Option<String>,
}

const REFRESH_MIN_INTERVAL: Duration = Duration::from_secs(60);
const ASYMMETRIC: &[Algorithm] = &[
    Algorithm::RS256, Algorithm::RS384, Algorithm::RS512, Algorithm::PS256, Algorithm::PS384, Algorithm::PS512,
    Algorithm::ES256, Algorithm::ES384, Algorithm::EdDSA,
];

#[derive(Deserialize)]
struct Claims {
    sub: String,
}

/// Pure token check: signature (by `kid`), `iss`, `aud`, `exp`. Returns the `sub`.
pub fn validate_token(token: &str, keys: &JwkSet, issuer: &str, audience: &str) -> Result<String, String> {
    let header = decode_header(token).map_err(|e| format!("malformed token: {e}"))?;
    if !ASYMMETRIC.contains(&header.alg) {
        return Err(format!("algorithm {:?} not accepted", header.alg));
    }
    let kid = header.kid.ok_or("token has no kid")?;
    let jwk = keys.find(&kid).ok_or_else(|| format!("unknown kid {kid}"))?;
    let key = DecodingKey::from_jwk(jwk).map_err(|e| format!("bad jwk: {e}"))?;
    let mut v = Validation::new(header.alg);
    v.set_issuer(&[issuer]);
    v.set_audience(&[audience]);
    v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    let data = decode::<Claims>(token, &key, &v).map_err(|e| format!("invalid token: {e}"))?;
    Ok(data.claims.sub)
}

impl Auth {
    pub async fn discover(issuer: String, resource: String) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?;
        let url = format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/'));
        let conf: serde_json::Value = http.get(&url).send().await?.error_for_status()?.json().await.context("openid-configuration")?;
        let jwks_uri = conf["jwks_uri"].as_str().context("discovery document has no jwks_uri")?.to_owned();
        let keys: JwkSet = http.get(&jwks_uri).send().await?.error_for_status()?.json().await.context("jwks")?;
        Ok(Self { issuer, resource, jwks_uri, keys: RwLock::new((keys, Instant::now())), http, dev_sub: None })
    }

    pub fn dev(sub: String, resource: String) -> Self {
        Self {
            issuer: String::new(),
            resource,
            jwks_uri: String::new(),
            keys: RwLock::new((JwkSet { keys: vec![] }, Instant::now())),
            http: reqwest::Client::new(),
            dev_sub: Some(sub),
        }
    }

    async fn validate(&self, token: &str) -> Result<String, String> {
        let first = validate_token(token, &self.keys.read().await.0, &self.issuer, &self.resource);
        match first {
            // Authelia rotated keys: refetch, at most once a minute.
            Err(e) if e.starts_with("unknown kid") => {
                let mut guard = self.keys.write().await;
                if guard.1.elapsed() >= REFRESH_MIN_INTERVAL {
                    match self.fetch_keys().await {
                        Ok(k) => *guard = (k, Instant::now()),
                        Err(err) => tracing::warn!("jwks refresh failed: {err:#}"),
                    }
                }
                validate_token(token, &guard.0, &self.issuer, &self.resource)
            }
            other => other,
        }
    }

    async fn fetch_keys(&self) -> anyhow::Result<JwkSet> {
        Ok(self.http.get(&self.jwks_uri).send().await?.error_for_status()?.json().await?)
    }

    pub fn metadata_url(&self) -> String {
        format!("{}/.well-known/oauth-protected-resource", self.resource.trim_end_matches('/'))
    }
}

/// RFC 9728 protected resource metadata; tells Claude/ChatGPT to log in at Authelia.
pub async fn metadata(State(auth): State<Arc<Auth>>) -> impl IntoResponse {
    axum::Json(json!({
        "resource": auth.resource,
        "authorization_servers": [auth.issuer],
        "bearer_methods_supported": ["header"],
        "scopes_supported": ["openid", "offline_access"],
    }))
}

pub async fn require_bearer(State(auth): State<Arc<Auth>>, mut req: Request, next: Next) -> Response {
    if let Some(sub) = &auth.dev_sub {
        req.extensions_mut().insert(Owner(sub.clone()));
        return next.run(req).await;
    }
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer ").or_else(|| h.strip_prefix("bearer ")));
    let Some(token) = token else { return unauthorized(&auth, None) };
    match auth.validate(token).await {
        Ok(sub) => {
            req.extensions_mut().insert(Owner(sub));
            next.run(req).await
        }
        Err(e) => {
            tracing::info!("rejected token: {e}");
            unauthorized(&auth, Some("invalid_token"))
        }
    }
}

fn unauthorized(auth: &Auth, error: Option<&str>) -> Response {
    let mut challenge = format!("Bearer resource_metadata=\"{}\"", auth.metadata_url());
    if let Some(e) = error {
        challenge.push_str(&format!(", error=\"{e}\""));
    }
    let mut res = StatusCode::UNAUTHORIZED.into_response();
    if let Ok(v) = HeaderValue::from_str(&challenge) {
        res.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    res
}
