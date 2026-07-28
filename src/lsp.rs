use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::config::{self, DprintxConfig, ProfileResolution};
use crate::matcher::ProfileMatcher;

/// Timeout for reading LSP responses from backends.
/// Fallback when no proxy config is loaded.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Map LSP languageId to file extension (without dot).
/// Used to rewrite URIs so dprint can match files by extension
/// even when the original file has no extension or a different one.
fn language_ext(language_id: &str) -> Option<&'static str> {
    Some(match language_id {
        "go" => "go",
        "lua" => "lua",
        "json" => "json",
        "jsonc" => "jsonc",
        "yaml" => "yaml",
        "markdown" => "md",
        "python" => "py",
        "rust" => "rs",
        "typescript" => "ts",
        "typescriptreact" => "tsx",
        "javascript" => "js",
        "javascriptreact" => "jsx",
        "sh" | "bash" | "zsh" => "sh",
        "toml" => "toml",
        "css" => "css",
        "html" => "html",
        "sql" => "sql",
        "dockerfile" => "Dockerfile",
        "graphql" => "graphql",
        _ => return None,
    })
}

/// Rewrite a file URI to have the correct extension based on languageId.
/// If the file already has the right extension, returns None (no rewrite needed).
/// Otherwise appends `.{ext}` to the URI so dprint can match it.
fn rewrite_uri(uri: &str, language_id: &str) -> Option<String> {
    let ext = language_ext(language_id)?;
    let path = uri_to_path(uri);

    // Check if file already has the correct extension.
    if let Some(file_ext) = path.extension()
        && file_ext.to_str() == Some(ext)
    {
        return None; // Already correct.
    }

    // Append extension to URI: file:///path/to/some -> file:///path/to/some.sh
    Some(format!("{uri}.{ext}"))
}

/// Apply URI rewriting to an LSP message based on the language map.
/// Modifies params.textDocument.uri in-place if a rewrite is needed.
fn apply_uri_rewrite(msg: &mut serde_json::Value, uri_languages: &HashMap<String, String>) {
    let uri = match msg
        .get("params")
        .and_then(|p| p.get("textDocument"))
        .and_then(|td| td.get("uri"))
        .and_then(|u| u.as_str())
    {
        Some(u) => u.to_string(),
        None => return,
    };

    if let Some(lang_id) = uri_languages.get(&uri)
        && let Some(new_uri) = rewrite_uri(&uri, lang_id)
    {
        msg["params"]["textDocument"]["uri"] = serde_json::Value::String(new_uri);
    }
}

/// LSP proxy: spawns dprint lsp per profile, routes requests by file URI.
pub struct LspProxy {
    dprint_bin: PathBuf,
    matcher: ProfileMatcher,
    config: DprintxConfig,
}

/// A running dprint lsp backend.
struct Backend {
    _child: Child,
    stdin: std::process::ChildStdin,
    responses: mpsc::Receiver<String>,
}

impl LspProxy {
    pub fn new(dprint_bin: PathBuf, matcher: ProfileMatcher, config: DprintxConfig) -> Self {
        Self {
            dprint_bin,
            matcher,
            config,
        }
    }

