//! Strips obvious credentials before anything reaches the database.
//!
//! Callers must redact the full text *before* cutting it to length: most
//! patterns need the whole secret to recognize it.

use std::sync::LazyLock;

use regex::{Captures, Regex};

pub struct Redacted {
    pub text: String,
    pub count: usize,
}

const MARK: &str = "[REDACTED";

/// Patterns whose entire match is a secret.
static WHOLE: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    [
        (
            "private_key",
            r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----[\s\S]*?-----END [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----",
        ),
        // Fails closed when cut: key material after a BEGIN with no END is
        // redacted to the end of the text, and a body that lost its BEGIN
        // line is caught by its END. A bare mention of the marker is kept.
        (
            "private_key",
            r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----\s*(?:[A-Za-z-]+:[^\n]*\s+)*[A-Za-z0-9+/=]{40,}[\s\S]*",
        ),
        ("private_key", r"(?:[A-Za-z0-9+/=]{16,}\s+)+-----END [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----"),
        ("anthropic_key", r"\bsk-ant-[A-Za-z0-9_\-]{20,}"),
        ("openai_key", r"\bsk-(?:proj-)?[A-Za-z0-9_\-]{20,}"),
        ("stripe_key", r"\b(?:sk|rk|pk)_(?:live|test)_[A-Za-z0-9]{16,}"),
        ("github_token", r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{22,})"),
        ("gitlab_token", r"\bglpat-[A-Za-z0-9_\-]{20,}"),
        ("huggingface_token", r"\bhf_[A-Za-z0-9]{30,}"),
        ("npm_token", r"\bnpm_[A-Za-z0-9]{36,}"),
        ("pypi_token", r"\bpypi-[A-Za-z0-9_\-]{50,}"),
        ("docker_token", r"\bdckr_pat_[A-Za-z0-9_\-]{20,}"),
        ("google_api_key", r"\bAIza[0-9A-Za-z_\-]{35}"),
        ("aws_access_key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
        ("slack_token", r"\bxox[abposr]-[A-Za-z0-9\-]{10,}"),
        ("slack_webhook", r"https://hooks\.slack\.com/services/[A-Za-z0-9/_\-]+"),
        ("discord_webhook", r"https://(?:[a-z]+\.)?discord(?:app)?\.com/api/webhooks/[0-9]+/[A-Za-z0-9_\-]+"),
        ("jwt", r"\beyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}"),
    ]
    .into_iter()
    .map(|(kind, pattern)| (kind, Regex::new(pattern).expect("valid redaction pattern")))
    .collect()
});

/// Patterns where group 1 is kept and group 2 is the secret. An optional
/// group 3 (e.g. the `@` of a URL) is kept after the marker. The flag says
/// whether a value that looks like a `$VAR`/`%VAR%` reference is spared.
static PREFIXED: LazyLock<Vec<(Regex, bool)>> = LazyLock::new(|| {
    [
        // scheme://user:password@host, including an empty user (redis://:pw@).
        // Percent-encoded passwords start with `%`, so no reference check.
        (r"(?i)\b([a-z][a-z0-9+.\-]*://[^\s:/@]*:)([^\s@/]+)(@)", false),
        // Bearer <token> anywhere.
        (r"(?i)\b(bearer\s+)([A-Za-z0-9._~+/=\-]{16,})", true),
        // curl/wget -u user:password
        (r"(?i)(\b(?:curl|wget|http)\b[^\n;|&]*?\s(?:-u|--user)\s+[^\s:/]+:)([^\s/]\S*)", true),
        // --password X, --token=X, --api-key X ...
        (
            r"(?i)(?:^|\s)(--?(?:password|passwd|pass|token|secret|api-?key|access-?key|auth-?token|client-?secret)(?:\s+|=))([^\s-]\S*)",
            true,
        ),
        // mysql -pSECRET (attached form), only as the command itself.
        (r"(?:^|[;&|]\s*)((?i:mysql(?:dump|admin)?)\b[^\n;|&]*?\s-p)([^\s\-]\S*)", true),
        // sqlcmd -P SECRET
        (r"(?:^|[;&|]\s*)((?i:sqlcmd)\b[^\n;|&]*?\s-P\s+)(\S+)", true),
        // setx DB_PASSWORD value
        (r"(?i)(\bsetx\s+\S*(?:password|passwd|secret|token|key|credential)\S*\s+)(\S+)", true),
        // [Environment]::SetEnvironmentVariable('X_TOKEN', 'value', ...)
        (
            r#"(?i)(SetEnvironmentVariable\(\s*['"][^'"]*(?:password|passwd|secret|token|key|credential)[^'"]*['"]\s*,\s*['"])([^'"]+)"#,
            true,
        ),
        // ConvertTo-SecureString 'plain text' -AsPlainText
        (r#"(?i)(ConvertTo-SecureString\s+(?:-String\s+)?['"])([^'"]+)"#, true),
    ]
    .into_iter()
    .map(|(pattern, spare_refs)| (Regex::new(pattern).expect("valid redaction pattern"), spare_refs))
    .collect()
});

/// `Authorization: <scheme> <credentials>`, header or assignment.
static AUTHORIZATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b((?:proxy-)?authorization["']?\s*[:=]\s*["']?)(?:([A-Za-z][A-Za-z0-9_\-]*)(\s+))?([A-Za-z0-9._~+/=\-]{8,})"#,
    )
    .unwrap()
});
const AUTH_SCHEMES: &[&str] =
    &["basic", "bearer", "token", "digest", "negotiate", "ntlm", "apikey", "aws4-hmac-sha256", "sso-key", "otp"];

