//! What a repository's own dprint config has to say about a file.
//!
//! A committed `dprint.json` is self-contained by necessity -- without its
//! `plugins` nobody but its author could run it -- so it describes exactly the
//! files its project intends to format, and dprintx defers to it there. Outside
//! that set the repo has expressed no opinion, and the global profile applies.
//!
//! Hence three answers, per file:
//!
//! - [`Verdict::Excluded`]  -- the config names this path in `excludes`; format
//!   it with nothing at all. Build output and vendored trees live here.
//! - [`Verdict::Owned`]     -- the config would format it; use the config as is.
//! - [`Verdict::Unclaimed`] -- neither; fall through to the profile. This is what
//!   lets a Go repo keep its own `.go` rules while Kot's profile still handles
//!   the `.md` files it never mentions.
//!
//! The two questions are answered differently. Coverage comes from dprint
//! itself, because a config with no `includes` formats whatever its plugins
//! recognise and only dprint knows what that is. Exclusion has to be computed
//! here, because `output-file-paths` reports excluded and unclaimed files the
//! same way -- as absence -- and the whole point is to tell them apart.
//!
//! Exclude matching goes through the `ignore` crate's `Gitignore`, the same
//! matcher dprint builds in `glob_matcher.rs`: a later `!pattern` opts a path
//! back in, and a bare basename matches at any depth.

use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::read_local_config;

/// What the repository's config says about a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Listed in `excludes` -- format with nothing, not even the profile.
    Excluded,
    /// The config would format it; run it as is, with no profile mixed in.
    Owned(PathBuf),
    /// The config says nothing about this file; the global profile decides.
    Unclaimed,
}

/// Ask the repository config covering `file` what to do with it.
///
/// Returns [`Verdict::Unclaimed`] when no repo config exists at all, which is
/// the common case outside a project and lands on the profile as before.
pub fn verdict(dprint_bin: &Path, file: &Path) -> Verdict {
    let Some(config_path) = file.parent().and_then(crate::config::find_local_config) else {
        return Verdict::Unclaimed;
    };

    // Excludes first: a path under `zig-out/` stays untouched even when its
    // extension is one the repo otherwise owns.
    if let Ok(excludes) = ConfigExcludes::load(&config_path)
        && excludes.excludes(file)
    {
        return Verdict::Excluded;
    }

    if config_covers(dprint_bin, &config_path, file) {
        Verdict::Owned(config_path)
    } else {
        Verdict::Unclaimed
    }
}

/// Verdicts for many files at once, reusing work across those that share a
/// repository.
///
/// Asking file by file would re-read the same config and spawn a fresh dprint
/// per file; a whole-tree `fmt` walks thousands of them, where ~9 ms each adds
/// up to minutes. Here each distinct repo config is parsed once and queried
/// once, with every file it might claim passed in a single command.
pub fn verdicts<'a>(
    dprint_bin: &Path,
    files: impl IntoIterator<Item = &'a Path>,
) -> HashMap<&'a Path, Verdict> {
    // Group by the config that governs each file, so one config means one query.
    let mut by_config: HashMap<Option<PathBuf>, Vec<&Path>> = HashMap::new();
    for file in files {
        let config = file.parent().and_then(crate::config::find_local_config);
        by_config.entry(config).or_default().push(file);
    }

    let mut result = HashMap::new();

    for (config_path, group) in by_config {
        let Some(config_path) = config_path else {
            result.extend(group.into_iter().map(|f| (f, Verdict::Unclaimed)));
            continue;
        };

        let excludes = ConfigExcludes::load(&config_path).ok();
        let (excluded, candidates): (Vec<_>, Vec<_>) = group.into_iter().partition(|file| {
            excludes
                .as_ref()
                .is_some_and(|matcher| matcher.excludes(file))
        });
        result.extend(excluded.into_iter().map(|f| (f, Verdict::Excluded)));

        let covered = covered_subset(dprint_bin, &config_path, &candidates);
        for file in candidates {
            let verdict = if covered.contains(file) {
                Verdict::Owned(config_path.clone())
            } else {
                Verdict::Unclaimed
            };
            result.insert(file, verdict);
        }
    }

    result
}

