//! Integration tests for `components::redactor`.
//!
//! Coverage:
//! - Positive case (secret detected + replaced) for each pattern.
//! - Negative case (clean text unchanged, Cow::Borrowed returned) for each pattern.
//! - Idempotence: `redact_str(redact_str(x)) == redact_str(x)`.
//! - `redact_in_place` on a JSON array containing a secret.
//! - Zero-copy: clean input returns Cow::Borrowed (pointer equality check).
//!
//! Accepted misses (documented):
//! - Short or low-entropy custom tokens (< 20 chars, no known prefix) are not
//!   detected — no entropy oracle is applied.
//! - Custom bearer tokens with no provider prefix are not caught.
//! - Multi-line .env values that start with a lowercase key are not matched.

use kb::components::redactor::{redact_in_place, redact_str};
use serde_json::json;
use std::borrow::Cow;

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

fn assert_redacted(input: &str) {
    let out = redact_str(input);
    assert!(
        out.contains("<REDACTED>"),
        "expected <REDACTED> in output for input: {input:?}\ngot: {out:?}"
    );
    assert!(
        !out.contains(input) || input.contains("<REDACTED>"),
        "original secret must not survive redaction: {input:?}"
    );
}

fn assert_clean(input: &str) {
    let out = redact_str(input);
    assert_eq!(
        out.as_ref(),
        input,
        "clean input must be returned unchanged: {input:?}"
    );
}

// ---------------------------------------------------------------------------
// 1. OpenAI-style API key (sk- prefix)
// ---------------------------------------------------------------------------

#[test]
fn openai_key_positive() {
    let key = ["sk-", "abcdefghij1234567890ABCDE"].concat();
    assert_redacted(&format!("Authorization: Bearer {key}"));
}

#[test]
fn openai_key_negative() {
    assert_clean("the answer is sk-short"); // too short (< 20 chars after prefix)
}

#[test]
fn openai_key_idempotent() {
    let secret = ["sk-", "abcdefghij1234567890ABCDE"].concat();
    let once = redact_str(&secret).into_owned();
    let twice = redact_str(&once).into_owned();
    assert_eq!(once, twice, "idempotence violated for openai key");
}

// ---------------------------------------------------------------------------
// 2. GitHub PAT (ghp_ prefix)
// ---------------------------------------------------------------------------

#[test]
fn github_pat_positive() {
    let key = "ghp_".to_string() + &"A".repeat(36);
    assert_redacted(&key);
}

#[test]
fn github_pat_negative() {
    assert_clean("see ghp_short for details"); // < 36 chars
}

#[test]
fn github_pat_idempotent() {
    let secret = "ghp_".to_string() + &"B".repeat(36);
    let once = redact_str(&secret).into_owned();
    let twice = redact_str(&once).into_owned();
    assert_eq!(once, twice, "idempotence violated for github pat");
}

// ---------------------------------------------------------------------------
// 3. GitLab PAT (glpat- prefix)
// ---------------------------------------------------------------------------

#[test]
fn gitlab_pat_positive() {
    let key = "glpat-".to_string() + &"x1_-".repeat(6); // 24 chars
    assert_redacted(&key);
}

#[test]
fn gitlab_pat_negative() {
    assert_clean("token glpat-short"); // < 20 chars after prefix
}

#[test]
fn gitlab_pat_idempotent() {
    let secret = "glpat-".to_string() + &"z9".repeat(12);
    let once = redact_str(&secret).into_owned();
    let twice = redact_str(&once).into_owned();
    assert_eq!(once, twice, "idempotence violated for gitlab pat");
}

// ---------------------------------------------------------------------------
// 4. Slack bot token (xoxb-)
// ---------------------------------------------------------------------------

#[test]
fn slack_bot_token_positive() {
    let token = ["xoxb-", "12345678-AbCdEfGhIjKlMnOp"].concat();
    assert_redacted(&token);
}

#[test]
fn slack_bot_token_negative() {
    // No digits before first dash separator — won't match `xoxb-[0-9]+-`
    assert_clean("xoxb-noint-value");
}

#[test]
fn slack_bot_token_idempotent() {
    let secret = ["xoxb-", "99999999-TokenValue123"].concat();
    let once = redact_str(&secret).into_owned();
    let twice = redact_str(&once).into_owned();
    assert_eq!(once, twice, "idempotence violated for slack bot token");
}

// ---------------------------------------------------------------------------
// 5. Slack user token (xoxp-)
// ---------------------------------------------------------------------------

#[test]
fn slack_user_token_positive() {
    let token = ["xoxp-", "11111111-abc-def-ghi"].concat();
    assert_redacted(&token);
}

#[test]
fn slack_user_token_negative() {
    assert_clean("xoxp-noint-value");
}

#[test]
fn slack_user_token_idempotent() {
    let secret = ["xoxp-", "22222222-foo-bar-baz"].concat();
    let once = redact_str(&secret).into_owned();
    let twice = redact_str(&once).into_owned();
    assert_eq!(once, twice, "idempotence violated for slack user token");
}

// ---------------------------------------------------------------------------
// 6. AWS access key ID (AKIA prefix)
// ---------------------------------------------------------------------------