/// `API_KEY=...`, `"password": "..."` and similar assignments. Groups 3-6
/// are the value: double-quoted, single-quoted, bare, or with an opening
/// quote that never closes (cut text).
static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b([a-z0-9_.\-]*(?:api[_\-]?key|secret|token|password|passwd|access[_\-]?key|account[_\-]?key|app[_\-]?key|private[_\-]?key|signing[_\-]?key|encryption[_\-]?key|credentials?)[a-z0-9_.\-]*)(["']?\s*[:=]\s*)(?:"([^"\n]*)"|'([^'\n]*)'|([^\s"',;]+)|["']([^\s"',;]+))"#,
    )
    .unwrap()
});

pub fn redact(input: &str) -> Redacted {
    let mut count = 0;
    let mut text = input.to_string();
    for (kind, re) in WHOLE.iter() {
        text = apply(re, text, &mut count, |_| Some(format!("{MARK}:{kind}]")));
    }
    for (re, spare_refs) in PREFIXED.iter() {
        text = apply(re, text, &mut count, |c| {
            let secret = &c[2];
            if secret.starts_with(MARK) || c[1].contains(MARK) || (*spare_refs && is_reference(secret)) {
                return None;
            }
            let suffix = c.get(3).map_or("", |m| m.as_str());
            Some(format!("{}{MARK}]{suffix}", leading(c)))
        });
    }
    text = apply(&AUTHORIZATION, text, &mut count, |c| {
        let credential = &c[4];
        let known_scheme = c.get(2).is_some_and(|s| AUTH_SCHEMES.contains(&s.as_str().to_ascii_lowercase().as_str()));
        let secret = !credential.starts_with(MARK)
            && (known_scheme || (c.get(2).is_none() && looks_random(credential) && !is_dotted_identifier(credential)));
        let scheme = format!("{}{}", c.get(2).map_or("", |m| m.as_str()), c.get(3).map_or("", |m| m.as_str()));
        secret.then(|| format!("{}{scheme}{MARK}]", &c[1]))
    });
    text = apply(&ASSIGNMENT, text, &mut count, |c| {
        let key = c[1].to_ascii_lowercase();
        let quoted = c.get(3).is_some() || c.get(4).is_some() || c.get(6).is_some();
        let value = c.get(3).or(c.get(4)).or(c.get(5)).or(c.get(6)).map_or("", |m| m.as_str());
        // `KEY=value` with nothing around the `=` is a literal assignment;
        // `password: &str` or `password == x` is code.
        let literal = quoted || &c[2] == "=";
        let secret = if is_explicit_secret_key(&key) && literal {
            is_concrete_value(value) && !looks_like_path_or_code(value)
        } else {
            looks_random(value)
        };
        secret.then(|| format!("{}{}{MARK}]", &c[1], &c[2]))
    });
    Redacted { text, count }
}

