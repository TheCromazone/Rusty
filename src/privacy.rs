//! Shared redaction and private persistence for the CLI and memory service.
// ------------------------------------------------------------- redaction

/// A key whose value is a secret when it ends with one of these.
const SECRET_KEYS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "secret",
    "token",
    "apikey",
    "api_key",
    "api-key",
    "access_key",
    "access-key",
    "secret_key",
    "secret-key",
    "private_key",
    "private-key",
    "credential",
    "credentials",
    "key-data",
];

/// Tokens that are secrets by their shape alone.
const SECRET_PREFIXES: &[&str] = &[
    "AKIA",
    "ASIA",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxr-",
    "xoxs-",
    "xapp-",
    "sk-",
    "sk_live_",
    "sk_test_",
    "rk_live_",
    "rk_test_",
    "AIza",
    "nvapi-",
    "hvs.",
    "hvb.",
    "npm_",
    "pypi-",
    "dop_v1_",
    "ya29.",
    "shpat_",
    "shpss_",
    "sq0atp-",
    "SG.",
    "eyJ",
];

const REDACTED: &str = "[redacted]";

/// Replaces secrets in tool output with `[redacted]` and counts them.
/// Handles key=value and key: value pairs with secret-looking keys, tokens
/// with well-known prefixes, bearer and basic auth headers, URL passwords,
/// PEM private keys and the data of Kubernetes Secrets.
pub fn redact(text: &str) -> (String, usize) {
    redact_by(text, &mut |_| REDACTED.to_string())
}

/// Like [`redact`], but each secret is shown as a handle such as
/// `${RUSTY_SECRET_1}` instead of `[redacted]`, and [`secret_env`] hands the
/// value to later shell commands under that name. A model can then use a
/// secret it fetched without ever reading it. Values already handed out are
/// replaced wherever they show up again, even without a secret-looking key.
pub fn redact_with_handles(text: &str) -> (String, usize) {
    let known: Vec<String> = vault().clone();
    let mut text = text.to_string();
    let mut count = 0;
    for (i, value) in known.iter().enumerate() {
        if value.chars().count() >= MIN_RECALL && text.contains(value.as_str()) {
            count += text.matches(value.as_str()).count();
            text = text.replace(value.as_str(), &handle_name(i));
        }
    }
    let (out, n) = redact_by(&text, &mut handle);
    (out, n + count)
}

/// Redacts one tool's output for the model. A secret store hands back its
/// secret in a plain field such as `value`, with nothing secret-looking
/// around it, so when the tool's name says it deals in secrets those fields
/// count as secrets too. `handles` picks [`redact_with_handles`] over
/// [`redact`].
pub fn redact_tool_output(tool: &str, text: &str, handles: bool) -> (String, usize) {
    let mut hidden = 0;
    let mut text = std::borrow::Cow::Borrowed(text);
    let name = tool.to_ascii_lowercase();
    if name.contains("secret") || name.contains("credential") {
        if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&text) {
            hidden = hide_secret_fields(&mut json, handles);
            if hidden > 0 {
                text = serde_json::to_string_pretty(&json).unwrap_or_default().into();
            }
        }
    }
    let (out, n) = if handles { redact_with_handles(&text) } else { redact(&text) };
    (out, n + hidden)
}

fn hide_secret_fields(value: &mut serde_json::Value, handles: bool) -> usize {
    match value {
        serde_json::Value::Object(map) => map
            .iter_mut()
            .map(|(k, v)| match v {
                serde_json::Value::String(s)
                    if !s.is_empty() && matches!(k.as_str(), "value" | "secret" | "plaintext" | "data") =>
                {
                    *s = if handles { handle(s) } else { REDACTED.to_string() };
                    1
                }
                _ => hide_secret_fields(v, handles),
            })
            .sum(),
        serde_json::Value::Array(items) => items.iter_mut().map(|v| hide_secret_fields(v, handles)).sum(),
        _ => 0,
    }
}