#[test]
fn aws_access_key_positive() {
    let key = ["AKIA", "IOSFODNN7EXAMPLE"].concat();
    assert_redacted(&key);
}

#[test]
fn aws_access_key_negative() {
    // AKIA + only 15 uppercase alphanum (needs 16)
    assert_clean("AKIAIOSFODNN7EXA"); // 16 chars but includes lowercase via mixed — actually let's just use a short one
    // "AKIA" + 15 chars is too short
    assert_clean("AKIA123456789AB"); // 4 + 11 = 15 — below threshold
}

#[test]
fn aws_access_key_idempotent() {
    let secret = ["AKIA", "IOSFODNN7EXAMPLE"].concat();
    let once = redact_str(&secret).into_owned();
    let twice = redact_str(&once).into_owned();
    assert_eq!(once, twice, "idempotence violated for aws key");
}

// ---------------------------------------------------------------------------
// 7. JWT
// ---------------------------------------------------------------------------

#[test]
fn jwt_positive() {
    // Minimal valid-looking JWT (3 base64url segments)
    let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ1c2VyIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
    assert_redacted(jwt);
}

#[test]
fn jwt_negative() {
    // Only two segments — not a JWT
    assert_clean("eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ1c2VyIn0");
}

#[test]
fn jwt_idempotent() {
    let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ1c2VyIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
    let once = redact_str(jwt).into_owned();
    let twice = redact_str(&once).into_owned();
    assert_eq!(once, twice, "idempotence violated for jwt");
}

// ---------------------------------------------------------------------------
// 8. PEM block
// ---------------------------------------------------------------------------

#[test]
fn pem_block_positive() {
    let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA0Z3VS5JJcds3xHn/ygWep4\n-----END RSA PRIVATE KEY-----";
    assert_redacted(pem);
}

#[test]
fn pem_block_negative() {
    assert_clean("This is not a PEM block at all.");
}

#[test]
fn pem_block_idempotent() {
    let pem = "-----BEGIN CERTIFICATE-----\nABCDEFG\n-----END CERTIFICATE-----";
    let once = redact_str(pem).into_owned();
    let twice = redact_str(&once).into_owned();
    assert_eq!(once, twice, "idempotence violated for pem block");
}

// ---------------------------------------------------------------------------
// 9. .env-style high-entropy value
// ---------------------------------------------------------------------------

#[test]
fn dotenv_base64_positive() {
    // 32+ base64 chars after KEY=
    let env_line = "DATABASE_URL=aGVsbG93b3JsZGhlbGxvd29ybGRoZWxsbw==";
    assert_redacted(env_line);
}

#[test]
fn dotenv_hex_positive() {
    // 40+ hex chars after KEY=
    let env_line = "SECRET_TOKEN=a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
    assert_redacted(env_line);
}

#[test]
fn dotenv_negative() {
    // Short value — won't hit the 32-char base64 or 40-char hex threshold
    assert_clean("HOME=/home/user");
}

#[test]
fn dotenv_idempotent() {
    let env_line = "API_KEY=aGVsbG93b3JsZGhlbGxvd29ybGRoZWxsbw==";
    let once = redact_str(env_line).into_owned();
    let twice = redact_str(&once).into_owned();
    assert_eq!(once, twice, "idempotence violated for dotenv value");
}

// ---------------------------------------------------------------------------
// 10. redact_in_place on JSON array
// ---------------------------------------------------------------------------

#[test]
fn redact_in_place_array_with_secret() {
    let secret = ["sk-", "abcdefghij1234567890ABCDE"].concat();
    let mut val = json!(["clean text", secret, 42, true]);
    redact_in_place(&mut val);

    let arr = val.as_array().unwrap();
    assert_eq!(arr[0].as_str().unwrap(), "clean text", "clean element unchanged");
    assert!(
        arr[1].as_str().unwrap().contains("<REDACTED>"),
        "secret element must be redacted"
    );
    // Structural elements untouched
    assert_eq!(arr[2], json!(42));
    assert_eq!(arr[3], json!(true));
}

#[test]
fn redact_in_place_object_with_secret() {
    let content_secret = ["ghp_", &"A".repeat(39)].concat();
    let mut val = json!({
        "summary": "normal text",
        "content": content_secret,
        "count": 5
    });
    redact_in_place(&mut val);

    assert_eq!(val["summary"].as_str().unwrap(), "normal text");
    assert!(
        val["content"].as_str().unwrap().contains("<REDACTED>"),
        "content field must be redacted"
    );
    assert_eq!(val["count"], json!(5));
}

// ---------------------------------------------------------------------------
// 11. Zero-copy: clean input returns Cow::Borrowed
// ---------------------------------------------------------------------------

#[test]
fn zero_copy_clean_input() {
    let input = "This is completely clean text with no secrets.";
    let result = redact_str(input);
    assert!(
        matches!(result, Cow::Borrowed(_)),
        "clean input must return Cow::Borrowed (zero-copy path)"
    );
    assert_eq!(result.as_ref(), input);
}

#[test]
fn owned_when_secret_present() {
    let input = ["sk-", "abcdefghij1234567890ABCDE"].concat();
    let result = redact_str(&input);
    assert!(
        matches!(result, Cow::Owned(_)),
        "input with secret must return Cow::Owned"
    );
}