/// Which of `files` the config would format, asked in one command.
fn covered_subset<'a>(
    dprint_bin: &Path,
    config_path: &Path,
    files: &[&'a Path],
) -> HashSet<&'a Path> {
    let mut covered = HashSet::new();
    if files.is_empty() {
        return covered;
    }
    let Some(config_dir) = config_path.parent() else {
        return covered;
    };

    let mut command = Command::new(dprint_bin);
    command
        .args(["output-file-paths", "--config"])
        .arg(config_path);
    for file in files {
        command.arg(file);
    }

    let Ok(output) = command.current_dir(config_dir).output() else {
        return covered;
    };
    if !output.status.success() {
        return covered;
    }

    // dprint prints canonical absolute paths, which need not be spelled the way
    // the caller spelled them, so match on the resolved form of each input.
    let listed: HashSet<PathBuf> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(PathBuf::from)
        .collect();
    for file in files {
        let canonical = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
        if listed.contains(&canonical) {
            covered.insert(*file);
        }
    }

    covered
}

/// How deep an `extends` chain may go before we call it a cycle.
///
/// dprint itself detects cycles properly; we only need a bound that a sane
/// config never reaches, since hitting it just means falling back to treating
/// the file as not excluded.
const MAX_EXTENDS_DEPTH: usize = 25;

/// Excludes of a repo config, resolved against the directory the config lives in.
pub struct ConfigExcludes {
    matcher: Gitignore,
    base_dir: PathBuf,
}

impl ConfigExcludes {
    /// Collect excludes from a config file and everything it extends.
    ///
    /// Unlike `includes`, excludes accumulate across the whole `extends` chain
    /// (dprint's `resolve_config.rs` extends the parent's list rather than
    /// replacing it), so a pattern from an extended file still applies here.
    pub fn load(config_path: &Path) -> Result<Self> {
        let base_dir = config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        let mut patterns = Vec::new();
        collect(config_path, &mut patterns, 0);

        // dprint adds this one regardless of what the config says, so a matcher
        // that omitted it would disagree with the formatter it is describing.
        patterns.push("**/node_modules".to_string());

        let mut builder = GitignoreBuilder::new(&base_dir);
        for pattern in &patterns {
            // Patterns rooted at the config dir are written `/dist` in dprint
            // configs and `/dist` in gitignore too, so they carry over as is.
            builder
                .add_line(None, pattern)
                .with_context(|| format!("invalid exclude pattern: {pattern}"))?;
        }

        Ok(Self {
            matcher: builder.build().with_context(|| {
                format!("building exclude matcher for {}", config_path.display())
            })?,
            base_dir,
        })
    }

    /// Whether the repo config excludes this file.
    ///
    /// Ancestors count: a pattern naming a directory (`**/zig-out`) excludes
    /// everything beneath it. dprint gets that effect from pruning directories
    /// during traversal, which a single-file question has to reproduce by
    /// walking the path's parents itself.
    ///
    /// Paths outside the config's directory are not excluded: the config has no
    /// authority over them, and reporting otherwise would silently disable
    /// formatting for unrelated files.
    pub fn excludes(&self, file: &Path) -> bool {
        let Ok(relative) = file.strip_prefix(&self.base_dir) else {
            return false;
        };
        self.matcher
            .matched_path_or_any_parents(relative, false)
            .is_ignore()
    }
}

