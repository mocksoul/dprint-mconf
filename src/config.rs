use anyhow::{Context, Result};
use regex::{RegexSet, RegexSetBuilder};
use serde::Deserialize;
use serde_json::Map;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Result of resolving a profile name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileResolution {
    /// Profile maps to a config file path.
    Config(PathBuf),
    /// Profile is explicitly set to null (ignore/skip file).
    Ignore,
}

/// Counter for generating unique temp file names within a process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// RAII guard for a temporary merged config file. Deletes the file on drop.
pub struct TempConfig {
    path: PathBuf,
}

impl TempConfig {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// dprintx.jsonc configuration.
///
/// Format:
/// ```jsonc
/// {
///   "dprint": "~/.cargo/bin/dprint",
///   "profiles": {
///     "maintainer": "~/.config/dprint/dprint-maintainer.jsonc",
///     "default": "~/.config/dprint/dprint-default.jsonc",
///     "ignore": null,
///   },
///   "match": {
///     "**/noc/cmdb/**": "maintainer",
///     "**/noc/invapi/**": "maintainer",
///     "**": "default",
///   },
///   "match_content": {
///     "^// Code generated .+ DO NOT EDIT\\.$": "ignore",
///   },
/// }
/// ```
#[derive(Debug, Deserialize)]
pub struct DprintxConfig {
    /// Directory containing the dprintx.jsonc config file.
    /// Used to resolve relative paths in profile configs.
    /// Populated after loading, not deserialized from JSON.
    #[serde(skip)]
    pub config_dir: PathBuf,

    /// Path to real dprint binary.
    pub dprint: String,

    /// Named profiles: name → config path (string) or null (ignore).
    pub profiles: Map<String, serde_json::Value>,

    /// Ordered match rules: glob pattern → profile name.
    /// Uses serde_json::Map with preserve_order for first-match semantics.
    #[serde(rename = "match")]
    pub match_rules: Map<String, serde_json::Value>,

    /// Ordered content match rules: regex pattern → profile name.
    /// Applied after path match. Scans entire file in line-aligned blocks.
    /// First match wins and overrides the path-matched profile.
    #[serde(default)]
    pub match_content: Option<Map<String, serde_json::Value>>,

    /// Optional diff pager command for `dprint check` (e.g. "delta -s").
    /// When set, check produces unified diff output:
    /// - stdout is TTY → pipe through pager
    /// - stdout is pipe/redirect → raw unified diff
    #[serde(default)]
    pub diff_pager: Option<String>,

    /// Rewrite file URIs in LSP based on editor's languageId.
    /// When true, the proxy appends the correct file extension to URIs
    /// forwarded to dprint, so files without extensions (or with wrong ones)
    /// get formatted according to the editor's filetype detection.
    /// Default: false (transparent passthrough).
    #[serde(default)]
    pub lsp_rewrite_uris: bool,

    /// How long to wait for a backend reply, in milliseconds.
    /// The first request against a config compiles its wasm plugins, which
    /// takes seconds, so a short timeout turns a cold start into a silent
    /// "no edits" answer.
    #[serde(default = "default_lsp_timeout_ms")]
    pub lsp_timeout_ms: u64,
}

fn default_lsp_timeout_ms() -> u64 {
    30_000
}

impl DprintxConfig {
    /// Try to load config from the default location (~/.config/dprint/dprintx.jsonc).
    /// Returns Ok(None) if the file doesn't exist.
    /// Returns Err if the file exists but is invalid.
    pub fn try_load_default() -> Result<Option<Self>> {
        let config_dir = dirs::config_dir().context("cannot determine config directory")?;
        let path = config_dir.join("dprint").join("dprintx.jsonc");
        if !path.exists() {
            return Ok(None);
        }
        Self::load(&path).map(Some)
    }

    /// Load config from a specific path.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config: {}", path.display()))?;

        // Strip JSONC comments (// and /* */) before parsing.
        let json = strip_jsonc_comments(&content);

