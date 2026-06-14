//! Secret redaction applied to every chunk at ingest time.
//!
//! context-keeper indexes raw Claude Code transcripts, which routinely
//! contain credentials the user pasted or that a tool printed — API keys,
//! tokens, private keys. Left untouched, those would be embedded, stored,
//! returned by `recall`, and shown in the UI — i.e. a user's own leaked
//! secret could resurface. We scrub them here, BEFORE tokenizing /
//! embedding / persisting, so no secret ever enters a derived artifact and
//! searching for a secret can't match it either.
//!
//! Patterns are deliberately HIGH-CONFIDENCE (distinctive prefixes / shapes)
//! to keep false positives near zero — we would rather miss an exotic key
//! than redact ordinary code. Each match becomes `[REDACTED:<kind>]`.

use once_cell::sync::Lazy;
use regex::Regex;

struct Rule {
    kind: &'static str,
    re: Regex,
}

static RULES: Lazy<Vec<Rule>> = Lazy::new(|| {
    let r = |kind: &'static str, pat: &str| Rule {
        kind,
        re: Regex::new(pat).expect("valid redaction regex"),
    };
    vec![
        // PEM private key blocks (any algorithm). Multiline; redact the whole block.
        r(
            "private-key",
            r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
        ),
        // Anthropic / OpenAI-style keys.
        r("anthropic-key", r"sk-ant-[A-Za-z0-9_\-]{20,}"),
        r("openai-key", r"sk-(?:proj-)?[A-Za-z0-9_\-]{20,}"),
        // GitHub tokens (PAT classic/fine-grained, OAuth, app, refresh).
        r(
            "github-token",
            r"(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{60,}",
        ),
        // AWS access key id + secret access key (the secret only when labelled,
        // to avoid eating any 40-char base64 token).
        r("aws-access-key-id", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
        r(
            "aws-secret-key",
            r#"(?i)aws_secret_access_key\s*[:=]\s*['"]?[A-Za-z0-9/+=]{40}['"]?"#,
        ),
        // Google API key.
        r("google-api-key", r"\bAIza[0-9A-Za-z_\-]{35}\b"),
        // Slack tokens.
        r("slack-token", r"xox[baprs]-[A-Za-z0-9\-]{10,}"),
        // Stripe live keys (test keys are not sensitive).
        r("stripe-key", r"\b(?:sk|rk)_live_[A-Za-z0-9]{20,}\b"),
        // Generic bearer token in an Authorization header.
        r(
            "bearer-token",
            r"(?i)authorization:\s*bearer\s+[A-Za-z0-9._\-]{16,}",
        ),
        // JSON Web Tokens (three base64url segments).
        r(
            "jwt",
            r"\beyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\b",
        ),
        // Last, lowest-confidence: an assignment whose KEY name signals a
        // secret and whose VALUE is a non-trivial quoted/bare token. Kept
        // strict (quotes or >=12 chars, no spaces) to avoid prose.
        r(
            "secret-assignment",
            r#"(?i)\b(?:api[_-]?key|secret|access[_-]?token|auth[_-]?token|client[_-]?secret|password|passwd)\b\s*[:=]\s*['"][^'"\n]{8,}['"]"#,
        ),
    ]
});

