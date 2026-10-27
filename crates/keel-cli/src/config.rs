//! Home and project `config.toml` loader (REQ-3.24, INV-8).
//! Secrets stay in the environment; files may only reference `${VAR}`.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use toml::Value;

const TRUST_FILE: &str = "trusted-configs";

#[derive(Clone)]
pub struct KeelConfig {
    pub servers: Vec<McpServerSpec>,
    pub project_path: Option<PathBuf>,
    pub project_raw: Option<String>,
    pub needs_confirm: bool,
}

#[derive(Clone, Deserialize)]
pub struct McpServerSpec {
    pub name: String,
    #[serde(default = "default_stdio")]
    pub transport: String,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

fn default_stdio() -> String {
    "stdio".into()
}

impl fmt::Debug for KeelConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeelConfig")
            .field("servers", &self.servers)
            .field("project_path", &self.project_path)
            .field("needs_confirm", &self.needs_confirm)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for McpServerSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpServerSpec")
            .field("name", &self.name)
            .field("transport", &self.transport)
            .field("command", &self.command)
            .field("args", &self.args)
            .field("url", &self.url)
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[derive(Default, Deserialize)]
struct FileConfig {
    #[serde(default)]
    mcp: McpSection,
}

#[derive(Default, Deserialize)]
struct McpSection {
    #[serde(default)]
    servers: Vec<McpServerSpec>,
}

pub fn home_config_path(keel_home: &Path) -> PathBuf {
    keel_home.join("config.toml")
}

pub fn project_config_path(cwd: &Path) -> PathBuf {
    repo_root(cwd).join(".keel").join("config.toml")
}

fn repo_root(cwd: &Path) -> PathBuf {
    let mut dir = cwd.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return dir;
        }
        if !dir.pop() {
            return cwd.to_path_buf();
        }
    }
}

pub fn load(keel_home: &Path, cwd: &Path) -> Result<KeelConfig> {
    load_with(keel_home, cwd, &|name| std::env::var(name).ok())
}

fn load_with(
    keel_home: &Path,
    cwd: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<KeelConfig> {
    let home_path = home_config_path(keel_home);
    let project_path = project_config_path(cwd);
    let home_raw = read_optional(&home_path)?;
    let project_raw = read_optional(&project_path)?;
    let same_file = same_path(&home_path, &project_path);

    if let Some(raw) = &home_raw {
        reject_secrets(raw, &home_path)?;
    }
    if let Some(raw) = &project_raw {
        if !same_file {
            reject_secrets(raw, &project_path)?;
        }
    }

    let mut merged = parse_toml(home_raw.as_deref().unwrap_or(""), &home_path)?;
    if !same_file {
        let overlay = parse_toml(project_raw.as_deref().unwrap_or(""), &project_path)?;
        merged = merge_value(merged, overlay);
    }
    subst_value(&mut merged, env)?;

    let parsed: FileConfig = merged
        .try_into()
        .map_err(|e| anyhow!("config: invalid merged settings: {e}"))?;
    for server in &parsed.mcp.servers {
        validate_server(server)?;
    }

    let project_raw = if same_file { None } else { project_raw };
    let project_path = project_raw.as_ref().map(|_| project_path.clone());
    let needs_confirm = match (&project_path, &project_raw) {
        (Some(path), Some(raw)) => needs_confirm(keel_home, path, raw)?,
        _ => false,
    };

    Ok(KeelConfig {
        servers: parsed.mcp.servers,
        project_path,
        project_raw,
        needs_confirm,
    })
}

pub fn needs_confirm(keel_home: &Path, project_config: &Path, raw: &str) -> Result<bool> {
    let key = path_key(project_config);
    let want = content_digest(raw);
    Ok(trust_entries(keel_home)?.get(&key).map(String::as_str) != Some(want.as_str()))
}

pub fn mark_trusted(keel_home: &Path, project_config: &Path, raw: &str) -> Result<()> {
    fs::create_dir_all(keel_home)
        .with_context(|| format!("config: create {}", keel_home.display()))?;
    let key = path_key(project_config);
    let mut entries = trust_entries(keel_home)?;
    entries.insert(key, content_digest(raw));
    let text = toml::to_string(&entries).context("config: encode trusted-configs")?;
    fs::write(keel_home.join(TRUST_FILE), text)
        .with_context(|| format!("config: write {}/{TRUST_FILE}", keel_home.display()))
}

/// Interactive yes/no for a first-seen project config. Tests use `needs_confirm` / `mark_trusted`.
pub fn prompt_trust(project_config: &Path) -> Result<bool> {
    eprintln!(
        "Trust new project config {} for MCP? [y]es [n]o",
        project_config.display()
    );
    let _ = io::stderr().flush();
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "yes"))
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow!(e).context(format!("config: {}", path.display()))),
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn parse_toml(raw: &str, path: &Path) -> Result<Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Value::Table(toml::map::Map::new()));
    }
    trimmed
        .parse::<Value>()
        .with_context(|| format!("config: failed to parse {}", path.display()))
}