        let mut config: DprintxConfig =
            serde_json::from_str(&json).with_context(|| "invalid dprintx.jsonc format")?;

        // Store the config directory for resolving relative paths.
        config.config_dir = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();

        Ok(config)
    }

    /// Resolve dprint binary path (expand ~ and relative paths).
    pub fn dprint_path(&self) -> PathBuf {
        self.resolve_path(&self.dprint)
    }

    /// Resolve a path string: expand ~ and resolve relative paths against config_dir.
    fn resolve_path(&self, path: &str) -> PathBuf {
        let expanded = expand_tilde(path);
        if expanded.is_relative() {
            self.config_dir.join(expanded)
        } else {
            expanded
        }
    }

    /// Resolve a profile name to its resolution (config path or ignore).
    ///
    /// Returns:
    /// - `Some(Config(path))` if profile maps to a config file path
    /// - `Some(Ignore)` if profile is explicitly null
    /// - `None` if profile name is not defined
    ///
    /// Relative paths are resolved against the config file directory.
    pub fn resolve_profile(&self, profile_name: &str) -> Option<ProfileResolution> {
        match self.profiles.get(profile_name) {
            Some(serde_json::Value::String(s)) => {
                Some(ProfileResolution::Config(self.resolve_path(s)))
            }
            Some(serde_json::Value::Null) => Some(ProfileResolution::Ignore),
            _ => None,
        }
    }

    /// Get ordered match rules as (glob_pattern, profile_name) pairs.
    pub fn match_rules_iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.match_rules.iter().filter_map(|(pattern, value)| {
            value.as_str().map(|profile| (pattern.as_str(), profile))
        })
    }

    /// Get ordered content match rules as (regex_pattern, profile_name) pairs.
    /// Returns empty iterator if match_content is not configured.
    pub fn match_content_rules_iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.match_content
            .iter()
            .flat_map(|m| m.iter())
            .filter_map(|(pattern, value)| {
                value.as_str().map(|profile| (pattern.as_str(), profile))
            })
    }

    /// Compile content match patterns into a RegexSet for efficient matching.
    /// Returns None if no match_content rules are configured.
    pub fn compile_content_patterns(&self) -> Result<Option<ContentMatcher>> {
        let rules: Vec<(&str, &str)> = self.match_content_rules_iter().collect();
        if rules.is_empty() {
            return Ok(None);
        }

        let patterns: Vec<&str> = rules.iter().map(|(p, _)| *p).collect();
        let profiles: Vec<String> = rules.iter().map(|(_, p)| p.to_string()).collect();

        let regex_set = RegexSetBuilder::new(&patterns)
            .multi_line(true)
            .build()
            .context("invalid regex in match_content")?;

        Ok(Some(ContentMatcher {
            regex_set,
            profiles,
        }))
    }
}

/// Compiled content match rules for efficient file content matching.
/// Uses multi-line mode: `^` matches at start of any line, `$` at end of any line.
#[derive(Debug)]
pub struct ContentMatcher {
    /// Compiled regex set (multi-line mode) for matching file content.
    regex_set: RegexSet,
    /// Profile names corresponding to each regex pattern (same order).
    profiles: Vec<String>,
}

impl ContentMatcher {
    /// Match file content against compiled patterns.
    /// Returns the profile name of the first matching pattern, or None.
    pub fn match_content(&self, content: &str) -> Option<&str> {
        // RegexSet::matches returns all matches; we want first-match semantics
        // based on config order, so take the minimum index.
        self.regex_set
            .matches(content)
            .iter()
            .next()
            .map(|idx| self.profiles[idx].as_str())
    }
}

/// Find a local dprint config by walking up from the given directory.
/// Looks for `dprint.json` and `dprint.jsonc` in each directory up to the root.
pub fn find_local_config(start_dir: &Path) -> Option<PathBuf> {
    let mut dir = start_dir;
    loop {
        for name in &["dprint.json", "dprint.jsonc"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return None,
        }
    }
}

