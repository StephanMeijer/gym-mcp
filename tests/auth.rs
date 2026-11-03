//! Token validation with a fixture RSA key (tests/fixtures; test-only, never used in deployment).

use gym_mcp::auth::{TokenError, validate_token};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::json;

const ISS: &str = "https://auth.example.com";
const AUD: &str = "https://gym.example.com";

fn keys() -> JwkSet {
    serde_json::from_str(include_str!("fixtures/jwks.json")).unwrap()
}

fn token(claims: serde_json::Value, kid: &str) -> String {
    let key = EncodingKey::from_rsa_pem(include_bytes!("fixtures/test_rsa.pem")).unwrap();
    let mut h = Header::new(Algorithm::RS256);
    h.kid = Some(kid.into());
    encode(&h, &claims, &key).unwrap()
}

fn claims(aud: &str, exp_offset: i64) -> serde_json::Value {
    let now = chrono::Utc::now().timestamp();
    json!({"iss": ISS, "aud": [aud], "sub": "alice", "exp": now + exp_offset, "iat": now})
}

#[test]
fn valid_token_yields_sub() {
    assert_eq!(
        validate_token(&token(claims(AUD, 300), "test"), &keys(), ISS, AUD).unwrap(),
        "alice"
    );
}

#[test]
fn rejects_wrong_audience_expired_unknown_kid_and_tampering() {
    assert!(
        validate_token(
            &token(claims("https://other.example.com", 300), "test"),
            &keys(),
            ISS,
            AUD
        )
        .is_err()
    );
    assert!(validate_token(&token(claims(AUD, -600), "test"), &keys(), ISS, AUD).is_err());
    assert_eq!(
        validate_token(&token(claims(AUD, 300), "nope"), &keys(), ISS, AUD),
        Err(TokenError::UnknownKid("nope".into()))
    );
    assert!(
        validate_token(
            &token(claims(AUD, 300), "test"),
            &keys(),
            "https://evil.example.com",
            AUD
        )
        .is_err()
    );

    let t = token(claims(AUD, 300), "test");
    let mut parts: Vec<&str> = t.split('.').collect();
    let forged = token(
        json!({"iss": ISS, "aud": [AUD], "sub": "mallory", "exp": chrono::Utc::now().timestamp() + 300}),
        "test",
    );
    parts[1] = forged.split('.').nth(1).unwrap();
    assert!(
        validate_token(&parts.join("."), &keys(), ISS, AUD).is_err(),
        "payload swapped under old signature"
    );
}

#[test]
fn rejects_symmetric_algorithms() {
    let mut h = Header::new(Algorithm::HS256);
    h.kid = Some("test".into());
    let t = encode(&h, &claims(AUD, 300), &EncodingKey::from_secret(b"secret")).unwrap();
    assert!(validate_token(&t, &keys(), ISS, AUD).is_err());
}