/// The handed-out secrets as environment variables for a shell command.
pub fn secret_env() -> Vec<(String, String)> {
    vault().iter().enumerate().map(|(i, v)| (format!("{HANDLE}{}", i + 1), v.clone())).collect()
}

/// Secrets taken out of tool output this session, in handle order. Memory
/// only: never written anywhere, gone when the process ends.
fn vault() -> std::sync::MutexGuard<'static, Vec<String>> {
    static VAULT: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    VAULT.lock().unwrap_or_else(|e| e.into_inner())
}

const HANDLE: &str = "RUSTY_SECRET_";
const MAX_HANDLES: usize = 100;
/// Shorter values are only replaced where a pattern finds them, so a short
/// password can't blank out every matching word in later output.
const MIN_RECALL: usize = 6;

fn handle_name(i: usize) -> String {
    format!("${{{HANDLE}{}}}", i + 1)
}

fn handle(value: &str) -> String {
    let mut v = vault();
    let i = match v.iter().position(|s| s == value) {
        Some(i) => i,
        None if v.len() < MAX_HANDLES => {
            v.push(value.to_string());
            v.len() - 1
        }
        None => return REDACTED.to_string(),
    };
    handle_name(i)
}

fn redact_by(text: &str, mark: &mut dyn FnMut(&str) -> String) -> (String, usize) {
    let mut lines: Vec<String> = Vec::new();
    let mut count = 0;
    let mut in_pem = false;
    let mut in_secret_data = false;
    let is_secret_object = text.contains("kind: Secret") || text.contains("\"kind\": \"Secret\"");
    for line in text.lines() {
        if in_pem {
            in_pem = !line.contains("-----END");
            continue;
        }
        if line.contains("-----BEGIN") && line.contains("PRIVATE KEY") {
            in_pem = true;
            count += 1;
            lines.push("[redacted private key]".into());
            continue;
        }
        let trimmed = line.trim_start();
        if is_secret_object {
            // Values under `data:` / `stringData:` until the block dedents.
            let indent = line.len() - trimmed.len();
            if matches!(trimmed.trim_end_matches(" {"), "data:" | "stringData:" | "\"data\":" | "\"stringData\":") {
                in_secret_data = true;
                lines.push(line.into());
                continue;
            }
            if in_secret_data && (indent == 0 || trimmed.starts_with('}')) {
                in_secret_data = false;
            }
            if in_secret_data {
                if let Some((k, v)) = split_kv(trimmed) {
                    count += 1;
                    let (v, tail) = match v.strip_suffix(',') {
                        Some(v) => (v, ","),
                        None => (v, ""),
                    };
                    let quoted = v.len() >= 2 && v.starts_with('"') && v.ends_with('"');
                    let v = if quoted { &v[1..v.len() - 1] } else { v };
                    let q = if quoted { "\"" } else { "" };
                    lines.push(format!("{}{k} {q}{}{q}{tail}", &line[..indent], mark(v)));
                    continue;
                }
            }
        }
        let (redacted, n) = redact_line(line, mark);
        count += n;
        lines.push(redacted);
    }
    let mut out = lines.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    // Also hide actual configured secrets, including providers with unfamiliar
    // token formats. Capture once; inspecting the environment is not a hook cost.
    static SECRETS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    for secret in SECRETS.get_or_init(|| {
        std::env::vars_os()
            .filter_map(|(k, v)| {
                let (k, v) = (k.to_str()?, v.to_str()?);
                (secret_key(k) && v.len() >= 8).then(|| v.to_owned())
            })
            .collect()
    }) {
        if out.contains(secret) {
            count += out.matches(secret).count();
            out = out.replace(secret, REDACTED);
        }
    }
    (out, count)
}

/// Scrub each string rather than serialized JSON, preserving JSON structure.
pub fn scrub(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => *s = redact(s).0,
        serde_json::Value::Array(v) => v.iter_mut().for_each(scrub),
        serde_json::Value::Object(v) => {
            for (k, v) in v {
                if secret_key(k) && v.is_string() {
                    *v = serde_json::Value::String(REDACTED.into());
                } else {
                    scrub(v);
                }
            }
        }
        _ => {}
    }
}