/// The text before the secret, including any leading whitespace the
/// pattern consumed outside group 1.
fn leading(c: &Captures) -> String {
    let whole = c.get(0).unwrap();
    let group = c.get(1).unwrap();
    format!("{}{}", &whole.as_str()[..group.start() - whole.start()], group.as_str())
}

fn apply(re: &Regex, text: String, count: &mut usize, replace: impl Fn(&Captures) -> Option<String>) -> String {
    re.replace_all(&text, |c: &Captures| match replace(c) {
        Some(r) => {
            *count += 1;
            r
        }
        None => c[0].to_string(),
    })
    .into_owned()
}

/// Keys that name a secret outright: any literal value is redacted.
/// (`credentials` is left to the randomness check: `credentials: 'include'`.)
fn is_explicit_secret_key(key: &str) -> bool {
    ["password", "passwd", "secret", "private_key", "privatekey", "signing", "encryption"]
        .iter()
        .any(|k| key.contains(k))
}

/// File paths (`/run/secrets/db`, `./sa.json`) and expressions (`=x`,
/// `(a)`, `[...]`) are not secrets themselves.
fn looks_like_path_or_code(value: &str) -> bool {
    value.starts_with(['/', '=', '&', '(', '[', '{', '~'])
        || value.starts_with("./")
        || value.starts_with("../")
        || value.as_bytes().get(1) == Some(&b':') && value.as_bytes().get(2).is_some_and(|b| *b == b'/' || *b == b'\\')
}

fn is_dotted_identifier(value: &str) -> bool {
    value.contains('.')
        && value.split('.').all(|part| !part.is_empty() && part.chars().all(|c| c.is_alphanumeric() || c == '_'))
}

/// A real value rather than a reference, a placeholder or a type name.
fn is_concrete_value(value: &str) -> bool {
    const NOT_VALUES: &[&str] = &[
        "string",
        "str",
        "number",
        "int",
        "bool",
        "boolean",
        "null",
        "none",
        "nil",
        "undefined",
        "true",
        "false",
        "optional",
        "required",
        "any",
        "object",
    ];
    let v = value.trim();
    !v.is_empty() && !v.starts_with(MARK) && !is_reference(v) && !NOT_VALUES.contains(&v.to_ascii_lowercase().as_str())
}

/// For keys like `token` or `api_key` that also name non-secrets
/// (`max_tokens = 4096`): only values that look random count.
fn looks_random(value: &str) -> bool {
    if !is_concrete_value(value) {
        return false;
    }
    let len = value.chars().count();
    let has_digit = value.chars().any(|c| c.is_ascii_digit());
    let has_alpha = value.chars().any(char::is_alphabetic);
    has_alpha && (len >= 16 || (len >= 8 && has_digit))
}