/// Whether a repo config claims this file, asked of dprint itself.
///
/// `output-file-paths` prints the path when the config would format it and
/// nothing when it would not, which is the one answer we cannot compute here:
/// a config with no `includes` at all formats whatever its plugins recognise,
/// and only dprint knows what those are.
///
/// The query costs about 9 ms and loads no plugins. It says nothing about
/// *why* a file is absent -- excluded and simply-not-covered look identical --
/// so callers must consult [`ConfigExcludes`] first.
pub fn config_covers(dprint_bin: &Path, config_path: &Path, file: &Path) -> bool {
    // dprint resolves a --config against the directory it runs in, so running
    // from the caller's cwd makes an absolute path outside that directory look
    // uncovered. The config's own directory is the root its patterns describe.
    let Some(config_dir) = config_path.parent() else {
        return false;
    };

    let Ok(output) = Command::new(dprint_bin)
        .args(["output-file-paths", "--config"])
        .arg(config_path)
        .arg(file)
        .current_dir(config_dir)
        .output()
    else {
        return false;
    };

    output.status.success() && !output.stdout.is_empty()
}

/// Walk a config and its local `extends`, appending each `excludes` entry.
///
/// Remote extends (http/https) are skipped: fetching them would put a network
/// call on the formatting path, and a config that reaches for the network to
/// decide about one file is worse than one that occasionally misses an exclude.
fn collect(config_path: &Path, out: &mut Vec<String>, depth: usize) {
    if depth >= MAX_EXTENDS_DEPTH {
        return;
    }
    let Ok(value) = read_local_config(config_path) else {
        return;
    };

    if let Some(excludes) = value.get("excludes").and_then(|v| v.as_array()) {
        out.extend(
            excludes
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string),
        );
    }

    let dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    for target in extends_targets(&value) {
        if target.starts_with("http://") || target.starts_with("https://") {
            continue;
        }
        collect(&dir.join(target), out, depth + 1);
    }
}

