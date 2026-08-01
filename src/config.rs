use anyhow::{Context, Result};
use regex::{RegexSet, RegexSetBuilder};
use serde::Deserialize;
use serde_json::Map;
use std::path::{Path, PathBuf};

/// Result of resolving a profile name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileResolution {
    /// Profile maps to a config file path.
    Config(PathBuf),
    /// Profile is explicitly set to null (ignore/skip file).
    Ignore,
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

    /// Pass `--no-gitignore` to backends so gitignored files still format in
    /// the editor. Opening a file is already an explicit request for it, and
    /// config excludes still decide what dprint owns.
    /// Needs a dprint that accepts the flag on `lsp`.
    #[serde(default = "default_lsp_no_gitignore")]
    pub lsp_no_gitignore: bool,
}

fn default_lsp_timeout_ms() -> u64 {
    30_000
}

fn default_lsp_no_gitignore() -> bool {
    true
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

/// Config file names dprint discovers automatically, in its own priority order.
///
/// Mirrors `POSSIBLE_CONFIG_FILE_NAMES` in dprint's `resolve_main_config_path.rs`.
/// The order matters: when a directory holds several of these, dprint silently
/// picks the first and never reads the rest.
pub const LOCAL_CONFIG_FILE_NAMES: &[&str] = &[
    "dprint.json",
    "dprint.jsonc",
    ".dprint.json",
    ".dprint.jsonc",
];

/// Find a local dprint config by walking up from the given directory.
///
/// The global config directory is skipped. A `dprint.jsonc` there is dprint's
/// own fallback for running without `--config`, not a project's config, and
/// treating it as one would make the profiles kept beside it format by whatever
/// that fallback happens to say.
pub fn find_local_config(start_dir: &Path) -> Option<PathBuf> {
    find_local_config_skipping(start_dir, global_config_dir().as_deref())
}

fn find_local_config_skipping(start_dir: &Path, skip_dir: Option<&Path>) -> Option<PathBuf> {
    let mut dir = start_dir;
    loop {
        if Some(dir) != skip_dir {
            for name in LOCAL_CONFIG_FILE_NAMES {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        dir = dir.parent()?;
    }
}

/// Where dprint looks for its global config, by dprint's own rules.
///
/// `resolve_global_config_dir` in dprint honours `DPRINT_CONFIG_DIR` first and
/// otherwise appends `dprint` to the XDG config directory.
fn global_config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("DPRINT_CONFIG_DIR") {
        let dir = PathBuf::from(dir);
        if !dir.as_os_str().is_empty() {
            return Some(dir);
        }
    }
    dirs::config_dir().map(|dir| dir.join("dprint"))
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

        // LSP defaults apply to configs written before these knobs existed.
        assert_eq!(config.lsp_timeout_ms, 30_000);
        assert!(config.lsp_no_gitignore);
    }

    #[test]
    fn test_lsp_options_are_overridable() {
        let input = r#"{
  "dprint": "dprint",
  "profiles": { "default": "~/.config/dprint/dprint-default.jsonc" },
  "match": { "**": "default" },
  "lsp_timeout_ms": 5000,
  "lsp_no_gitignore": false,
}"#;
        let json = strip_jsonc_comments(input);
        let config: DprintxConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(config.lsp_timeout_ms, 5000);
        assert!(!config.lsp_no_gitignore);
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
    fn test_find_local_config_direct() {
        let dir = std::env::temp_dir().join("dprintx-test-find-direct");
        let _ = std::fs::create_dir_all(&dir);
        let config_path = dir.join("dprint.json");
        std::fs::write(&config_path, "{}").unwrap();

        let result = find_local_config(&dir);
        assert_eq!(result, Some(config_path));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// dprint's own `dprint.jsonc` lives in the global config dir next to the
    /// profiles, and claiming it would make those profiles format by whatever
    /// the fallback config says rather than by their match rule.
    #[test]
    fn global_config_dir_is_not_a_project() {
        let dir = std::env::temp_dir().join("dprintx-test-global-skip");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("dprint.jsonc"), "{}").unwrap();

        assert_eq!(find_local_config_skipping(&dir, Some(&dir)), None);
        // Any other directory keeps its config.
        assert_eq!(
            find_local_config_skipping(&dir, Some(Path::new("/nowhere"))),
            Some(dir.join("dprint.jsonc"))
        );

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
    fn test_find_local_config_name_priority() {
        let dir = std::env::temp_dir().join("dprintx-test-find-prefer");
        let _ = std::fs::create_dir_all(&dir);

        // Create all names at once, then remove them one by one: each removal
        // must uncover exactly the next name in dprint's own priority order.
        for name in LOCAL_CONFIG_FILE_NAMES {
            std::fs::write(dir.join(name), "{}").unwrap();
        }
        for name in LOCAL_CONFIG_FILE_NAMES {
            assert_eq!(find_local_config(&dir), Some(dir.join(name)));
            std::fs::remove_file(dir.join(name)).unwrap();
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_find_local_config_hidden_walkup() {
        // A repo whose only config is `.dprint.jsonc` (menoti does this) must not
        // look like a repo without any config at all.
        let root = std::env::temp_dir().join("dprintx-test-find-hidden");
        let sub = root.join("src").join("nested");
        let _ = std::fs::create_dir_all(&sub);

        let config_path = root.join(".dprint.jsonc");
        std::fs::write(&config_path, "{}").unwrap();

        assert_eq!(find_local_config(&sub), Some(config_path));

        let _ = std::fs::remove_dir_all(&root);
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
