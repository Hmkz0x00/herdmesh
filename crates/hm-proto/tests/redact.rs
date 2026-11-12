//! Adversarial fixtures are synthesized locally; none are live credentials.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use hm_proto::redact::{Kind, MAX_INPUT_BYTES, contains_secret, redact};
use proptest::prelude::*;
use std::time::Instant;

fn payload(n: usize) -> String {
    (0..n)
        .map(|i| b"aB3dE6gH9jK2mN5pQ8sT1vW4yZ7cF0iL"[i % 32] as char)
        .collect()
}
fn key(prefix: &str, n: usize) -> String {
    format!("{prefix}{}", payload(n))
}

#[test]
fn token_families_table() {
    let cases = [
        (format!("AKIA{}", "A1".repeat(8)), Kind::AwsAccessKey),
        (format!("ASIA{}", "B2".repeat(8)), Kind::AwsAccessKey),
        (key("ghp_", 36), Kind::GithubToken),
        (key("gho_", 36), Kind::GithubToken),
        (key("ghs_", 36), Kind::GithubToken),
        (key("github_pat_", 64), Kind::GithubToken),
        (key("sk-", 48), Kind::OpenAiKey),
        (key("sk-ant-", 80), Kind::AnthropicKey),
        (key("xoxb-", 40), Kind::SlackToken),
        (key("xoxp-", 40), Kind::SlackToken),
        (key("AIza", 35), Kind::GoogleApiKey),
        (
            format!("eyJ{}.eyJ{}.{}", payload(24), payload(36), payload(40)),
            Kind::Jwt,
        ),
    ];
    for (secret, kind) in cases {
        let result = redact(&format!("before {secret} after"));
        assert_eq!(
            result.text,
            format!("before [REDACTED:{}] after", kind.as_str()),
            "{kind:?}"
        );
        assert_eq!(result.hits, vec![kind]);
        assert!(contains_secret(&secret));
    }
}

#[test]
fn pem_variants_multiline_and_unterminated() {
    for label in [
        "PRIVATE KEY",
        "RSA PRIVATE KEY",
        "EC PRIVATE KEY",
        "DSA PRIVATE KEY",
        "ENCRYPTED PRIVATE KEY",
        "OPENSSH PRIVATE KEY",
        "ED25519 PRIVATE KEY",
        "SOME FUTURE PRIVATE KEY",
    ] {
        let input = format!(
            "before\n-----BEGIN {label}-----\r\n{}\r\n{}\r\n-----END {label}-----\nafter",
            payload(64),
            payload(32)
        );
        assert_eq!(redact(&input).text, "before\n[REDACTED:private_key]\nafter");
    }
    assert_eq!(
        redact("-----BEGIN PRIVATE KEY-----\ntruncated").text,
        "[REDACTED:private_key]"
    );
}

#[test]
fn context_values_preserve_keys_and_surroundings() {
    for (input, expected) in [
        ("API_KEY=sample-value", "API_KEY=[REDACTED:assignment]"),
        (
            "export db_password = 'sample value' # note",
            "export db_password = '[REDACTED:assignment]' # note",
        ),
        (
            "prefix SERVICE_TOKEN=sample-value suffix",
            "prefix SERVICE_TOKEN=[REDACTED:assignment] suffix",
        ),
        (
            "CLIENT_SECRET=\"sample value\"",
            "CLIENT_SECRET=\"[REDACTED:assignment]\"",
        ),
        (
            "PRIVATE_CONFIG=sample",
            "PRIVATE_CONFIG=[REDACTED:assignment]",
        ),
        (
            "Authorization: Bearer sample-token",
            "Authorization: Bearer [REDACTED:bearer]",
        ),
        (
            "authorization:\tbeArEr\tsample-token",
            "authorization:\tbeArEr\t[REDACTED:bearer]",
        ),
        (
            "postgres://user:sample-password@localhost/db",
            "postgres://user:[REDACTED:password]@localhost/db",
        ),
        (
            "redis://:sample%40password@host:123/db",
            "redis://:[REDACTED:password]@host:123/db",
        ),
        ("PASSWORD=\"a\\\"b\"", "PASSWORD=\"[REDACTED:assignment]\""),
        (
            "TOKEN=\"unterminated\nNEXT=ok",
            "TOKEN=\"[REDACTED:assignment]\nNEXT=ok",
        ),
    ] {
        let result = redact(input);
        assert_eq!(result.text, expected, "{input}");
        assert!(!result.hits.is_empty());
        assert_eq!(redact(&result.text).text, result.text);
        assert!(!contains_secret(&result.text));
    }
}