/// Read a local dprint config file as a JSON Value.
/// Handles both .json and .jsonc (strips comments and trailing commas).
pub fn read_local_config(path: &Path) -> Result<serde_json::Value> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    // Strip JSONC comments if needed (safe to run on plain JSON too).
    let json = strip_jsonc_comments(&content);

    serde_json::from_str(&json)
        .with_context(|| format!("parsing local dprint config: {}", path.display()))
}

/// Inject a profile config path into the `extends` field of a local dprint config.
///
/// The profile path is prepended to extends so that local config settings
/// take precedence (dprint applies extends first, then local overrides on top).
///
/// Handles three cases:
/// - `extends` absent → set to the profile path string
/// - `extends` is a string → convert to array [profile_path, old_string]
/// - `extends` is an array → prepend profile_path
pub fn inject_extends(config: &mut serde_json::Value, profile_config_path: &Path) {
    let path_str = profile_config_path.display().to_string();
    let obj = match config.as_object_mut() {
        Some(obj) => obj,
        None => return,
    };

    match obj.get("extends") {
        None => {
            obj.insert("extends".to_string(), serde_json::Value::String(path_str));
        }
        Some(serde_json::Value::String(_)) => {
            let old = obj.remove("extends").unwrap();
            let arr = serde_json::Value::Array(vec![serde_json::Value::String(path_str), old]);
            obj.insert("extends".to_string(), arr);
        }
        Some(serde_json::Value::Array(_)) => {
            if let Some(serde_json::Value::Array(arr)) = obj.get_mut("extends") {
                arr.insert(0, serde_json::Value::String(path_str));
            }
        }
        _ => {
            // Unexpected type — overwrite with profile path.
            obj.insert("extends".to_string(), serde_json::Value::String(path_str));
        }
    }
}

/// Build a merged config for a file: find local dprint config, inject profile
/// extends, write to a unique temp file.
///
/// Returns None if no local config is found (caller should use profile config directly).
/// Returns a `TempConfig` guard that auto-deletes the file on drop.
///
/// The temp file is written to `$XDG_RUNTIME_DIR/dprintx/` (per-user, secure).
/// Falls back to `$TMPDIR/dprintx/` if unavailable.
pub fn build_merged_config(
    file_dir: &Path,
    profile_config_path: &Path,
) -> Result<Option<TempConfig>> {
    let local_config_path = match find_local_config(file_dir) {
        Some(p) => p,
        None => return Ok(None),
    };

    // If the local config IS the profile config, skip merging.
    if local_config_path == profile_config_path {
        return Ok(None);
    }

    let mut local_config = read_local_config(&local_config_path)?;
    inject_extends(&mut local_config, profile_config_path);

    // Write to a per-user runtime dir with a unique name.
    let cache_dir = merged_config_dir()?;
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let temp_path = cache_dir.join(format!("merged-{pid}-{seq}.json"));

    let json = serde_json::to_string_pretty(&local_config).context("serializing merged config")?;
    std::fs::write(&temp_path, json)
        .with_context(|| format!("writing merged config to {}", temp_path.display()))?;

    Ok(Some(TempConfig { path: temp_path }))
}

/// Get the directory for merged config temp files.
/// Prefers $XDG_RUNTIME_DIR/dprintx/ (per-user tmpfs, mode 700).
/// Falls back to $TMPDIR/dprintx/.
fn merged_config_dir() -> Result<PathBuf> {
    let dir = match dirs::runtime_dir() {
        Some(runtime) => runtime.join("dprintx"),
        None => std::env::temp_dir().join("dprintx"),
    };

    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating merged config dir: {}", dir.display()))?;

    clean_orphaned_configs(&dir);

    Ok(dir)
}

/// Remove merged configs left behind by processes that are gone.
///
/// The `TempConfig` guard deletes the file on drop, but a killed process never
/// drops anything, so the directory accumulates files across crashes. The pid
/// in the name says who owned it; if that process is gone, so is the need.
fn clean_orphaned_configs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let own_pid = std::process::id();

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix("merged-") else {
            continue;
        };
        let Some((pid, _)) = rest.split_once('-') else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };

        if pid != own_pid && !process_exists(pid) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(unix))]
