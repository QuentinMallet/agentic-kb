//! Write-time credential redaction for KB entries.
//!
//! `redact_str` scans a string for known secret patterns and replaces every
//! match with `<REDACTED>`.  The zero-copy path (`Cow::Borrowed`) is taken
//! when no pattern matches, keeping clean writes allocation-free.
//!
//! `redact_in_place` walks a `serde_json::Value` tree and calls `redact_str`
//! on every string leaf; structural nodes (object, array, number, bool, null)
//! are left untouched.
//!
//! # Accepted misses
//!
//! - Short or low-entropy custom tokens (< 20 chars, no known prefix) are not
//!   detected.  There is no entropy oracle; false-positive risk would be too
//!   high for generic base64 strings.
//! - Custom bearer tokens with no provider prefix are not caught.
//! - Multi-line `.env` files embedded inside a larger text block: only lines
//!   that start with `[A-Z][A-Z0-9_]{2,}=` are matched, per the pattern.

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::borrow::Cow;

/// Replacement sentinel inserted in place of a detected secret.
const REDACTED: &str = "<REDACTED>";

// ---------------------------------------------------------------------------
// Pattern list — compiled once at first use.
// ---------------------------------------------------------------------------

struct Pattern {
    re: Regex,
}

static PATTERNS: Lazy<Vec<Pattern>> = Lazy::new(|| {
    let raw: &[&str] = &[
        // Provider-prefixed API keys
        r"sk-[A-Za-z0-9]{20,}",
        r"ghp_[A-Za-z0-9]{36}",
        r"glpat-[A-Za-z0-9_\-]{20,}",
        r"xoxb-[0-9]+-[A-Za-z0-9]+",
        r"xoxp-[0-9]+-[A-Za-z0-9\-]+",
        // AWS access key ID
        r"AKIA[A-Z0-9]{16}",
        // JWT (three base64url segments separated by dots)
        r"eyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+",
        // PEM blocks (multi-line)
        r"-----BEGIN [A-Z ]+-----[\s\S]*?-----END [A-Z ]+-----",
        // .env-style high-entropy values: KEY=<long base64 or hex>
        r"(?m)^[A-Z][A-Z0-9_]{2,}=(?:[A-Za-z0-9+/]{32,}={0,2}|[A-Za-z0-9]{40,})",
    ];
    raw.iter()
        .map(|pat| Pattern {
            re: Regex::new(pat).expect("redactor: invalid regex"),
        })
        .collect()
});

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Apply all redaction patterns to `s`.
///
/// Returns `Cow::Borrowed(s)` unchanged when no pattern matches (zero-copy).
/// Returns `Cow::Owned(result)` with every match replaced by `<REDACTED>`.
pub fn redact_str(s: &str) -> Cow<'_, str> {
    // Check whether any pattern matches before allocating.
    let any_match = PATTERNS.iter().any(|p| p.re.is_match(s));
    if !any_match {
        return Cow::Borrowed(s);
    }

    // Apply patterns sequentially; each pass operates on the result of the
    // previous one so overlapping patterns all get replaced.
    let mut result = s.to_owned();
    for p in PATTERNS.iter() {
        let replaced = p.re.replace_all(&result, REDACTED).into_owned();
        result = replaced;
    }
    Cow::Owned(result)
}

/// Walk a JSON value tree and redact every string leaf in-place.
///
/// Structural nodes (Object, Array, Number, Bool, Null) are traversed or
/// left untouched.  Only `Value::String` leaves are modified.
pub fn redact_in_place(value: &mut Value) {
    match value {
        Value::String(s) => {
            let redacted = redact_str(s);
            if let Cow::Owned(new_s) = redacted {
                *s = new_s;
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                redact_in_place(item);
            }
        }
        Value::Object(map) => {
            for v in map.values_mut() {
                redact_in_place(v);
            }
        }
        // Number, Bool, Null — nothing to redact.
        _ => {}
    }
}