/// Refuse symlinks, tighten existing files, and never create a public file first.
pub fn private_file(path: &std::path::Path, append: bool) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    if !f.metadata()?.is_file() {
        return Err(std::io::Error::other("private persistence requires a regular file"));
    }
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    if !append {
        f.set_len(0)?;
    }
    Ok(f)
}

pub fn private_dir(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::other("private directory must be owned by this user and not a symlink"));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

pub fn write_json(path: &std::path::Path, mut value: serde_json::Value) -> anyhow::Result<()> {
    use std::io::Write;
    scrub(&mut value);
    let mut writer = std::io::BufWriter::new(private_file(path, false)?);
    serde_json::to_writer(&mut writer, &value)?;
    writer.flush()?;
    Ok(())
}

/// `key: value` or `"key": "value",` → (key part including the separator, value).
fn split_kv(s: &str) -> Option<(&str, &str)> {
    let i = s.find(':')?;
    let value = s[i + 1..].trim();
    (!value.is_empty()).then_some((&s[..=i], value))
}

/// What the next token is, once a key or header word has been seen.
enum Expect {
    /// `Bearer x`: the next word, whatever separates them.
    Header,
    /// `key=x`, `key: x`, `"key": "x"`: needs an assignment between them.
    Key,
    /// `--password x`: a flag may take its value after a plain space.
    Flag,
}

fn redact_line(line: &str, mark: &mut dyn FnMut(&str) -> String) -> (String, usize) {
    let (line, mut count) = redact_urls(line, mark);
    let mut out = String::with_capacity(line.len());
    let mut rest = line.as_str();
    let mut expect: Option<Expect> = None;
    let mut key = "";
    while !rest.is_empty() {
        let start = rest.find(is_token_char).unwrap_or(rest.len());
        let (sep, after) = rest.split_at(start);
        let end = after.find(|c| !is_token_char(c)).unwrap_or(after.len());
        let (token, after) = after.split_at(end);
        rest = after;
        out.push_str(sep);
        if token.is_empty() {
            break;
        }
        let wanted = match expect.take() {
            // `Bearer <api key>` in docs is a placeholder, not a credential.
            Some(Expect::Header) => !sep.is_empty() && sep.trim().is_empty(),
            Some(Expect::Key) => is_assignment(sep),
            Some(Expect::Flag) => is_assignment(sep) || (!sep.is_empty() && sep.trim().is_empty()),
            None => false,
        };
        let prefix_len = line.len() - rest.len() - token.len() - sep.len();
        if (wanted && secret_value(token) && !code_expression(key, sep, token, rest, &line[..prefix_len]))
            || secret_shape(token)
        {
            let padded = rest.trim_start_matches('='); // base64 padding
            let value = &line[line.len() - rest.len() - token.len()..line.len() - padded.len()];
            out.push_str(&mark(value));
            count += 1;
            rest = padded;
            continue;
        }
        out.push_str(token);
        key = token;
        expect = if matches!(token.to_ascii_lowercase().as_str(), "bearer" | "basic") {
            Some(Expect::Header)
        } else if secret_key(token) {
            Some(if token.starts_with("--") { Expect::Flag } else { Expect::Key })
        } else {
            None
        };
    }
    (out, count)
}

/// `token = self.token.clone()`, `self.api_key = api_key`, `access_token: String,`:
/// an unquoted value that is a call, an index, a path, a known type name or
/// the key's own name again is source code, not a secret. Anything else
/// counts, including `{password: …}` in a YAML map, `$(mysql --password=…)` and
/// `password: SecretValue,`: a capitalised word is not proof of a type.
fn code_expression(key: &str, sep: &str, value: &str, rest: &str, prefix: &str) -> bool {
    if sep.ends_with(['"', '\'']) {
        return false;
    }
    // A quoted key with a bare value is ambiguous in ordinary text and YAML.
    // Exempt it only when the surrounding source explicitly identifies Rust's
    // `json!` macro; otherwise a scalar such as `"password": hunter2` leaks.
    if sep.starts_with(['"', '\'']) && prefix.contains("json!(") {
        return true;
    }
    let name = |s: &str| s.rsplit('.').next().unwrap_or("").replace(['_', '-'], "").to_ascii_lowercase();
    let after = rest.trim_start();
    let typed_field = sep.trim_end().ends_with(':')
        && is_common_type(value)
        && (after.is_empty() || after.starts_with([',', ';', ')', '=', '>']));
    rest.starts_with(['(', '[', '<'])
        || rest.starts_with("::")
        || name(value) == name(key.trim_start_matches('-'))
        || typed_field
}