/// Redact high-confidence secrets from `text`. Returns the cleaned string and
/// the number of redactions made (0 when nothing matched — the common case,
/// for which we avoid allocating a new String).
pub fn redact_secrets(text: &str) -> (std::borrow::Cow<'_, str>, usize) {
    // Cheap pre-filter: only the rules whose tell-tale substrings appear can
    // match, so a chunk with no secrets does near-zero work.
    let lower = text.to_ascii_lowercase();
    let cues = [
        "private key",
        "sk-",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "akia",
        "asia",
        "aws_secret",
        "aiza",
        "xox",
        "sk_live_",
        "rk_live_",
        "bearer",
        "eyj",
        "secret",
        "password",
        "passwd",
        "token",
        "api_key",
        "api-key",
        "apikey",
    ];
    if !cues.iter().any(|c| lower.contains(c)) {
        return (std::borrow::Cow::Borrowed(text), 0);
    }
    let mut out = text.to_string();
    let mut n = 0usize;
    for rule in RULES.iter() {
        let mut replaced = String::new();
        let mut last = 0;
        let mut hit = false;
        for m in rule.re.find_iter(&out) {
            hit = true;
            n += 1;
            replaced.push_str(&out[last..m.start()]);
            replaced.push_str(&format!("[REDACTED:{}]", rule.kind));
            last = m.end();
        }
        if hit {
            replaced.push_str(&out[last..]);
            out = replaced;
        }
    }
    if n == 0 {
        (std::borrow::Cow::Borrowed(text), 0)
    } else {
        (std::borrow::Cow::Owned(out), n)
    }
}

#[cfg(test)]
mod tests {
    use super::redact_secrets;

    fn red(s: &str) -> String {
        redact_secrets(s).0.into_owned()
    }

    #[test]
    fn redacts_common_credentials() {
        let cases = [
            "key=sk-ant-api03-abcdEFGH1234567890abcdEFGH1234567890",
            "export OPENAI_API_KEY=sk-proj-ABCDEFGHIJKLMNOPQRSTUVWX",
            "token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
            "AKIAIOSFODNN7EXAMPLE",
            "AIzaabcdefghijklmnopqrstuvwxyz012345678",
            // Provider prefixes are split via concat! so no contiguous fake
            // token literal sits in the source (keeps secret scanners / GitHub
            // push protection quiet). The redactor sees the full string at
            // runtime, so the test is unchanged.
            concat!("xox", "b-123456789012-abcdefghijklmnop"),
            concat!("stripe sk", "_live_ABCDEFGHIJKLMNOPQRSTUVWX"),
            "Authorization: Bearer abcdef0123456789ABCDEF",
            r#"password = "hunter2hunter2""#,
            r#"client_secret: "abcd1234efgh5678ijkl""#, // gitleaks:allow (fake fixture)
        ];
        for c in cases {
            let out = red(c);
            assert!(out.contains("[REDACTED:"), "should redact: {c} -> {out}");
        }
    }

    #[test]
    fn redacts_private_key_block() {
        let pem = "before\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAA\nmorelines==\n-----END OPENSSH PRIVATE KEY-----\nafter"; // gitleaks:allow (synthetic fixture)
        let out = red(pem);
        assert!(out.contains("[REDACTED:private-key]"));
        assert!(out.contains("before") && out.contains("after"));
        assert!(!out.contains("BEGIN OPENSSH"));
    }

    #[test]
    fn leaves_ordinary_code_alone() {
        // No false positives on normal prose/code with cue-ish words.
        let benign = [
            "Let's add a token-budget cap to the recall hook.",
            "The password field should be masked in the UI.",
            "fn secret_santa(n: u32) -> u32 { n * 2 }",
            "I set the API key in my environment, then ran the test.",
            "git commit -m \"add bearer auth middleware\"",
            "the access token rotation logic lives in ck-store",
        ];
        for b in benign {
            assert_eq!(red(b), b, "should NOT redact: {b}");
        }
    }

    #[test]
    fn counts_multiple_and_preserves_surrounding_text() {
        let s = "a sk-ant-API0123456789abcdefABCD b ghp_0123456789abcdefghijABCDEFGHIJ0123456789 c"; // gitleaks:allow (fake fixtures)
        let (out, n) = redact_secrets(s);
        assert_eq!(n, 2, "two secrets");
        assert!(out.starts_with("a ") && out.ends_with(" c"));
        assert!(!out.contains("sk-ant") && !out.contains("ghp_"));
    }

    #[test]
    fn no_secret_is_zero_cost_borrow() {
        let s = "completely ordinary text with no credentials at all";
        let (out, n) = redact_secrets(s);
        assert_eq!(n, 0);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
    }
}