#[test]
fn aws_nearby_secret_and_entropy() {
    let access = format!("AKIA{}", "A1".repeat(8));
    // A low-entropy synthetic secret still needs redaction when adjacent to an AWS ID.
    let secret = "z".repeat(40);
    for input in [format!("{access}\n{secret}"), format!("{secret} {access}")] {
        let result = redact(&input);
        assert!(result.hits.contains(&Kind::AwsSecretKey));
        assert!(!result.text.contains(&secret));
    }
    for blob in ["0123456789abcdef".repeat(2), payload(64)] {
        assert_eq!(redact(&blob).text, "[REDACTED:entropy]");
    }
}

#[test]
fn negatives_table() {
    let git = format!(
        "git:{}:src/lib.rs#L1-9",
        "0123456789abcdef".repeat(3)[..40].to_owned()
    );
    for input in [
        git.as_str(),
        "550e8400-e29b-41d4-a716-446655440000",
        "This is ordinary prose about API keys and tokens.",
        "fn main() { let token_count = tokens.len(); println!(\"hello\"); }",
        "pub const MAX_TOKEN_COUNT: usize = 32;",
        "let x = value == other;",
        "NAME=example\nPORT=8080",
        "https://example.com/path",
        "postgres://user@host/db",
        "sk-short",
        "ghp_short",
        "AIza-short",
        "-----BEGIN PUBLIC KEY-----\nexample\n-----END PUBLIC KEY-----",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "this_is_a_long_but_ordinary_rust_identifier_name",
        "",
        "秘密のない文章 🦀",
    ] {
        let result = redact(input);
        assert_eq!(result.text, input, "negative: {input}");
        assert!(result.hits.is_empty());
        assert!(!contains_secret(input));
    }
}

#[test]
fn markers_unicode_and_overlap_are_idempotent() {
    for input in [
        format!("🦀 API_TOKEN=\"{}\" 🦀", key("ghp_", 36)),
        format!("Authorization: Bearer {}", key("sk-ant-", 80)),
        "SECRET=[REDACTED:assignment]".into(),
        "PRIVATE_KEY=\"-----BEGIN PRIVATE KEY-----\nbody\n-----END PRIVATE KEY-----\"".into(),
        "PASSWORD=秘密🦀".into(),
        format!(
            "PASSWORD=git:{}:x",
            "0123456789abcdef".repeat(3)[..40].to_owned()
        ),
    ] {
        let once = redact(&input);
        let twice = redact(&once.text);
        assert_eq!(once.text, twice.text);
        assert!(twice.hits.is_empty());
    }
}