/// Type names a field is commonly declared with, in Rust, Python and TypeScript.
fn is_common_type(token: &str) -> bool {
    matches!(
        token,
        "String"
            | "str"
            | "string"
            | "bool"
            | "boolean"
            | "char"
            | "int"
            | "float"
            | "number"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "f32"
            | "f64"
            | "Vec"
            | "HashMap"
            | "HashSet"
            | "BTreeMap"
            | "BTreeSet"
            | "PathBuf"
            | "Duration"
    )
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+' | '/' | '@' | '~' | '%')
}

/// `=`, `: `, `": "` and the like, ignoring quotes and spaces. A bare colon
/// joins a namespaced name (`secret:access`, `token:read`), not a value.
fn is_assignment(sep: &str) -> bool {
    let core: String = sep.chars().filter(|c| !c.is_whitespace() && !matches!(c, '"' | '\'')).collect();
    core == "=" || (core == ":" && sep != ":")
}

fn secret_key(key: &str) -> bool {
    let k = key.trim_end_matches(|c: char| c.is_ascii_digit() || c == '_').to_ascii_lowercase();
    // The shell's working-directory variables end in "pwd" but hold paths.
    if matches!(k.as_str(), "pwd" | "oldpwd") {
        return false;
    }
    SECRET_KEYS.iter().any(|s| k.ends_with(s))
}

/// Short values and placeholders are not secrets. `${VAR}` and `<value>`
/// never reach here: `$` and `<` are not token characters, so the
/// assignment check fails on them.
fn secret_value(v: &str) -> bool {
    !v.is_empty() && !matches!(v.to_ascii_lowercase().as_str(), "null" | "none" | "true" | "false" | "changeme")
}

fn secret_shape(token: &str) -> bool {
    if token.starts_with("eyJ") {
        // A JWT: three base64url parts.
        return token.split('.').count() == 3 && token.len() > 30;
    }
    SECRET_PREFIXES.iter().any(|p| token.starts_with(p) && token.len() >= p.len() + 12)
}

/// `scheme://user:pass@host` anywhere in the line → `scheme://user:[redacted]@host`.
fn redact_urls(line: &str, mark: &mut dyn FnMut(&str) -> String) -> (String, usize) {
    let mut out = String::with_capacity(line.len());
    let mut count = 0;
    let mut rest = line;
    while let Some(i) = rest.find("://") {
        let scheme_start = rest[..i].rfind(|c: char| !c.is_ascii_alphanumeric() && c != '+').map_or(0, |j| j + 1);
        let url_end = rest[i..]
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>'))
            .map_or(rest.len(), |e| i + e);
        let url = &rest[scheme_start..url_end];
        out.push_str(&rest[..scheme_start]);
        match redact_url(url, mark) {
            Some(redone) => {
                out.push_str(&redone);
                count += 1;
            }
            None => out.push_str(url),
        }
        rest = &rest[url_end..];
    }
    out.push_str(rest);
    (out, count)
}

fn redact_url(url: &str, mark: &mut dyn FnMut(&str) -> String) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let (userinfo, host) = rest.split_once('@')?;
    let (user, pass) = userinfo.split_once(':')?;
    (!pass.is_empty() && !host.is_empty() && !userinfo.contains('/') && !pass.starts_with("${"))
        .then(|| format!("{scheme}://{user}:{}@{host}", mark(pass)))
}