    /// Run the LSP proxy. Blocks forever (until stdin closes).
    pub fn run(&self) -> Result<()> {
        eprintln!(
            "dprintx: lsp proxy starting (timeout={}ms)",
            self.read_timeout().as_millis()
        );

        // Map: profile config path -> backend.
        let backends: Arc<Mutex<HashMap<PathBuf, Backend>>> = Arc::new(Mutex::new(HashMap::new()));

        // Shared stdout lock for writing responses.
        let stdout = Arc::new(Mutex::new(io::stdout()));

        let stdin = io::stdin();
        let mut reader = BufReader::new(stdin.lock());

        // Track initialize state for lazy backend spawning.
        let mut _initialized = false;
        let mut last_init_params: Option<serde_json::Value> = None;
        // Merged configs, keyed by the directory whose local config was merged.
        // Every message carrying a URI would otherwise write a fresh temp file
        // and spawn a backend for it, since each file gets a unique name.
        let mut merged_configs: HashMap<PathBuf, config::TempConfig> = HashMap::new();
        // Track URI -> languageId from textDocument/didOpen for URI rewriting.
        let mut uri_languages: HashMap<String, String> = HashMap::new();
        // Open documents, kept so a backend spawned later still learns about
        // them: a backend that never saw didOpen has no text to format.
        let mut open_docs: HashMap<String, serde_json::Value> = HashMap::new();
        let rewrite_uris = self.config.lsp_rewrite_uris;

        loop {
            // Read LSP message (Content-Length header + body).
            let msg = match read_lsp_message(&mut reader) {
                Ok(msg) => msg,
                Err(_) => break, // EOF or error, exit.
            };

            // Parse as JSON.
            let parsed: serde_json::Value = match serde_json::from_str(&msg) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let method = parsed.get("method").and_then(|m| m.as_str());

            match method {
                Some("initialize") => {
                    // Start all backends with initialize.
                    let id = parsed.get("id").cloned();
                    let params = parsed.get("params").cloned();
                    last_init_params = params.clone();

                    // Spawn backends for each unique profile.
                    let mut profile_configs = Vec::new();
                    let mut seen = std::collections::HashSet::new();
                    for (_pattern, profile_name) in self.config.match_rules_iter() {
                        if seen.insert(profile_name.to_string())
                            && let Some(ProfileResolution::Config(config_path)) =
                                self.config.resolve_profile(profile_name)
                        {
                            profile_configs.push(config_path);
                        }
                    }

                    // Spawn all backends.
                    let mut first_response = None;
                    for config_path in &profile_configs {
                        let backend = self.spawn_backend(config_path)?;
                        let mut backends_lock = backends.lock().unwrap();
                        backends_lock.insert(config_path.clone(), backend);
                        drop(backends_lock);

                        // Send initialize to this backend. A profile config lives in
                        // ~/.config/dprint and says nothing about where the code is,
                        // so pass the editor's own workspace through untouched.
                        let init_params = params.clone().unwrap_or(serde_json::json!({}));
                        let init_msg = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "method": "initialize",
                            "params": init_params,
                        });

                        self.send_to_backend(&backends, config_path, &init_msg)?;

                        // Read response from this backend.
                        if let Ok(resp) = self.read_from_backend(&backends, config_path, &stdout)
                            && first_response.is_none()
                        {
                            first_response = Some(resp);
                        }
                    }

                    // Send first backend's response as our response.
                    if let Some(resp) = first_response {
                        write_lsp_message(&stdout, &resp)?;
                    }

                    _initialized = true;
                }

                Some("initialized") => {
                    // Forward to all backends.
                    let backends_lock = backends.lock().unwrap();
                    let keys: Vec<PathBuf> = backends_lock.keys().cloned().collect();
                    drop(backends_lock);

                    for config_path in &keys {
                        let _ = self.send_to_backend(&backends, config_path, &parsed);
                    }
                }

                Some("shutdown") => {
                    // Forward to all backends.
                    let backends_lock = backends.lock().unwrap();
                    let keys: Vec<PathBuf> = backends_lock.keys().cloned().collect();
                    drop(backends_lock);

                    for config_path in &keys {
                        let _ = self.send_to_backend(&backends, config_path, &parsed);
                        // Read response (with timeout).
                        let _ = self.read_from_backend(&backends, config_path, &stdout);
                    }

                    // Respond with null result.
                    let response = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": parsed.get("id"),
                        "result": null,
                    });
                    write_lsp_message(&stdout, &serde_json::to_string(&response)?)?;
                }

                Some("exit") => {
                    // Forward to all backends and exit.
                    let backends_lock = backends.lock().unwrap();
                    let keys: Vec<PathBuf> = backends_lock.keys().cloned().collect();
                    drop(backends_lock);

                    for config_path in &keys {
                        let _ = self.send_to_backend(&backends, config_path, &parsed);
                    }
                    break;
                }

                Some(method) if method.starts_with("textDocument/") => {
                    let method_name = method.to_string();
                    let has_id = parsed.get("id").is_some();
                    eprintln!(
                        "dprintx: recv {} ({})",
                        method_name,
                        if has_id { "request" } else { "notification" }
                    );

                    // Track languageId from didOpen, clean up on didClose.
                    if method_name == "textDocument/didOpen" {
                        if let Some(td) = parsed.get("params").and_then(|p| p.get("textDocument"))
                            && let (Some(uri), Some(lang_id)) = (
                                td.get("uri").and_then(|u| u.as_str()),
                                td.get("languageId").and_then(|l| l.as_str()),
                            )
                        {
                            if rewrite_uris {
                                eprintln!("dprintx: track {uri} as {lang_id}");
                            }
                            uri_languages.insert(uri.to_string(), lang_id.to_string());
                        }
                    } else if method_name == "textDocument/didClose"
                        && let Some(uri) = extract_uri(&parsed)
                    {
                        uri_languages.remove(&uri);
                        open_docs.remove(&uri);
                    }

                    // Clone and optionally rewrite URI based on languageId.
                    let mut msg = parsed.clone();
                    let original_uri = extract_uri(&parsed);
                    if rewrite_uris {
                        apply_uri_rewrite(&mut msg, &uri_languages);
                    }
                    // Use rewritten URI for routing, fall back to original.
                    let uri = extract_uri(&msg).or(original_uri);

                    // Store the rewritten form: that is what a backend would
                    // have received had it existed at the time.
                    if method_name == "textDocument/didOpen"
                        && let Some(uri) = &uri
                    {
                        open_docs.insert(uri.clone(), msg.clone());
                    }

                    if let Some(uri) = uri {
                        let file_path = uri_to_path(&uri);
                        let profile_config =
                            match self.matcher.resolve_config(&file_path, &self.config) {
                                Ok(Some(ProfileResolution::Config(p))) => p,
                                _ => {
                                    // No profile matched — respond with null result if it's a request.
                                    if let Some(id) = parsed.get("id").cloned() {
                                        let null_resp = serde_json::json!({
                                            "jsonrpc": "2.0",
                                            "id": id,
                                            "result": null,
                                        });
                                        write_lsp_message(
                                            &stdout,
                                            &serde_json::to_string(&null_resp)?,
                                        )?;
                                    }
                                    continue;
                                }
                            };

                        // Resolve effective config (merged local + profile, or just profile).
                        // `workspace_root` is the directory the backend should treat as its
                        // root: the project owning the local config, or whatever the editor
                        // opened. Never the config's own directory -- a profile lives in
                        // ~/.config/dprint and a merged config in a temp dir, and neither
                        // contains the files being formatted.
                        let mut workspace_root = None;
                        let effective_config = match file_path.parent() {
                            Some(parent) => match config::find_local_config(parent) {
                                Some(local) if local != profile_config => {
                                    let local_dir = local.parent().unwrap_or(parent).to_path_buf();
                                    workspace_root = Some(local_dir.clone());
                                    match merged_configs.entry(local_dir) {
                                        Entry::Occupied(e) => e.get().path().to_path_buf(),
                                        Entry::Vacant(e) => {
                                            match config::build_merged_config(
                                                parent,
                                                &profile_config,
                                            ) {
                                                Ok(Some(tc)) => e.insert(tc).path().to_path_buf(),
                                                Ok(None) => profile_config,
                                                Err(err) => {
                                                    eprintln!(
                                                        "dprintx: warning: build_merged_config failed: {err}"
                                                    );
                                                    profile_config
                                                }
                                            }
                                        }
                                    }
                                }
                                _ => profile_config,
                            },
                            None => profile_config,
                        };

                        // Ensure backend is spawned (lazily for merged configs).
                        {
                            let backends_lock = backends.lock().unwrap();
                            if !backends_lock.contains_key(&effective_config) {
                                drop(backends_lock);
                                let backend = self.spawn_backend(&effective_config)?;
                                let mut backends_lock = backends.lock().unwrap();
                                backends_lock.insert(effective_config.clone(), backend);
                                drop(backends_lock);

                                // Send initialize to the new backend.
                                if let Some(init_params) = &last_init_params {
                                    let mut params = init_params.clone();
                                    if let Some(root) = workspace_root.as_deref() {
                                        set_root(&mut params, root);
                                    }
                                    let init_msg = serde_json::json!({
                                        "jsonrpc": "2.0",
                                        "id": 1,
                                        "method": "initialize",
                                        "params": params,
                                    });
                                    let _ = self.send_to_backend(
                                        &backends,
                                        &effective_config,
                                        &init_msg,
                                    );
                                    // Read and discard initialize response.
                                    let _ = self.read_from_backend(
                                        &backends,
                                        &effective_config,
                                        &stdout,
                                    );

                                    // Send initialized notification.
                                    let initialized_msg = serde_json::json!({
                                        "jsonrpc": "2.0",
                                        "method": "initialized",
                                        "params": {},
                                    });
                                    let _ = self.send_to_backend(
                                        &backends,
                                        &effective_config,
                                        &initialized_msg,
                                    );

                                    // Replay open documents. This backend missed
                                    // every didOpen sent before it existed, and
                                    // formatting reads from that document store.
                                    for doc in open_docs.values() {
                                        let _ =
                                            self.send_to_backend(&backends, &effective_config, doc);
                                    }
                                }
                            }
                        }

                        // Send request to the right backend (with rewritten URI if enabled).
                        self.send_to_backend(&backends, &effective_config, &msg)?;

                        // If it's a request (has id), read response.
                        if let Some(id) = parsed.get("id").cloned() {
                            let t0 = std::time::Instant::now();
                            match self.read_from_backend_matching(
                                &backends,
                                &effective_config,
                                &stdout,
                                Some(&id),
                            ) {
                                Ok(resp) => {
                                    eprintln!(
                                        "dprintx: {} responded in {:?}",
                                        method_name,
                                        t0.elapsed()
                                    );
                                    write_lsp_message(&stdout, &resp)?;
                                }
                                Err(e) => {
                                    eprintln!(
                                        "dprintx: {} timeout/error in {:?}: {}",
                                        method_name,
                                        t0.elapsed(),
                                        e
                                    );
                                    let error_resp = serde_json::json!({
                                        "jsonrpc": "2.0",
                                        "id": id,
                                        "result": null,
                                    });
                                    write_lsp_message(
                                        &stdout,
                                        &serde_json::to_string(&error_resp)?,
                                    )?;
                                }
                            }
                        }
                    }
                }

                _ => {
                    // Unknown method — forward to all backends.
                    let backends_lock = backends.lock().unwrap();
                    let keys: Vec<PathBuf> = backends_lock.keys().cloned().collect();
                    drop(backends_lock);

                    for config_path in &keys {
                        let _ = self.send_to_backend(&backends, config_path, &parsed);
                    }

                    // If it's a request, respond from first backend.
                    if let Some(id) = parsed.get("id").cloned()
                        && let Some(config_path) = keys.first()
                    {
                        match self.read_from_backend(&backends, config_path, &stdout) {
                            Ok(resp) => {
                                write_lsp_message(&stdout, &resp)?;
                            }
                            Err(_) => {
                                let error_resp = serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": null,
                                });
                                write_lsp_message(&stdout, &serde_json::to_string(&error_resp)?)?;
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Whether the backend understands `lsp --no-gitignore`. Older dprint
    /// versions reject the flag outright, so ask before using it.
    fn backend_takes_no_gitignore(&self) -> bool {
        static SUPPORTED: OnceLock<bool> = OnceLock::new();
        *SUPPORTED.get_or_init(|| {
            Command::new(&self.dprint_bin)
                .args(["lsp", "--help"])
                .output()
                // dprint prints subcommand help on stderr, so check both.
                .map(|out| {
                    let help = String::from_utf8_lossy(&out.stdout).into_owned()
                        + &String::from_utf8_lossy(&out.stderr);
                    help.contains("--no-gitignore")
                })
                .unwrap_or(false)
        })
    }

    fn spawn_backend(&self, config_path: &PathBuf) -> Result<Backend> {
        let mut cmd = Command::new(&self.dprint_bin);
        cmd.arg("lsp");
        if self.config.lsp_no_gitignore && self.backend_takes_no_gitignore() {
            cmd.arg("--no-gitignore");
        }
        let mut child = cmd
            .arg("--config")
            .arg(config_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawning dprint lsp --config {}", config_path.display()))?;

        let stdin = child.stdin.take().context("no stdin on dprint lsp")?;
        let child_stdout = child.stdout.take().context("no stdout on dprint lsp")?;

        // Spawn a reader thread that reads LSP messages and sends them via channel.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(child_stdout);
            while let Ok(msg) = read_lsp_message(&mut reader) {
                if tx.send(msg).is_err() {
                    break; // Receiver dropped.
                }
            }
        });

        Ok(Backend {
            _child: child,
            stdin,
            responses: rx,
        })
    }

    fn send_to_backend(
        &self,
        backends: &Arc<Mutex<HashMap<PathBuf, Backend>>>,
        config_path: &PathBuf,
        msg: &serde_json::Value,
    ) -> Result<()> {
        let json = serde_json::to_string(msg)?;
        let mut backends_lock = backends.lock().unwrap();
        if let Some(backend) = backends_lock.get_mut(config_path) {
            let header = format!("Content-Length: {}\r\n\r\n", json.len());
            backend.stdin.write_all(header.as_bytes())?;
            backend.stdin.write_all(json.as_bytes())?;
            backend.stdin.flush()?;
        }
        Ok(())
    }

    fn read_timeout(&self) -> Duration {
        match self.config.lsp_timeout_ms {
            0 => READ_TIMEOUT,
            ms => Duration::from_millis(ms),
        }
    }

    /// Read the response to `expect_id` from a backend.
    ///
    /// Notifications are forwarded to the editor as they arrive. So are replies
    /// to other requests: a backend advertising hover and completion answers
    /// several requests at once, and returning whichever reply lands first would
    /// hand the editor an answer to a question it asked about something else.
    /// `None` keeps the old behaviour of taking the first reply, for call sites
    /// that discard the result anyway.
    fn read_from_backend_matching(
        &self,
        backends: &Arc<Mutex<HashMap<PathBuf, Backend>>>,
        config_path: &PathBuf,
        stdout: &Arc<Mutex<io::Stdout>>,
        expect_id: Option<&serde_json::Value>,
    ) -> Result<String> {
        let backends_lock = backends.lock().unwrap();
        let backend = backends_lock
            .get(config_path)
            .context("backend not found")?;

        let deadline = std::time::Instant::now() + self.read_timeout();
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                bail!("backend read timeout");
            }

            let msg = backend
                .responses
                .recv_timeout(remaining)
                .context("backend read timeout")?;

            // Check if this is a response (has "id") or a notification.
            let parsed: serde_json::Value = serde_json::from_str(&msg)?;
            if let Some(id) = parsed.get("id") {
                match expect_id {
                    Some(expected) if id != expected => {
                        // Someone else's reply: pass it through and keep waiting.
                        let _ = write_lsp_message(stdout, &msg);
                        continue;
                    }
                    _ => return Ok(msg),
                }
            }

            // It's a notification — forward to editor and keep waiting.
            let _ = write_lsp_message(stdout, &msg);
        }
    }

    /// Read the next response from a backend, whatever request it belongs to.
    fn read_from_backend(
        &self,
        backends: &Arc<Mutex<HashMap<PathBuf, Backend>>>,
        config_path: &PathBuf,
        stdout: &Arc<Mutex<io::Stdout>>,
    ) -> Result<String> {
        self.read_from_backend_matching(backends, config_path, stdout, None)
    }
}