fn merge_value(base: Value, overlay: Value) -> Value {
    match (base, overlay) {
        (Value::Table(mut base), Value::Table(overlay)) => {
            for (key, right) in overlay {
                let merged = match base.remove(&key) {
                    Some(left) => merge_value(left, right),
                    None => right,
                };
                base.insert(key, merged);
            }
            Value::Table(base)
        }
        (_, overlay) => overlay,
    }
}

fn subst_value(value: &mut Value, env: &dyn Fn(&str) -> Option<String>) -> Result<()> {
    match value {
        Value::String(s) => *s = substitute_vars(s, env)?,
        Value::Array(items) => {
            for item in items {
                subst_value(item, env)?;
            }
        }
        Value::Table(table) => {
            let keys: Vec<String> = table.keys().cloned().collect();
            for key in keys {
                if let Some(item) = table.get_mut(&key) {
                    subst_value(item, env)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn substitute_vars(input: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str("${");
            rest = after;
            continue;
        };
        let name = &after[..end];
        if !is_ident(name) {
            bail!("config: invalid environment placeholder");
        }
        match env(name) {
            Some(val) => out.push_str(&val),
            None => bail!("config: missing environment variable {name}"),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('A'..='Z' | 'a'..='z' | '_'))
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn reject_secrets(raw: &str, path: &Path) -> Result<()> {
    let value = parse_toml(raw, path)?;
    walk_reject(&value, path, false, None)
}

fn walk_reject(value: &Value, path: &Path, in_headers: bool, key: Option<&str>) -> Result<()> {
    match value {
        Value::Table(table) => {
            let child_headers = key.is_some_and(|k| k.eq_ignore_ascii_case("headers"));
            for (k, child) in table {
                if is_forbidden_key(k) {
                    bail!(
                        "config: {} must not contain API keys or tokens; use ${{VAR}}",
                        path.display()
                    );
                }
                walk_reject(child, path, child_headers, Some(k))?;
            }
            Ok(())
        }
        Value::Array(items) => {
            for child in items {
                walk_reject(child, path, in_headers, None)?;
            }
            Ok(())
        }
        Value::String(s) => {
            if in_headers && key.is_some_and(is_secret_header) && !header_secret_is_var(s) {
                bail!(
                    "config: {} MCP header secrets must use ${{VAR}}",
                    path.display()
                );
            }
            if looks_like_api_key(s) {
                bail!(
                    "config: {} must not contain API keys or tokens; use ${{VAR}}",
                    path.display()
                );
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn is_forbidden_key(key: &str) -> bool {
    let n: String = key
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(|c| c.to_lowercase())
        .collect();
    matches!(
        n.as_str(),
        "apikey"
            | "openaiapikey"
            | "openaikey"
            | "accesstoken"
            | "authtoken"
            | "secret"
            | "password"
            | "token"
    )
}

fn is_secret_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "authorization"
        || n.contains("api-key")
        || n.contains("apikey")
        || n.contains("token")
        || n.contains("secret")
}

fn header_secret_is_var(value: &str) -> bool {
    let v = value.trim();
    let body = v
        .strip_prefix("Bearer ")
        .or_else(|| v.strip_prefix("bearer "))
        .or_else(|| v.strip_prefix("Token "))
        .or_else(|| v.strip_prefix("token "))
        .unwrap_or(v)
        .trim();
    body.strip_prefix("${")
        .and_then(|s| s.strip_suffix('}'))
        .is_some_and(is_ident)
}

fn looks_like_api_key(s: &str) -> bool {
    s.split_whitespace().any(|part| {
        let part = part
            .strip_prefix("Bearer")
            .or_else(|| part.strip_prefix("bearer"))
            .unwrap_or(part)
            .trim_start_matches(':')
            .trim();
        part.starts_with("sk-") && part.len() >= 12
    })
}

fn validate_server(server: &McpServerSpec) -> Result<()> {
    if server.name.trim().is_empty() {
        bail!("config: mcp server name is required");
    }
    match server.transport.as_str() {
        "stdio" => {
            if server.command.as_deref().unwrap_or("").is_empty() {
                bail!("config: mcp server {} missing command", server.name);
            }
        }
        "http" => {
            if server.url.as_deref().unwrap_or("").is_empty() {
                bail!("config: mcp server {} missing url", server.name);
            }
        }
        other => bail!(
            "config: mcp server {} unknown transport {other}",
            server.name
        ),
    }
    Ok(())
}

fn trust_entries(keel_home: &Path) -> Result<BTreeMap<String, String>> {
    let path = keel_home.join(TRUST_FILE);
    match fs::read_to_string(&path) {
        Ok(text) if text.trim().is_empty() => Ok(BTreeMap::new()),
        Ok(text) => toml::from_str(&text)
            .with_context(|| format!("config: failed to parse {}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(anyhow!(e).context(format!("config: {}", path.display()))),
    }
}

fn path_key(project_config: &Path) -> String {
    fs::canonicalize(project_config)
        .unwrap_or_else(|_| project_config.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn content_digest(raw: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in raw.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}-{}", raw.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;

    fn write(path: &Path, body: &str) {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    /// REQ-3.24 project overlay replaces home `[[mcp.servers]]`.
    #[test]
    fn req_3_24_home_overlaid_by_project_config() {
        let home = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        fs::create_dir(proj.path().join(".git")).unwrap();
        write(
            &home.path().join("config.toml"),
            r#"
[[mcp.servers]]
name = "filesystem"
command = "npx"
args = ["-y", "fs"]
"#,
        );
        write(
            &proj.path().join(".keel/config.toml"),
            r#"
[[mcp.servers]]
name = "search"
transport = "http"
url = "https://mcp.example/search"
headers = { Authorization = "Bearer ${KEEL_TEST_SEARCH}" }
"#,
        );
        let env: HashMap<String, String> =
            HashMap::from([("KEEL_TEST_SEARCH".into(), "from-env".into())]);
        let cfg = load_with(home.path(), proj.path(), &|k| env.get(k).cloned()).unwrap();
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.servers[0].name, "search");
        assert_eq!(cfg.servers[0].transport, "http");
        assert_eq!(
            cfg.servers[0]
                .headers
                .get("Authorization")
                .map(String::as_str),
            Some("Bearer from-env")
        );
        assert!(cfg.needs_confirm);
        assert!(!format!("{cfg:?}").contains("from-env"));
    }

    /// REQ-3.24 INV-8 missing `${VAR}` fails without leaving the token.
    #[test]
    fn req_3_24_inv_8_missing_var_fails_clearly() {
        let home = tempfile::tempdir().unwrap();
        write(
            &home.path().join("config.toml"),
            r#"
[[mcp.servers]]
name = "search"
transport = "http"
url = "https://mcp.example/search"
headers = { Authorization = "Bearer ${KEEL_TEST_MISSING}" }
"#,
        );
        let err = load_with(home.path(), home.path(), &|_| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("KEEL_TEST_MISSING"), "{err}");
        assert!(err.contains("missing environment variable"), "{err}");
        assert!(!err.contains("Bearer ${KEEL_TEST_MISSING}"));
    }

    /// REQ-3.24 INV-8 literal API keys and tokens in files are rejected.
    #[test]
    fn req_3_24_inv_8_rejects_literal_secret_in_file() {
        let home = tempfile::tempdir().unwrap();
        let secret = "sk-literalvalue";
        write(
            &home.path().join("config.toml"),
            &format!("[[mcp.servers]]\nname = \"x\"\ncommand = \"npx\"\napi_key = \"{secret}\"\n"),
        );
        let err = load_with(home.path(), home.path(), &|_| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("must not contain API keys"), "{err}");
        assert!(!err.contains(secret), "{err}");

        write(
            &home.path().join("config.toml"),
            r#"
[[mcp.servers]]
name = "search"
transport = "http"
url = "https://mcp.example/search"
headers = { Authorization = "Bearer sk-literalvalue" }
"#,
        );
        let err = load_with(home.path(), home.path(), &|_| None)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("${VAR}") || err.contains("must not contain"),
            "{err}"
        );
        assert!(!err.contains("sk-literalvalue"), "{err}");
    }

    /// REQ-3.24 first-seen project config is confirmed once; content change asks again.
    #[test]
    fn req_3_24_first_seen_project_config_needs_confirm() {
        let home = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let path = proj.path().join(".keel/config.toml");
        let raw = "[[mcp.servers]]\nname = \"fs\"\ncommand = \"npx\"\n";
        write(&path, raw);
        assert!(needs_confirm(home.path(), &path, raw).unwrap());
        mark_trusted(home.path(), &path, raw).unwrap();
        assert!(!needs_confirm(home.path(), &path, raw).unwrap());
        let cfg = load_with(home.path(), proj.path(), &|_| None).unwrap();
        assert!(!cfg.needs_confirm);

        let changed = "[[mcp.servers]]\nname = \"other\"\ncommand = \"npx\"\n";
        write(&path, changed);
        assert!(needs_confirm(home.path(), &path, changed).unwrap());
        let cfg = load_with(home.path(), proj.path(), &|_| None).unwrap();
        assert!(cfg.needs_confirm);
        assert_eq!(cfg.servers[0].name, "other");

        mark_trusted(home.path(), &path, changed).unwrap();
        let cfg = load_with(home.path(), proj.path(), &|_| None).unwrap();
        assert!(!cfg.needs_confirm);
    }
}