/// The line appended to a tool result that had secrets in it. With handles,
/// it says how to use one; without, how to work around not seeing it.
pub fn redaction_note(n: usize, handles: bool) -> String {
    let what =
        format!("rusty redacted {n} secret value{} from this output before you saw it", if n == 1 { "" } else { "s" });
    if handles {
        return format!(
            "\n[{what}. Each one is shown as a name like ${{{HANDLE}1}}, and your bash commands have that variable \
             set to the real value, so use the name inside a command: psql \"postgresql://user:${{{HANDLE}1}}@host/db\", \
             curl -H \"Authorization: Bearer ${{{HANDLE}2}}\". The same secret keeps the same name. Don't re-run the \
             command to see a value, and don't print one: output is redacted again.]"
        );
    }
    format!(
        "\n[{what}. Never try to print or copy a secret, and don't re-run the command hoping to see it: the value \
         works, you just can't read it. To use one, pass it inside a single command without printing it \
         (DSN=$(...) && psql \"$DSN\" -c ...). To change one in a file, rewrite its whole line \
         (sed -i 's/^KEY=.*/KEY=.../') rather than matching the value.]"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_name_each_secret_and_hand_it_to_shells() {
        // Built at run time so the source holds no credential-shaped literal.
        let (user, pass) = ("app", ["handle", "test", "pass"].join("-"));
        let (out, n) = redact_with_handles(&format!(
            "DB_PASSWORD={pass}\nurl: postgres://{user}:{pass}@db:5432/x\ntoken: aGFuZGxlLXRvay=="
        ));
        assert_eq!(n, 3, "{out}");
        let name = |v: &str| {
            let (k, _) = secret_env().into_iter().find(|(_, val)| val == v).expect(v);
            format!("${{{k}}}")
        };
        let name_of_pass = name(&pass);
        assert!(name_of_pass.starts_with("${RUSTY_SECRET_"), "{pass}");
        // One secret keeps one name, wherever it shows up; base64 padding stays with its value.
        assert!(out.contains(&format!("DB_PASSWORD={name_of_pass}\n")), "{out}");
        assert!(out.contains(&format!("postgres://{user}:{name_of_pass}@db:5432/x")), "{out}");
        assert!(out.contains(&format!("token: {}", name("aGFuZGxlLXRvay=="))), "{out}");
        // Echoed later with nothing secret-looking around it, it is still hidden.
        let (again, n) = redact_with_handles(&format!("value is {pass}."));
        assert_eq!((again, n), (format!("value is {name_of_pass}."), 1));
        // A handle is not itself a secret, and plain redaction is unchanged.
        assert_eq!(redact(&out).1, 0, "{out}");
        assert_eq!(redact(&format!("DB_PASSWORD={pass}")).0, "DB_PASSWORD=[redacted]");
        let secret = "kind: Secret\ndata:\n  \"pw\": \"aGFuZGxlLXNlY3JldA==\",\n";
        let (out, _) = redact_with_handles(secret);
        assert!(out.contains(&format!("  \"pw\": \"{}\",\n", name("aGFuZGxlLXNlY3JldA=="))), "{out}");
    }

    #[test]
    fn a_secret_tool_s_value_field_is_a_secret() {
        let key = ["tp", "live", "shop", "8c41f0e2b7d9"].join("_");
        let answer = format!("{{\"version\": 1, \"stage\": \"current\", \"value\": \"{key}\"}}");
        let (out, n) = redact_tool_output("mcp__simcloud__secret_access", &answer, true);
        assert_eq!(n, 1, "{out}");
        assert!(!out.contains(&key) && out.contains("\"stage\": \"current\""), "{out}");
        assert!(secret_env().iter().any(|(_, v)| *v == key));
        // Shown again later by any tool, it stays hidden.
        assert!(!redact_with_handles(&format!("key={key}")).0.contains(&key));
        // Without handles it is plainly redacted; other tools' value fields are data.
        assert_eq!(redact_tool_output("mcp__x__get_secret", &answer, false).1, 1);
        assert_eq!(redact_tool_output("mcp__x__get_config", "{\"value\": \"blue\"}", true).1, 0);
    }

    #[test]
    fn short_secrets_are_not_hunted_in_later_output() {
        let short = ["ab", "12"].concat();
        let (out, _) = redact_with_handles(&format!("password: {short}"));
        assert!(out.starts_with("password: ${RUSTY_SECRET_"), "{out}");
        let later = format!("t{short} and {short}");
        assert_eq!(redact_with_handles(&later).0, later);
    }

    #[test]
    fn code_expressions_after_a_secret_key_are_not_secrets() {
        for line in [
            "access_token: String,",
            "let token = std::env::var(\"X\").ok();",
            "let token = self.token.clone();",
            "self.access_token = access.to_string();",
            "let access_token = t[\"access_token\"].as_str();",
            "fn save(&self, id_token: Option<&str>)",
        ] {
            assert_eq!(redact(line), (line.to_string(), 0));
        }
        assert_eq!(redact("\"token\": \"abc123\",").0, "\"token\": \"[redacted]\",");
        assert_eq!(redact("TOKEN=abc123").0, "TOKEN=[redacted]");
        assert_eq!(redact("token: abc123").0, "token: [redacted]");
        assert_eq!(redact("--password hunter2").0, "--password [redacted]");
        assert_eq!(redact("Server=db;Password=hunter2;Port=1").0, "Server=db;Password=[redacted];Port=1");
        assert_eq!(redact("Cookie: session_token=abc123; path=/").0, "Cookie: session_token=[redacted]; path=/");
        assert_eq!(redact("password = hunter2,").0, "password = [redacted],");
        assert_eq!(redact("password = hunter2;").0, "password = [redacted];");
        assert_eq!(redact("password: Hunter2,").0, "password: [redacted],");
        assert_eq!(redact("password: SecretValue,").0, "password: [redacted],");
        assert_eq!(redact("password: SecretValue;").0, "password: [redacted];");
        assert_eq!(redact("\"password\": hunter2").0, "\"password\": [redacted]");
        assert_eq!(redact("  \"access_token\": fresh,").0, "  \"access_token\": [redacted],");
        let json_macro = "let body = json!({\"access_token\": fresh,});";
        assert_eq!(redact(json_macro), (json_macro.to_string(), 0));
        for line in ["self.api_key = api_key", "password = args.password", "Client(api_key=api_key)", "password: str"] {
            assert_eq!(redact(line), (line.to_string(), 0));
        }
        // Closing brackets and trailing commas end values in YAML, shells and
        // connection strings. The value is assembled so no scanner takes the
        // fixture for a credential.
        let pw = ["hun", "ter2"].concat();
        for (line, want) in [
            ("db: {user: admin, password: PW}", "db: {user: admin, password: [redacted]}"),
            ("x=$(mysql --password=PW)", "x=$(mysql --password=[redacted])"),
            ("{user: a, password: PW, port: 5}", "{user: a, password: [redacted], port: 5}"),
            ("Server=db; Password = PW; Port=1", "Server=db; Password = [redacted]; Port=1"),
        ] {
            assert_eq!(redact(&line.replace("PW", &pw)).0, want, "{line}");
        }
    }

    #[test]
    fn the_working_directory_is_not_a_password() {
        assert!(!secret_key("PWD") && !secret_key("OLDPWD"));
        assert!(secret_key("DB_PWD") && secret_key("MYSQL_PWD"));
        assert_eq!(redact("PWD=/home/runner/work/app").0, "PWD=/home/runner/work/app");
    }

    #[test]
    fn private_json_stays_valid_and_refuses_symlinks() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = std::env::temp_dir().join(format!("rusty-privacy-{}", std::process::id()));
        private_dir(&dir).unwrap();
        let path = dir.join("session.json");
        write_json(&path, serde_json::json!({"messages":[{"content":"DB_PASSWORD=canary-value-123"}],"private_key":"-----BEGIN RSA PRIVATE KEY-----\nraw\n-----END RSA PRIVATE KEY-----"})).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let _: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(!text.contains("canary-value") && !text.contains("raw"));
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let link = dir.join("link");
        symlink(&path, &link).unwrap();
        assert!(private_file(&link, false).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