/// Read an LSP message from a buffered reader.
/// Format: "Content-Length: N\r\n\r\n" followed by N bytes.
fn read_lsp_message<R: BufRead>(reader: &mut R) -> Result<String> {
    let mut content_length: Option<usize> = None;

    // Read headers.
    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            bail!("EOF while reading LSP headers");
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            break; // End of headers.
        }

        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            content_length = Some(rest.trim().parse().context("invalid Content-Length")?);
        }
    }

    let length = content_length.context("missing Content-Length header")?;

    // Read body.
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;

    String::from_utf8(body).context("invalid UTF-8 in LSP message body")
}

/// Write an LSP message to stdout.
fn write_lsp_message(stdout: &Arc<Mutex<io::Stdout>>, body: &str) -> Result<()> {
    let mut out = stdout.lock().unwrap();
    write!(out, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
    out.flush()?;
    Ok(())
}

/// Extract file URI from LSP params.
/// Looks for params.textDocument.uri.
/// Point `initialize` params at the directory a backend should call its root.
///
/// Each backend serves exactly one config, so it gets exactly one folder --
/// dprintx is what decides which config covers which directory. All three
/// fields are set because a client only reads the newest one it understands,
/// and dprint prefers `workspaceFolders` over the deprecated `rootUri`.
fn set_root(params: &mut serde_json::Value, root: &Path) {
    let uri = format!("file://{}", root.display());
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| uri.clone());
    params["workspaceFolders"] = serde_json::json!([{ "uri": uri, "name": name }]);
    params["rootUri"] = serde_json::Value::String(uri);
    params["rootPath"] = serde_json::Value::String(root.display().to_string());
}

