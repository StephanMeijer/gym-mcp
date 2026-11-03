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
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

#[derive(Deserialize)]
struct Claims {
    sub: String,
}

/// Why a token was rejected. `UnknownKid` is the one a key refresh can fix.
#[derive(Debug, PartialEq)]
pub enum TokenError {
    UnknownKid(String),
    Invalid(String),
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::UnknownKid(kid) => write!(f, "unknown kid {kid}"),
            TokenError::Invalid(m) => f.write_str(m),
        }
    }
}

/// Pure token check: signature (by `kid`), `iss`, `aud`, `exp`. Returns the `sub`.
pub fn validate_token(token: &str, keys: &JwkSet, issuer: &str, audience: &str) -> Result<String, TokenError> {
    let invalid = TokenError::Invalid;
    let header = decode_header(token).map_err(|e| invalid(format!("malformed token: {e}")))?;
    if !ASYMMETRIC.contains(&header.alg) {
        return Err(invalid(format!("algorithm {:?} not accepted", header.alg)));
    }
    let kid = header.kid.ok_or_else(|| invalid("token has no kid".into()))?;
    let jwk = keys.find(&kid).ok_or(TokenError::UnknownKid(kid.clone()))?;
    let key = DecodingKey::from_jwk(jwk).map_err(|e| invalid(format!("bad jwk: {e}")))?;
    let mut v = Validation::new(header.alg);
    v.set_issuer(&[issuer]);
    v.set_audience(&[audience]);
    v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    let data = decode::<Claims>(token, &key, &v).map_err(|e| invalid(format!("invalid token: {e}")))?;
    Ok(data.claims.sub)
}

impl Auth {
    pub async fn discover(issuer: String, resource: String) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?;
        let base = issuer.trim_end_matches('/').to_owned();
        let url = format!("{base}/.well-known/openid-configuration");
        let conf: serde_json::Value = http
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("openid-configuration")?;
        // Tokens carry the document's `iss` exactly; OIDC_ISSUER may differ from it by a trailing slash.
        let issuer = conf["issuer"]
            .as_str()
            .context("discovery document has no issuer")?
            .to_owned();
        anyhow::ensure!(
            issuer.trim_end_matches('/') == base,
            "OIDC_ISSUER {base} doesn't match the discovery document's issuer {issuer}"
        );
        let jwks_uri = conf["jwks_uri"]
            .as_str()
            .context("discovery document has no jwks_uri")?
            .to_owned();
        let keys: JwkSet = http
            .get(&jwks_uri)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("jwks")?;
        Ok(Self {
            issuer,
            resource,
            jwks_uri,
            keys: RwLock::new((keys, Instant::now())),
            http,
            dev_sub: None,
        })
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

    async fn validate(&self, token: &str) -> Result<String, TokenError> {
        let first = validate_token(token, &self.keys.read().await.0, &self.issuer, &self.resource);
        match first {
            // Authelia rotated keys: refetch, at most once a minute.
            Err(TokenError::UnknownKid(_)) => {
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
        Ok(self
            .http
            .get(&self.jwks_uri)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub fn metadata_url(&self) -> String {
        format!(
            "{}/.well-known/oauth-protected-resource",
            self.resource.trim_end_matches('/')
        )
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
    let Some(token) = token else {
        return unauthorized(&auth, None);
    };
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    use axum::{Router, routing::get};
    use jsonwebtoken::{EncodingKey, Header, encode};

    use super::*;

    const AUD: &str = "https://gym.example.com";

    /// A local issuer serving its discovery document (announcing `announce`, else its own URL) and the
    /// fixture JWKS. Returns its URL and a count of JWKS fetches.
    async fn issuer(announce: Option<&str>) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let conf = json!({"issuer": announce.unwrap_or(&base), "jwks_uri": format!("{base}/jwks")});
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let app = Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(move || {
                    let conf = conf.clone();
                    async move { axum::Json(conf) }
                }),
            )
            .route(
                "/jwks",
                get(move || {
                    h.fetch_add(1, SeqCst);
                    async { include_str!("../tests/fixtures/jwks.json") }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, hits)
    }

    /// A token from `iss`, signed with the fixture key under `kid`.
    fn token(iss: &str, kid: &str) -> String {
        let mut h = Header::new(Algorithm::RS256);
        h.kid = Some(kid.into());
        let key = EncodingKey::from_rsa_pem(include_bytes!("../tests/fixtures/test_rsa.pem")).unwrap();
        let exp = chrono::Utc::now().timestamp() + 300;
        encode(&h, &json!({"iss": iss, "aud": [AUD], "sub": "alice", "exp": exp}), &key).unwrap()
    }

    #[tokio::test]
    async fn unknown_kid_refreshes_keys_at_most_once_a_minute() {
        let (base, hits) = issuer(None).await;
        let auth = Auth::discover(base.clone(), AUD.into()).await.unwrap();
        assert_eq!(hits.load(SeqCst), 1);
        // Authelia rotated its keys: none of ours match, and the last fetch was over a minute ago.
        *auth.keys.write().await = (JwkSet { keys: vec![] }, Instant::now() - REFRESH_MIN_INTERVAL);

        assert_eq!(auth.validate(&token(&base, "test")).await, Ok("alice".into()));
        assert_eq!(hits.load(SeqCst), 2, "refetched once");

        let other = auth.validate(&token(&base, "other")).await;
        assert!(matches!(other, Err(TokenError::UnknownKid(_))), "{other:?}");
        assert_eq!(hits.load(SeqCst), 2, "no second fetch within a minute");
    }

    #[tokio::test]
    async fn discover_trusts_the_documents_issuer() {
        let (base, _) = issuer(None).await;
        let auth = Auth::discover(format!("{base}/"), AUD.into()).await.unwrap();
        assert_eq!(
            auth.issuer, base,
            "OIDC_ISSUER's trailing slash doesn't reach the iss check"
        );
        assert_eq!(auth.validate(&token(&base, "test")).await, Ok("alice".into()));

        let (other, _) = issuer(Some("https://elsewhere.example.com")).await;
        assert!(
            Auth::discover(other, AUD.into()).await.is_err(),
            "issuer mismatch fails at startup"
        );
    }
}