/// The `extends` entries of a config, which dprint accepts as either a single
/// string or an array of them.
fn extends_targets(value: &serde_json::Value) -> Vec<String> {
    match value.get("extends") {
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes files into a fresh directory and hands back its path.
    fn scratch(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dprintx-excludes-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (path, content) in files {
            let full = dir.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(full, content).unwrap();
        }
        dir
    }

    #[test]
    fn matches_directory_pattern() {
        let dir = scratch(
            "dirpat",
            &[("dprint.json", r#"{"excludes": ["**/zig-out", "/dist"]}"#)],
        );
        let ex = ConfigExcludes::load(&dir.join("dprint.json")).unwrap();

        assert!(ex.excludes(&dir.join("zig-out/build.md")));
        assert!(ex.excludes(&dir.join("nested/zig-out/build.md")));
        assert!(ex.excludes(&dir.join("dist/app.js")));
        assert!(!ex.excludes(&dir.join("src/main.zig")));

        // `/dist` is anchored at the config dir, so a nested dist is untouched.
        assert!(!ex.excludes(&dir.join("sub/dist/app.js")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The exact exclude list menoti ships, checked against what `dprint
    /// output-file-paths` reports for the same tree: of probe.md placed in each
    /// of these directories plus the repo root, only the root one survives.
    #[test]
    fn matches_dprint_on_a_real_config() {
        let dir = scratch(
            "real",
            &[(
                ".dprint.jsonc",
                r#"{
                     // trailing comma and comments, as a real jsonc config has
                     "excludes": ["**/.zig-cache", "**/zig-out", "**/.ci/bin"],
                   }"#,
            )],
        );
        let ex = ConfigExcludes::load(&dir.join(".dprint.jsonc")).unwrap();

        assert!(ex.excludes(&dir.join("zig-out/probe.md")));
        assert!(ex.excludes(&dir.join(".zig-cache/probe.md")));
        assert!(ex.excludes(&dir.join(".ci/bin/probe.md")));
        assert!(ex.excludes(&dir.join("node_modules/pkg/probe.md")));

        assert!(!ex.excludes(&dir.join("probe.md")));
        assert!(!ex.excludes(&dir.join(".ci/drift.sh")));
        assert!(!ex.excludes(&dir.join(".github/workflows/ci.yml")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn node_modules_excluded_without_being_configured() {
        let dir = scratch("implicit", &[("dprint.json", "{}")]);
        let ex = ConfigExcludes::load(&dir.join("dprint.json")).unwrap();

        assert!(ex.excludes(&dir.join("node_modules/pkg/index.js")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn negated_pattern_opts_back_in() {
        let dir = scratch(
            "negated",
            &[("dprint.json", r#"{"excludes": ["**/*.md", "!README.md"]}"#)],
        );
        let ex = ConfigExcludes::load(&dir.join("dprint.json")).unwrap();

        assert!(ex.excludes(&dir.join("docs/guide.md")));
        assert!(!ex.excludes(&dir.join("README.md")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inherits_excludes_through_extends() {
        let dir = scratch(
            "extends",
            &[
                (
                    "dprint.json",
                    r#"{"extends": "./base.json", "excludes": ["/own"]}"#,
                ),
                ("base.json", r#"{"excludes": ["/inherited"]}"#),
            ],
        );
        let ex = ConfigExcludes::load(&dir.join("dprint.json")).unwrap();

        assert!(ex.excludes(&dir.join("own/a.md")));
        assert!(ex.excludes(&dir.join("inherited/a.md")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extends_array_and_remote_entries() {
        let dir = scratch(
            "extends-array",
            &[
                (
                    "dprint.json",
                    r#"{"extends": ["https://example.com/remote.json", "./local.json"]}"#,
                ),
                ("local.json", r#"{"excludes": ["/vendor"]}"#),
            ],
        );
        let ex = ConfigExcludes::load(&dir.join("dprint.json")).unwrap();

        // The remote entry is skipped rather than fetched, and the local one
        // beside it still contributes.
        assert!(ex.excludes(&dir.join("vendor/lib.go")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extends_cycle_terminates() {
        let dir = scratch(
            "cycle",
            &[
                ("a.json", r#"{"extends": "./b.json", "excludes": ["/x"]}"#),
                ("b.json", r#"{"extends": "./a.json"}"#),
            ],
        );
        let ex = ConfigExcludes::load(&dir.join("a.json")).unwrap();

        assert!(ex.excludes(&dir.join("x/file.md")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn paths_outside_config_dir_are_not_excluded() {
        let dir = scratch(
            "outside",
            &[("dprint.json", r#"{"excludes": ["**/*.md"]}"#)],
        );
        let ex = ConfigExcludes::load(&dir.join("dprint.json")).unwrap();

        assert!(ex.excludes(&dir.join("a.md")));
        assert!(!ex.excludes(Path::new("/somewhere/else/a.md")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The coverage query runs the real dprint, so it is only meaningful when
    /// an installed binary is present.
    fn installed_dprint() -> Option<PathBuf> {
        let path = dirs::home_dir()?.join(".local/lib/dprintx/dprint");
        path.is_file().then_some(path)
    }

    /// Config and files live in a temp dir while the test process runs from the
    /// crate root, so a passing assertion here also shows the query does not
    /// depend on the caller's cwd -- the case that broke `dprintx fmt` on
    /// absolute paths before `run_dir()` existed.
    #[test]
    fn coverage_query_answers_for_a_real_config() {
        let Some(dprint) = installed_dprint() else {
            return;
        };
        let dir = scratch(
            "coverage",
            &[
                (
                    "dprint.json",
                    r#"{
                         "includes": ["**/*.md"],
                         "plugins": ["https://plugins.dprint.dev/markdown-0.21.1.wasm"]
                       }"#,
                ),
                ("covered.md", "hi\n"),
                ("nested/deep.md", "hi\n"),
                ("other.ts", "let x = 1\n"),
            ],
        );
        let config = dir.join("dprint.json");

        assert!(config_covers(&dprint, &config, &dir.join("covered.md")));
        assert!(config_covers(&dprint, &config, &dir.join("nested/deep.md")));
        assert!(!config_covers(&dprint, &config, &dir.join("other.ts")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_or_broken_config_excludes_nothing_beyond_defaults() {
        let dir = scratch("broken", &[("dprint.json", "{ this is not json")]);
        let ex = ConfigExcludes::load(&dir.join("dprint.json")).unwrap();

        assert!(!ex.excludes(&dir.join("a.md")));
        assert!(ex.excludes(&dir.join("node_modules/x.js")));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