fn extract_uri(msg: &serde_json::Value) -> Option<String> {
    msg.get("params")?
        .get("textDocument")?
        .get("uri")?
        .as_str()
        .map(|s| s.to_string())
}

/// Convert file:// URI to a filesystem path.
fn uri_to_path(uri: &str) -> PathBuf {
    if let Some(path) = uri.strip_prefix("file://") {
        // URL-decode percent-encoded characters.
        let decoded = percent_decode(path);
        PathBuf::from(decoded)
    } else {
        PathBuf::from(uri)
    }
}

/// Simple percent-decoding for file URIs.
fn percent_decode(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&input[i + 1..i + 3], 16)
        {
            result.push(byte as char);
            i += 3;
            continue;
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uri_to_path() {
        assert_eq!(
            uri_to_path("file:///home/user/file.go"),
            PathBuf::from("/home/user/file.go")
        );
        assert_eq!(
            uri_to_path("file:///home/user/my%20file.go"),
            PathBuf::from("/home/user/my file.go")
        );
    }

    #[test]
    fn test_extract_uri() {
        let msg = serde_json::json!({
            "method": "textDocument/formatting",
            "params": {
                "textDocument": {
                    "uri": "file:///home/user/file.go"
                }
            }
        });
        assert_eq!(
            extract_uri(&msg),
            Some("file:///home/user/file.go".to_string())
        );
    }

    #[test]
    fn test_extract_uri_missing() {
        let msg = serde_json::json!({
            "method": "shutdown"
        });
        assert_eq!(extract_uri(&msg), None);
    }

    #[test]
    fn test_language_ext() {
        assert_eq!(language_ext("go"), Some("go"));
        assert_eq!(language_ext("lua"), Some("lua"));
        assert_eq!(language_ext("sh"), Some("sh"));
        assert_eq!(language_ext("bash"), Some("sh"));
        assert_eq!(language_ext("zsh"), Some("sh"));
        assert_eq!(language_ext("markdown"), Some("md"));
        assert_eq!(language_ext("python"), Some("py"));
        assert_eq!(language_ext("rust"), Some("rs"));
        assert_eq!(language_ext("unknown_lang_xyz"), None);
    }

    #[test]
    fn test_rewrite_uri_no_extension() {
        // File without extension + known languageId → append .sh
        assert_eq!(
            rewrite_uri("file:///home/user/myscript", "sh"),
            Some("file:///home/user/myscript.sh".to_string())
        );
    }

    #[test]
    fn test_rewrite_uri_correct_extension() {
        // File already has correct extension → no rewrite
        assert_eq!(rewrite_uri("file:///home/user/main.go", "go"), None);
    }

    #[test]
    fn test_rewrite_uri_wrong_extension() {
        // File has .py extension but editor says sh → rewrite
        assert_eq!(
            rewrite_uri("file:///home/user/script.py", "sh"),
            Some("file:///home/user/script.py.sh".to_string())
        );
    }

    #[test]
    fn test_rewrite_uri_unknown_language() {
        // Unknown languageId → no rewrite
        assert_eq!(
            rewrite_uri("file:///home/user/file.xyz", "unknown_lang"),
            None
        );
    }

    #[test]
    fn test_apply_uri_rewrite() {
        let mut uri_languages = HashMap::new();
        uri_languages.insert("file:///home/user/myscript".to_string(), "sh".to_string());

        let mut msg = serde_json::json!({
            "method": "textDocument/formatting",
            "params": {
                "textDocument": {
                    "uri": "file:///home/user/myscript"
                }
            }
        });

        apply_uri_rewrite(&mut msg, &uri_languages);
        assert_eq!(
            msg["params"]["textDocument"]["uri"],
            "file:///home/user/myscript.sh"
        );
    }

    #[test]
    fn test_set_root_replaces_editor_workspace() {
        // The editor opened one project; this backend serves another.
        let mut params = serde_json::json!({
            "rootUri": "file:///home/user/other",
            "rootPath": "/home/user/other",
            "workspaceFolders": [{ "uri": "file:///home/user/other", "name": "other" }],
            "capabilities": {},
        });

        set_root(&mut params, Path::new("/home/user/project"));

        assert_eq!(params["rootUri"], "file:///home/user/project");
        assert_eq!(params["rootPath"], "/home/user/project");
        assert_eq!(
            params["workspaceFolders"],
            serde_json::json!([{ "uri": "file:///home/user/project", "name": "project" }])
        );
        // Untouched fields survive.
        assert_eq!(params["capabilities"], serde_json::json!({}));
    }

    #[test]
    fn test_set_root_single_folder_only() {
        // A backend serves exactly one config, so it must not inherit a list of
        // folders the editor happened to have open.
        let mut params = serde_json::json!({
            "workspaceFolders": [
                { "uri": "file:///a", "name": "a" },
                { "uri": "file:///b", "name": "b" },
            ],
        });

        set_root(&mut params, Path::new("/home/user/project"));

        assert_eq!(params["workspaceFolders"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_apply_uri_rewrite_no_match() {
        let uri_languages = HashMap::new(); // empty map

        let mut msg = serde_json::json!({
            "method": "textDocument/formatting",
            "params": {
                "textDocument": {
                    "uri": "file:///home/user/myscript"
                }
            }
        });

        let original = msg.clone();
        apply_uri_rewrite(&mut msg, &uri_languages);
        // No rewrite — message unchanged.
        assert_eq!(msg, original);
    }

    #[test]
    fn test_apply_uri_rewrite_already_correct() {
        let mut uri_languages = HashMap::new();
        uri_languages.insert("file:///home/user/main.go".to_string(), "go".to_string());

        let mut msg = serde_json::json!({
            "method": "textDocument/formatting",
            "params": {
                "textDocument": {
                    "uri": "file:///home/user/main.go"
                }
            }
        });

        let original = msg.clone();
        apply_uri_rewrite(&mut msg, &uri_languages);
        // Already correct extension — no rewrite.
        assert_eq!(msg, original);
    }
}