#[test]
fn cap_fails_closed_and_full_input_is_fast() {
    let too_big = "x".repeat(MAX_INPUT_BYTES + 1);
    assert_eq!(redact(&too_big).text, "[REDACTED:input_limit]");
    assert_eq!(redact(&too_big).hits, vec![Kind::InputLimit]);
    assert!(contains_secret(&too_big));
    // When run alone, the first fixture includes one-time regex compilation.
    for pattern in [
        "ordinary prose. ",
        "TOKEN=sample\n",
        "-----BEGIN PUBLIC KEY-----\n",
        "a",
        "x://x://x://x://",
    ] {
        let input = pattern.repeat(MAX_INPUT_BYTES / pattern.len() + 1);
        let input = &input[..MAX_INPUT_BYTES];
        let start = Instant::now();
        let _result = redact(input);
        let elapsed = start.elapsed();
        eprintln!("256 KiB redaction: {elapsed:?}");
        assert!(elapsed.as_millis() < 50, "{elapsed:?}");
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, failure_persistence: None, ..ProptestConfig::default() })]
    #[test]
    fn arbitrary_unicode_is_idempotent(input in ".{0,4096}") {
        let first=redact(&input);
        let second=redact(&first.text);
        prop_assert_eq!(&first.text,&second.text);
        prop_assert!(second.hits.is_empty());
    }
    #[test]
    fn adversarial_contexts_are_idempotent(prefix in ".{0,64}", body in "[a-zA-Z0-9_+/=.-]{0,128}", suffix in ".{0,64}") {
        let first=redact(&format!("{prefix}\nTOKEN=\"{body}\"\n{suffix}"));
        prop_assert_eq!(&redact(&first.text).text,&first.text);
    }
}

#[test]
fn credentials_next_to_punctuation_and_nonsensitive_fields() {
    for prefix in ["value=", "path/", "(", "https://host/"] {
        let secret = format!("ghp_{}", "a".repeat(36));
        let result = redact(&format!("{prefix}{secret}"));
        assert!(!result.text.contains(&secret));
        assert!(result.hits.contains(&Kind::GithubToken));
    }
}

#[test]
fn alphabetic_base64_padding_and_jwt_values() {
    let letters = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    assert_eq!(redact(letters).text, "[REDACTED:entropy]");
    assert_eq!(
        redact(&format!("{}==", payload(64))).text,
        "[REDACTED:entropy]"
    );
    let jwt = format!("eyJ{}.eyJ{}.{}", payload(24), payload(36), payload(40));
    assert_eq!(redact(&format!("value={jwt}")).text, "value=[REDACTED:jwt]");
}

#[test]
fn overlapping_contexts_are_stable() {
    for input in [
        "TOKEN=\"prefix -----BEGIN PRIVATE KEY-----\nbody\n-----END PRIVATE KEY----- suffix\"",
        "TOKEN=\"prefix Authorization: Bearer sample-token\"",
        "TOKEN=\"postgres://user:sample-password@host/db\"",
    ] {
        let first = redact(input);
        assert_eq!(redact(&first.text).text, first.text);
        assert!(!contains_secret(&first.text));
    }
}

#[test]
fn large_private_block_with_embedded_token_prefixes_is_linear() {
    let prefix = "-----BEGIN OPENSSH PRIVATE KEY-----\n";
    let suffix = "\n-----END OPENSSH PRIVATE KEY-----";
    let filler = format!("ghp_{} ", "a".repeat(36));
    let size = MAX_INPUT_BYTES - prefix.len() - suffix.len();
    let body = filler.repeat(size / filler.len() + 1);
    let input = format!("{prefix}{}{suffix}", &body[..size]);
    redact("warm up");
    let start = Instant::now();
    let out = redact(&input);
    let elapsed = start.elapsed();
    assert_eq!(out.text, "[REDACTED:private_key]");
    eprintln!("256 KiB nested private block: {elapsed:?}");
    assert!(elapsed.as_millis() < 50, "{elapsed:?}");
}

#[test]
fn rust_bindings_are_not_env_assignments_and_slack_app_tokens_are_secrets() {
    for input in [
        "let token = parsed_token;",
        "let mut api_key_count = 32;",
        "let password_length = 12;",
    ] {
        assert_eq!(redact(input).text, input);
    }
    for prefix in ["xapp-", "xoxc-"] {
        assert_eq!(redact(&key(prefix, 40)).text, "[REDACTED:slack_token]");
    }
}

#[test]
fn urlsafe_entropy_and_sentence_punctuation() {
    let blob = format!("{}-_", payload(64));
    assert_eq!(redact(&blob).text, "[REDACTED:entropy]");
    assert_eq!(
        redact(&format!("{}...", payload(64))).text,
        "[REDACTED:entropy]..."
    );
}