/// Environment-variable references and template placeholders, matched
/// whole, so a password that merely starts with `$` or `%` still counts.
fn is_reference(value: &str) -> bool {
    static REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^(?:\$\{?\w+\}?|\$\(.*|%\w+%|\$env:\w+|<[^<>]*>|\{\{.*\}\})[\s;,)]*$").unwrap()
    });
    let lower = value.to_ascii_lowercase();
    REFERENCE.is_match(value)
        || ["process.env.", "os.environ", "os.getenv", "env.", "env::var", "std::env"]
            .iter()
            .any(|p| lower.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::redact;

    fn assert_redacted(input: &str, leaked: &str) {
        let r = redact(input);
        assert!(r.count >= 1, "nothing redacted in {input:?}");
        assert!(!r.text.contains(leaked), "{leaked:?} survived: {}", r.text);
    }

    fn pem(lines: usize) -> String {
        let body: Vec<String> =
            (0..lines).map(|i| format!("MIIEowIBAAKCAQEA{i:04}xk9QzLmNoPqRsTuVwXyZ0123456789abcdefghij")).collect();
        format!("-----BEGIN RSA PRIVATE KEY-----\n{}\n-----END RSA PRIVATE KEY-----", body.join("\n"))
    }

    #[test]
    fn redacts_vendor_keys() {
        assert_redacted("key is sk-ant-api03-abcdefghijklmnopqrstuv", "abcdefghijklmnop");
        assert_redacted("OPENAI sk-proj-AbCdEfGhIjKlMnOpQrStUv12", "AbCdEfGhIjKl");
        assert_redacted("token ghp_0123456789abcdefghijABCDEFGHIJ012345", "ghp_0123");
        assert_redacted("aws AKIAIOSFODNN7EXAMPLE here", "AKIAIOSFODNN7EXAMPLE");
        assert_redacted("slack xoxb-1234567890-abcdefghij", "xoxb-1234");
        assert_redacted("jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N", "eyJzdWIi");
        assert_redacted("Authorization: Bearer abcdef0123456789abcdef", "abcdef0123456789");
        assert_redacted("stripe sk_live_51HxAbCdEfGhIjKlMnOp", "51HxAbCd");
        assert_redacted("gitlab glpat-AbCdEfGhIjKlMnOpQrSt", "AbCdEfGh");
        assert_redacted("hf hf_AbCdEfGhIjKlMnOpQrStUvWxYz012345", "AbCdEfGh");
        assert_redacted("npm npm_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789", "AbCdEfGh");
        assert_redacted("dckr_pat_AbCdEfGhIjKlMnOpQrStUv", "AbCdEfGh");
        assert_redacted("key=AIzaSyA1234567890abcdefghijklmnopqrstuv", "AIzaSyA12345");
        assert_redacted("curl https://hooks.slack.com/services/T000/B000/XXXXsecretXXXX", "XXXXsecret");
        assert_redacted("https://discord.com/api/webhooks/123456/AbCdEf-secret_tok", "AbCdEf-secret");
    }

    #[test]
    fn redacts_private_keys_even_when_cut() {
        let full = pem(25);
        let r = redact(&format!("before\n{full}\nafter"));
        assert_eq!(r.text, "before\n[REDACTED:private_key]\nafter");

        let head_only = &full[..full.len() / 2];
        let r = redact(&format!("here it is: {head_only}"));
        assert!(!r.text.contains("MIIEow"), "{}", r.text);

        let tail_only = &full[full.len() / 2..];
        let r = redact(&format!("…{tail_only}\nerror: bad key"));
        assert!(!r.text.contains("MIIEow"), "{}", r.text);
        assert!(r.text.contains("error: bad key"));
    }

    #[test]
    fn redacts_url_and_header_credentials() {
        assert_eq!(
            redact("db at postgres://csm:hunter22@localhost:5432/db").text,
            "db at postgres://csm:[REDACTED]@localhost:5432/db"
        );
        assert_eq!(redact("redis://:p4ssw0rd@cache:6379").text, "redis://:[REDACTED]@cache:6379");
        assert_redacted("curl -H 'Authorization: Basic YWRtaW46aHVudGVyMg=='", "YWRtaW46");
        assert_redacted("curl -H \"Authorization: token 0123456789abcdef0123\"", "0123456789abcdef");
    }

    #[test]
    fn redacts_command_line_credentials() {
        assert_redacted("curl -u admin:hunter2 https://x", "hunter2");
        assert_redacted("deploy --password hunter2 --verbose", "hunter2");
        assert_redacted("tool --token=abc --x", "abc");
        assert_redacted("tool --api-key s3cr3t", "s3cr3t");
        assert_redacted("mysql -u root -phunter2 db", "hunter2");
        assert_redacted("sqlcmd -S . -U sa -P hunter2", "hunter2");
        assert_redacted("setx DB_PASSWORD hunter2", "hunter2");
        assert_redacted(
            "[Environment]::SetEnvironmentVariable('SERVICE_TOKEN', '9f86d081884c7d659a2feaa0c55ad015', 'User')",
            "9f86d081",
        );
        assert_redacted("$pw = ConvertTo-SecureString 'Hunter2Hunter2!' -AsPlainText -Force", "Hunter2Hunter2");
        for kept in [
            "mkdir -p build && git log -p",
            "docker run --rm -u 1000:1000 -v ./src:/src node:20 npm ci",
            "docker run -u postgres:postgres img",
            "redis-cli -u redis://localhost:6379 ping",
            "mysql -h 127.0.0.1 -P3307 -u app app_db",
            "docker run -d --name mysql -p3306:3306 mysql:8",
            "docker exec -it mysql mkdir -pv /var/lib/backup",
        ] {
            let r = redact(kept);
            assert_eq!(r.count, 0, "false positive on {kept:?}: {}", r.text);
        }
    }

    #[test]
    fn redacts_url_passwords_that_look_like_references() {
        assert_redacted("psql postgres://app:%40dm1n%21Pass99@db.internal:5432/app", "%40dm1n");
        assert_redacted("mongodb://admin:$tr0ngPassw0rd@mongo:27017", "tr0ngPassw0rd");
    }

    #[test]
    fn redacts_values_whose_closing_quote_was_cut() {
        assert_redacted("export DB_PASSWORD=\"hunter2hunter2", "hunter2hunter2");
        assert_redacted("API_KEY=\"9f86d081884c7d659a2feaa0", "9f86d081");
    }

    #[test]
    fn keeps_a_bare_mention_of_a_key_header() {
        let text = "Decision: signing keys are PKCS#8 (header -----BEGIN PRIVATE KEY-----), not PKCS#1. Loader lives in src/keys.rs.";
        assert_eq!(redact(text).text, text);
    }

    #[test]
    fn redacts_assignments() {
        assert_redacted("DATABASE_PASSWORD=s3cretPassw0rd", "s3cretPassw0rd");
        assert_redacted(r#"{"api_key": "abc123def456"}"#, "abc123def456");
        assert_redacted("client_secret: correcthorsebatterystaple", "correcthorse");
        assert_redacted("PGPASSWORD=letmein psql", "letmein");
        assert_redacted("$env:DB_PASSWORD = 'hunter2'", "hunter2");
        assert_redacted("kubectl create secret generic x --from-literal=password=hunter2", "hunter2");
        assert_redacted(r#"{"password": "correct horse battery staple"}"#, "horse");
        assert_redacted("SECRET_KEY=django-insecure-environ", "django");
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        for text in [
            "max_tokens = 100000",
            "the token: limit is fine",
            "tokenizer = 'porter'",
            "apiKey: process.env.OPENAI_API_KEY",
            "SECRET=$MY_SECRET",
            "password: string;",
            "token = <your-token-here>",
            "we decided to use SQLite FTS5 for search",
            "task-runner-for-a-very-long-identifier",
            "fn login(username: &str, password: &str) -> Result<Token>",
            "if password == expected {",
            "if (password === confirm) {",
            "fetch(url, { credentials: 'include' })",
            "pub struct Db { password: Option<String> }",
            "def login(self, user, password: str):",
            "password: Optional[str] = None",
            "secret: Buffer",
            "const PASSWORD_MIN_LENGTH = 12;",
            "password_expiry_days: 90",
            "\"password\": {\"type\": \"string\"}",
            "AUTH_PASSWORD_VALIDATORS = [",
            "GOOGLE_APPLICATION_CREDENTIALS=./sa.json npm test",
            "MYSQL_ROOT_PASSWORD_FILE=/run/secrets/db",
            "$secret = Get-Secret -Name Foo",
            "$password = Read-Host -AsSecureString",
            "git -c credential.helper= clone https://example.com/repo.git",
            "secret: rotate on deploy",
            "const authorization = req.headers.authorization;",
            "Authorization: implemented in middleware/auth.rs, checked per route",
        ] {
            let r = redact(text);
            assert_eq!(r.count, 0, "false positive on {text:?}: {}", r.text);
            assert_eq!(r.text, text);
        }
    }

    #[test]
    fn is_idempotent() {
        for input in [
            "OPENAI_API_KEY=sk-proj-AbCdEfGhIjKlMnOpQrStUv12",
            "postgres://u:pw123456@h/db",
            "Authorization: Bearer abcdef0123456789abcdef",
            "deploy --password hunter2",
            "PASSWORD=hunter2",
        ] {
            let once = redact(input);
            let twice = redact(&once.text);
            assert_eq!(twice.count, 0, "{input:?} -> {} -> {}", once.text, twice.text);
            assert_eq!(twice.text, once.text);
        }
    }
}