fn process_exists(_pid: u32) -> bool {
    // Without a cheap liveness check, keep the file: leaking beats deleting a
    // config another process is still using.
    true
}

/// Expand ~ to home directory in a path string.
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// Strip JSONC-style comments from a string.
/// Handles // line comments and /* */ block comments.
/// Does not strip inside strings.
fn strip_jsonc_comments(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut in_string = false;

    while i < len {
        if in_string {
            result.push(chars[i]);
            if chars[i] == '\\' && i + 1 < len {
                i += 1;
                result.push(chars[i]);
            } else if chars[i] == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        if chars[i] == '"' {
            in_string = true;
            result.push(chars[i]);
            i += 1;
            continue;
        }

        // Line comment
        if chars[i] == '/' && i + 1 < len && chars[i + 1] == '/' {
            // Skip until end of line
            i += 2;
            while i < len && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }

        // Block comment
        if chars[i] == '/' && i + 1 < len && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < len && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            if i + 1 < len {
                i += 2; // skip */
            }
            continue;
        }

        result.push(chars[i]);
        i += 1;
    }

    // Strip trailing commas before } and ] (JSONC allows them, JSON doesn't).
    strip_trailing_commas(&result)
}

/// Remove trailing commas before } and ].
fn strip_trailing_commas(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut in_string = false;

    while i < len {
        if in_string {
            result.push(chars[i]);
            if chars[i] == '\\' && i + 1 < len {
                i += 1;
                result.push(chars[i]);
            } else if chars[i] == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        if chars[i] == '"' {
            in_string = true;
            result.push(chars[i]);
            i += 1;
            continue;
        }

        if chars[i] == ',' {
            // Look ahead for } or ] (skipping whitespace)
            let mut j = i + 1;
            while j < len && chars[j].is_whitespace() {
                j += 1;
            }
            if j < len && (chars[j] == '}' || chars[j] == ']') {
                // Skip the trailing comma
                i += 1;
                continue;
            }
        }

        result.push(chars[i]);
        i += 1;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_jsonc_comments() {
        let input = r#"{
  // line comment
  "key": "value", // inline comment
  /* block comment */
  "key2": "val with // not a comment"
}"#;
        let result = strip_jsonc_comments(input);
        assert!(result.contains("\"key\": \"value\""));
        assert!(result.contains("\"key2\": \"val with // not a comment\""));
        assert!(!result.contains("line comment"));
        assert!(!result.contains("inline comment"));
        assert!(!result.contains("block comment"));
    }

    #[test]
    fn test_strip_trailing_commas() {
        let input = r#"{"a": 1, "b": 2,}"#;
        let result = strip_trailing_commas(input);
        assert_eq!(result, r#"{"a": 1, "b": 2}"#);
    }

    #[test]
    fn test_expand_tilde() {
        let home = dirs::home_dir().unwrap();
        let result = expand_tilde("~/foo/bar");
        assert_eq!(result, home.join("foo/bar"));

        let result = expand_tilde("/absolute/path");
        assert_eq!(result, PathBuf::from("/absolute/path"));
    }

    #[test]
    fn test_parse_full_dprintx_jsonc() {
        let input = r#"{
  // Path to real dprint binary
  "dprint": "~/.cargo/bin/dprint",
  "profiles": {
    "maintainer": "~/.config/dprint/dprint-maintainer.jsonc",
    "default": "~/.config/dprint/dprint-default.jsonc",
  },
  "match": {
    "**/noc/cmdb/**": "maintainer",
    "**/noc/invapi/**": "maintainer",
    /* catch-all */
    "**": "default",
  },
}"#;
        let json = strip_jsonc_comments(input);
        let config: DprintxConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(config.dprint, "~/.cargo/bin/dprint");
        assert_eq!(config.profiles.len(), 2);

        // Verify match rules preserve order (first match semantics).
        let rules: Vec<(&str, &str)> = config.match_rules_iter().collect();
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0], ("**/noc/cmdb/**", "maintainer"));
        assert_eq!(rules[1], ("**/noc/invapi/**", "maintainer"));
        assert_eq!(rules[2], ("**", "default"));

        // No match_content by default.
        assert!(config.match_content.is_none());
    }

    #[test]
    fn test_resolve_profile_config() {
        let config_json = r#"{
            "dprint": "/usr/bin/dprint",
            "profiles": {
                "maintainer": "/config/dprint-maintainer.jsonc",
                "default": "/config/dprint-default.jsonc"
            },
            "match": { "**": "default" }
        }"#;
        let config: DprintxConfig = serde_json::from_str(config_json).unwrap();

        assert_eq!(
            config.resolve_profile("maintainer"),
            Some(ProfileResolution::Config(PathBuf::from(
                "/config/dprint-maintainer.jsonc"
            )))
        );
        assert_eq!(
            config.resolve_profile("default"),
            Some(ProfileResolution::Config(PathBuf::from(
                "/config/dprint-default.jsonc"
            )))
        );
        assert_eq!(config.resolve_profile("nonexistent"), None);
    }

    #[test]
    fn test_resolve_profile_null_ignore() {
        let config_json = r#"{
            "dprint": "/usr/bin/dprint",
            "profiles": {
                "default": "/config/dprint-default.jsonc",
                "ignore": null
            },
            "match": { "**": "default" }
        }"#;
        let config: DprintxConfig = serde_json::from_str(config_json).unwrap();

        assert_eq!(
            config.resolve_profile("ignore"),
            Some(ProfileResolution::Ignore)
        );
        assert_eq!(
            config.resolve_profile("default"),
            Some(ProfileResolution::Config(PathBuf::from(
                "/config/dprint-default.jsonc"
            )))
        );
    }

    #[test]
    fn test_resolve_profile_relative_paths() {
        let config_json = r#"{
            "dprint": "./bin/dprint",
            "profiles": {
                "maintainer": "./profiles/maintainer.jsonc",
                "default": "./profiles/default.jsonc",
                "absolute": "/etc/dprint/absolute.jsonc",
                "tilde": "~/configs/tilde.jsonc",
                "ignore": null
            },
            "match": { "**": "default" }
        }"#;
        let mut config: DprintxConfig = serde_json::from_str(config_json).unwrap();
        config.config_dir = PathBuf::from("/home/user/.config/dprint");

        // Relative paths resolved against config_dir.
        assert_eq!(
            config.resolve_profile("maintainer"),
            Some(ProfileResolution::Config(PathBuf::from(
                "/home/user/.config/dprint/profiles/maintainer.jsonc"
            )))
        );
        assert_eq!(
            config.resolve_profile("default"),
            Some(ProfileResolution::Config(PathBuf::from(
                "/home/user/.config/dprint/profiles/default.jsonc"
            )))
        );

        // Absolute paths stay as-is.
        assert_eq!(
            config.resolve_profile("absolute"),
            Some(ProfileResolution::Config(PathBuf::from(
                "/etc/dprint/absolute.jsonc"
            )))
        );

        // Tilde-expanded paths are absolute, stay as-is.
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            config.resolve_profile("tilde"),
            Some(ProfileResolution::Config(home.join("configs/tilde.jsonc")))
        );

        // Null profile stays Ignore.
        assert_eq!(
            config.resolve_profile("ignore"),
            Some(ProfileResolution::Ignore)
        );

        // dprint binary path also resolved.
        assert_eq!(
            config.dprint_path(),
            PathBuf::from("/home/user/.config/dprint/bin/dprint")
        );
    }

    #[test]
    fn test_load_sets_config_dir() {
        let dir = std::env::temp_dir().join("dprintx-test-load-dir");
        let _ = std::fs::create_dir_all(&dir);

        let config_path = dir.join("dprintx.jsonc");
        std::fs::write(
            &config_path,
            r#"{
                "dprint": "/usr/bin/dprint",
                "profiles": { "default": "/config/default.jsonc" },
                "match": { "**": "default" }
            }"#,
        )
        .unwrap();

        let config = DprintxConfig::load(&config_path).unwrap();
        assert_eq!(config.config_dir, dir);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_match_content() {
        let input = r#"{
            "dprint": "/usr/bin/dprint",
            "profiles": {
                "default": "/config/default.jsonc",
                "ignore": null
            },
            "match": { "**": "default" },
            "match_content": {
                "^// Code generated .+ DO NOT EDIT\\.$": "ignore",
                "^# Code generated .+ DO NOT EDIT\\.$": "ignore"
            }
        }"#;
        let config: DprintxConfig = serde_json::from_str(input).unwrap();

        assert!(config.match_content.is_some());
        let rules: Vec<(&str, &str)> = config.match_content_rules_iter().collect();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].1, "ignore");
        assert_eq!(rules[1].1, "ignore");
    }

    #[test]
    fn test_compile_content_patterns() {
        let config_json = r#"{
            "dprint": "/usr/bin/dprint",
            "profiles": {
                "default": "/config/default.jsonc",
                "ignore": null
            },
            "match": { "**": "default" },
            "match_content": {
                "^// Code generated .+ DO NOT EDIT\\.$": "ignore"
            }
        }"#;
        let config: DprintxConfig = serde_json::from_str(config_json).unwrap();

        let matcher = config.compile_content_patterns().unwrap();
        assert!(matcher.is_some());

        let matcher = matcher.unwrap();
        assert_eq!(
            matcher.match_content("// Code generated by protoc DO NOT EDIT."),
            Some("ignore")
        );
        assert_eq!(
            matcher.match_content("package main\n\nfunc main() {}"),
            None
        );
    }

    #[test]
    fn test_compile_content_patterns_none() {
        let config_json = r#"{
            "dprint": "/usr/bin/dprint",
            "profiles": { "default": "/config/default.jsonc" },
            "match": { "**": "default" }
        }"#;
        let config: DprintxConfig = serde_json::from_str(config_json).unwrap();

        let matcher = config.compile_content_patterns().unwrap();
        assert!(matcher.is_none());
    }

    #[test]
    fn test_content_matcher_empty_content() {
        let config_json = r#"{
            "dprint": "/usr/bin/dprint",
            "profiles": {
                "default": "/config/default.jsonc",
                "ignore": null
            },
            "match": { "**": "default" },
            "match_content": {
                "DO NOT EDIT": "ignore"
            }
        }"#;
        let config: DprintxConfig = serde_json::from_str(config_json).unwrap();
        let matcher = config.compile_content_patterns().unwrap().unwrap();

        assert_eq!(matcher.match_content(""), None);
    }

    #[test]
    fn test_content_matcher_multiline() {
        let config_json = r#"{
            "dprint": "/usr/bin/dprint",
            "profiles": {
                "default": "/config/default.jsonc",
                "ignore": null
            },
            "match": { "**": "default" },
            "match_content": {
                "DO NOT EDIT": "ignore"
            }
        }"#;
        let config: DprintxConfig = serde_json::from_str(config_json).unwrap();
        let matcher = config.compile_content_patterns().unwrap().unwrap();

        // Pattern found on line 3 — still matches (regex searches full content).
        let content = "package main\n\nimport \"fmt\"\n// DO NOT EDIT\nfunc main() {}";
        assert_eq!(matcher.match_content(content), Some("ignore"));

        // Pattern not present at all.
        let content = "package main\n\nfunc main() {}";
        assert_eq!(matcher.match_content(content), None);
    }

    #[test]
    fn test_compile_content_patterns_invalid_regex() {
        let config_json = r#"{
            "dprint": "/usr/bin/dprint",
            "profiles": { "ignore": null },
            "match": { "**": "ignore" },
            "match_content": {
                "[invalid regex": "ignore"
            }
        }"#;
        let config: DprintxConfig = serde_json::from_str(config_json).unwrap();

        let result = config.compile_content_patterns();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid regex"));
    }

    #[test]
    fn test_content_matcher_first_match_wins() {
        let config_json = r#"{
            "dprint": "/usr/bin/dprint",
            "profiles": {
                "default": "/config/default.jsonc",
                "ignore": null,
                "strict": "/config/strict.jsonc"
            },
            "match": { "**": "default" },
            "match_content": {
                "DO NOT EDIT": "ignore",
                "STRICT MODE": "strict"
            }
        }"#;
        let config: DprintxConfig = serde_json::from_str(config_json).unwrap();
        let matcher = config.compile_content_patterns().unwrap().unwrap();

        // Both patterns match — first one (ignore) wins.
        assert_eq!(
            matcher.match_content("// DO NOT EDIT STRICT MODE"),
            Some("ignore")
        );
        // Only second matches.
        assert_eq!(
            matcher.match_content("// STRICT MODE enabled"),
            Some("strict")
        );
    }

    #[test]
    fn test_inject_extends_absent() {
        let mut config = serde_json::json!({
            "typescript": { "lineWidth": 120 }
        });
        inject_extends(&mut config, Path::new("/profiles/main.jsonc"));
        assert_eq!(config["extends"], "/profiles/main.jsonc");
    }

    #[test]
    fn test_inject_extends_string() {
        let mut config = serde_json::json!({
            "extends": "https://example.com/base.json"
        });
        inject_extends(&mut config, Path::new("/profiles/main.jsonc"));
        let extends = config["extends"].as_array().unwrap();
        assert_eq!(extends.len(), 2);
        assert_eq!(extends[0], "/profiles/main.jsonc");
        assert_eq!(extends[1], "https://example.com/base.json");
    }

    #[test]
    fn test_inject_extends_array() {
        let mut config = serde_json::json!({
            "extends": ["https://a.com/base.json", "https://b.com/extra.json"]
        });
        inject_extends(&mut config, Path::new("/profiles/main.jsonc"));
        let extends = config["extends"].as_array().unwrap();
        assert_eq!(extends.len(), 3);
        assert_eq!(extends[0], "/profiles/main.jsonc");
        assert_eq!(extends[1], "https://a.com/base.json");
        assert_eq!(extends[2], "https://b.com/extra.json");
    }

    #[test]
    fn test_find_local_config_direct() {
        let dir = std::env::temp_dir().join("dprintx-test-find-direct");
        let _ = std::fs::create_dir_all(&dir);
        let config_path = dir.join("dprint.json");
        std::fs::write(&config_path, "{}").unwrap();

        let result = find_local_config(&dir);
        assert_eq!(result, Some(config_path));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_find_local_config_walkup() {
        let root = std::env::temp_dir().join("dprintx-test-find-walkup");
        let sub = root.join("a").join("b").join("c");
        let _ = std::fs::create_dir_all(&sub);

        // Place config at root level.
        let config_path = root.join("dprint.jsonc");
        std::fs::write(&config_path, "{}").unwrap();

        // Find from deeply nested dir should walk up and find it.
        let result = find_local_config(&sub);
        assert_eq!(result, Some(config_path));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_find_local_config_prefers_json_over_jsonc() {
        let dir = std::env::temp_dir().join("dprintx-test-find-prefer");
        let _ = std::fs::create_dir_all(&dir);

        // Both exist — dprint.json should be found first.
        std::fs::write(dir.join("dprint.json"), "{}").unwrap();
        std::fs::write(dir.join("dprint.jsonc"), "{}").unwrap();

        let result = find_local_config(&dir);
        assert_eq!(result, Some(dir.join("dprint.json")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_find_local_config_none() {
        // Use a directory with no dprint config in its ancestors (temp dir is unlikely to have one).
        let dir = std::env::temp_dir().join("dprintx-test-find-none");
        let _ = std::fs::create_dir_all(&dir);

        // This test might find a real dprint.json somewhere up the tree,
        // so we just verify the function doesn't crash.
        let _ = find_local_config(&dir);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_clean_orphaned_configs() {
        let dir = std::env::temp_dir().join("dprintx-test-orphans");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // pid 0 is never a real process, so this file is unowned.
        let orphan = dir.join("merged-0-1.json");
        // The current process is alive, so its file must survive.
        let own = dir.join(format!("merged-{}-1.json", std::process::id()));
        // Anything not matching the naming scheme is none of our business.
        let unrelated = dir.join("notes.json");

        for path in [&orphan, &own, &unrelated] {
            std::fs::write(path, "{}").unwrap();
        }

        clean_orphaned_configs(&dir);

        assert!(!orphan.exists(), "orphaned config should be removed");
        assert!(own.exists(), "live process config should be kept");
        assert!(unrelated.exists(), "unrelated file should be kept");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_build_merged_config_no_local() {
        // No local config exists — returns None.
        let result = build_merged_config(
            Path::new("/nonexistent/dir"),
            Path::new("/profiles/main.jsonc"),
        )
        .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_build_merged_config_with_local() {
        let dir = std::env::temp_dir().join("dprintx-test-build-merged");
        let _ = std::fs::create_dir_all(&dir);

        // Write a local dprint.json.
        let local_path = dir.join("dprint.json");
        std::fs::write(&local_path, r#"{"typescript": {"lineWidth": 120}}"#).unwrap();

        let profile_path = Path::new("/profiles/main.jsonc");
        let guard = build_merged_config(&dir, profile_path).unwrap();
        assert!(guard.is_some());

        let tc = guard.unwrap();
        assert!(tc.path().exists());

        let content = std::fs::read_to_string(tc.path()).unwrap();
        let val: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(val["extends"], "/profiles/main.jsonc");
        assert_eq!(val["typescript"]["lineWidth"], 120);

        // File should be deleted when guard drops.
        let path = tc.path().to_path_buf();
        drop(tc);
        assert!(!path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_build_merged_config_preserves_existing_extends() {
        let dir = std::env::temp_dir().join("dprintx-test-build-merged-extends");
        let _ = std::fs::create_dir_all(&dir);

        let local_path = dir.join("dprint.json");
        std::fs::write(
            &local_path,
            r#"{"extends": "https://example.com/base.json", "typescript": {"lineWidth": 80}}"#,
        )
        .unwrap();

        let profile_path = Path::new("/profiles/main.jsonc");
        let tc = build_merged_config(&dir, profile_path).unwrap().unwrap();

        let content = std::fs::read_to_string(tc.path()).unwrap();
        let val: serde_json::Value = serde_json::from_str(&content).unwrap();

        // extends should now be an array with profile first.
        let extends = val["extends"].as_array().unwrap();
        assert_eq!(extends[0], "/profiles/main.jsonc");
        assert_eq!(extends[1], "https://example.com/base.json");

        let _ = std::fs::remove_dir_all(&dir);
        // tc drops here → temp file auto-deleted
    }

    #[test]
    fn test_read_local_config_json() {
        let dir = std::env::temp_dir().join("dprintx-test-read-local");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("dprint.json");
        std::fs::write(&path, r#"{"plugins": ["https://example.com/plugin.wasm"]}"#).unwrap();

        let val = read_local_config(&path).unwrap();
        assert!(val.is_object());
        assert!(val.get("plugins").unwrap().is_array());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_local_config_jsonc() {
        let dir = std::env::temp_dir().join("dprintx-test-read-local-jsonc");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("dprint.jsonc");
        std::fs::write(
            &path,
            r#"{
                // local overrides
                "typescript": {
                    "lineWidth": 120,
                },
                "plugins": [
                    "https://example.com/plugin.wasm",
                ],
            }"#,
        )
        .unwrap();

        let val = read_local_config(&path).unwrap();
        assert!(val.is_object());
        assert_eq!(val["typescript"]["lineWidth"], 120);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
