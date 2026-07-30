use anyhow::{Context, Result};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::{self, DprintxConfig, ProfileResolution};
use crate::matcher::ProfileMatcher;

/// Runs the real dprint binary with appropriate config.
pub struct DprintRunner {
    dprint_bin: std::path::PathBuf,
}

/// The directory dprint should run in when handed `files`.
///
/// dprint anchors a `--config` to the directory it runs in, so inheriting the
/// caller's cwd makes the outcome depend on where the command was typed: an
/// editor or hook formatting an absolute path from elsewhere gets "no files
/// found" instead of a formatted file. The files themselves say where they
/// belong, so use their deepest common directory.
fn run_dir(files: &[impl AsRef<Path>]) -> Option<PathBuf> {
    let mut common: Option<PathBuf> = None;

    for file in files {
        let abs = std::fs::canonicalize(file.as_ref()).ok()?;
        let dir = abs.parent()?.to_path_buf();
        common = Some(match common {
            None => dir,
            Some(current) => {
                let shared = current
                    .components()
                    .zip(dir.components())
                    .take_while(|(a, b)| a == b)
                    .map(|(a, _)| a)
                    .collect::<PathBuf>();
                if shared.as_os_str().is_empty() {
                    return None;
                }
                shared
            }
        });
    }

    common
}

impl DprintRunner {
    pub fn new(config: &DprintxConfig) -> Self {
        Self {
            dprint_bin: config.dprint_path(),
        }
    }

    /// Format stdin for a single file. Reads stdin, resolves config by filename,
    /// pipes through dprint fmt --stdin <filename> --config <resolved>.
    pub fn fmt_stdin(
        &self,
        filename: &str,
        matcher: &ProfileMatcher,
        config: &DprintxConfig,
        flags: &[String],
    ) -> Result<()> {
        let abs_path =
            std::fs::canonicalize(filename).unwrap_or_else(|_| std::path::PathBuf::from(filename));

        let config_path = matcher
            .resolve_config(&abs_path, config)
            .with_context(|| format!("resolving config for {filename}"))?;

        let Some(ProfileResolution::Config(profile_config)) = config_path else {
            // No profile matched or ignore — pass through stdin unchanged.
            let mut input = Vec::new();
            io::stdin()
                .read_to_end(&mut input)
                .context("reading stdin")?;
            io::stdout().write_all(&input)?;
            return Ok(());
        };

        // Try to build a merged config (local dprint.json + profile extends).
        // Hold the guard alive until dprint finishes — it deletes the temp file on drop.
        let merged_guard = if let Some(parent) = abs_path.parent() {
            config::build_merged_config(parent, &profile_config)?
        } else {
            None
        };
        let effective_config = match &merged_guard {
            Some(tc) => tc.path(),
            None => &profile_config,
        };

        // Read all stdin.
        let mut input = Vec::new();
        io::stdin()
            .read_to_end(&mut input)
            .context("reading stdin")?;

        // Run: dprint fmt --stdin <filename> --config <config_path>
        let mut cmd = Command::new(&self.dprint_bin);
        cmd.args(["fmt", "--stdin", filename, "--config"])
            .arg(effective_config)
            .args(flags)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = abs_path.parent() {
            cmd.current_dir(dir);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning dprint: {}", self.dprint_bin.display()))?;

        // Write input to child stdin.
        if let Some(ref mut stdin) = child.stdin {
            stdin.write_all(&input).context("writing to dprint stdin")?;
        }
        // Drop stdin to signal EOF.
        drop(child.stdin.take());

        let output = child.wait_with_output().context("waiting for dprint")?;

        // Forward stderr.
        if !output.stderr.is_empty() {
            io::stderr().write_all(&output.stderr)?;
        }

        // Forward stdout (formatted output).
        if !output.stdout.is_empty() {
            io::stdout().write_all(&output.stdout)?;
        }

        if !output.status.success() {
            std::process::exit(output.status.code().unwrap_or(1));
        }

        Ok(())
    }

    /// Output file paths for all profiles (deduped, filtered by match rules).
    pub fn output_file_paths(
        &self,
        matcher: &ProfileMatcher,
        config: &DprintxConfig,
    ) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        let mut profile_configs: Vec<std::path::PathBuf> = Vec::new();

        for (_pattern, profile_name) in config.match_rules_iter() {
            if let Some(ProfileResolution::Config(config_path)) =
                config.resolve_profile(profile_name)
                && seen.insert(config_path.clone())
            {
                profile_configs.push(config_path);
            }
        }

        let mut all_files = std::collections::BTreeSet::new();

        for profile_config in &profile_configs {
            let output = Command::new(&self.dprint_bin)
                .args(["output-file-paths", "--config"])
                .arg(profile_config)
                .output()
                .with_context(|| {
                    format!("getting file paths for config {}", profile_config.display())
                })?;

            if output.status.success() {
                let file_list = String::from_utf8_lossy(&output.stdout);
                for line in file_list.lines() {
                    let resolved = matcher.resolve_config(std::path::Path::new(line), config);
                    if let Ok(Some(ProfileResolution::Config(ref p))) = resolved
                        && p == profile_config
                    {
                        all_files.insert(line.to_string());
                    }
                }
            }
        }

        for file in &all_files {
            println!("{file}");
        }

        Ok(())
    }

    /// Generate shell completions, patching dprint's output with dprintx extras.
    pub fn completions(&self, shell: &str) -> Result<()> {
        let output = Command::new(&self.dprint_bin)
            .args(["completions", shell])
            .output()
            .context("running dprint completions")?;

        if !output.status.success() {
            io::stderr().write_all(&output.stderr)?;
            std::process::exit(output.status.code().unwrap_or(1));
        }

        let script = String::from_utf8_lossy(&output.stdout);

        // Patch the completions:
        let patched = script
            // 1. Fix file completion (add _files action to empty file args).
            .replace(
                "in the config file.:'",
                "in the config file.:_files'",
            )
            // 2. Update config subcommand description.
            .replace(
                "'config:Functionality related to the configuration file.'",
                "'config:Show resolved dprintx profiles and match rules.'",
            )
            // 3. Update --config description.
            .replace(
                "Path or url to JSON configuration file. Defaults to dprint.json(c) or .dprint.json(c) in current or ancestor directory when not provided.",
                "Override dprintx config path (default: ~/.config/dprint/dprintx.jsonc).",
            )
            // 4. Update lsp description.
            .replace(
                "'lsp:Starts up a language server for formatting files.'",
                "'lsp:Start LSP server with dprintx multi-profile support and URI rewriting.'",
            );

        io::stdout().write_all(patched.as_bytes())?;
        Ok(())
    }

    /// Passthrough raw args to real dprint (unknown commands, --help, etc).
    /// For --help/-h: capture output and append dprintx section.
    pub fn passthrough_raw(&self, args: &[String]) -> Result<()> {
        let is_help = args.iter().any(|a| a == "--help" || a == "-h");

        if is_help {
            let output = Command::new(&self.dprint_bin)
                .args(args)
                .output()
                .with_context(|| format!("running dprint {}", args.join(" ")))?;

            io::stdout().write_all(&output.stdout)?;
            io::stderr().write_all(&output.stderr)?;

            // Append dprintx-specific section.
            println!();
            println!("DPRINTX OPTIONS:");
            println!("  --config <PATH>     Override config path (~/.config/dprint/dprintx.jsonc)");
            println!();
            println!("DPRINTX SUBCOMMANDS:");
            println!("  config              Show resolved profiles and match rules.");
            println!("  config <FILE>       Show which dprint config would be used for a file.");
            println!();
            println!("DPRINTX CONFIG (dprintx.jsonc):");
            println!("  diff_pager          Pager for `dprint check` diffs (e.g. \"delta -s\").");
            println!();

            std::process::exit(output.status.code().unwrap_or(0));
        }

        let status = Command::new(&self.dprint_bin)
            .args(args)
            .status()
            .with_context(|| format!("running dprint {}", args.join(" ")))?;

        if !status.success() {
            std::process::exit(status.code().unwrap_or(1));
        }

        Ok(())
    }

    /// Format explicit files, grouped by effective config (profile or merged).
    pub fn fmt_files(
        &self,
        files: &[String],
        matcher: &ProfileMatcher,
        config: &DprintxConfig,
        flags: &[String],
    ) -> Result<()> {
        // Hold all merged config guards alive until dprint finishes.
        let mut _guards: Vec<config::TempConfig> = Vec::new();
        let mut groups: std::collections::HashMap<PathBuf, Vec<&str>> =
            std::collections::HashMap::new();

        for file in files {
            let abs_path = std::fs::canonicalize(file).unwrap_or_else(|_| PathBuf::from(file));
            let resolution = matcher
                .resolve_config(&abs_path, config)
                .with_context(|| format!("resolving config for {file}"))?;
            if let Some(ProfileResolution::Config(profile_config)) = resolution {
                let effective = if let Some(parent) = abs_path.parent() {
                    match config::build_merged_config(parent, &profile_config)? {
                        Some(tc) => {
                            let p = tc.path().to_path_buf();
                            _guards.push(tc);
                            p
                        }
                        None => profile_config,
                    }
                } else {
                    profile_config
                };
                groups.entry(effective).or_default().push(file);
            }
        }

        // Run dprint once per group.
        let mut failed = false;
        for (config_path, group_files) in &groups {
            let mut cmd = Command::new(&self.dprint_bin);
            cmd.arg("fmt").arg("--config").arg(config_path).args(flags);
            for f in group_files {
                cmd.arg(f);
            }
            if let Some(dir) = run_dir(group_files) {
                cmd.current_dir(dir);
            }

            let status = cmd.status().with_context(|| {
                format!("running dprint fmt --config {}", config_path.display())
            })?;

            if !status.success() {
                failed = true;
            }
        }

        if failed {
            std::process::exit(1);
        }

        Ok(())
        // _guards drop here → temp files deleted
    }

    /// Format all files using all profiles.
    ///
    /// For each profile, runs `dprint output-file-paths --config <profile>` to get the
    /// file list, filters by match rules, then runs `dprint fmt --config <profile> <files>`.
    pub fn fmt_all(
        &self,
        matcher: &ProfileMatcher,
        config: &DprintxConfig,
        flags: &[String],
    ) -> Result<()> {
        self.run_all("fmt", matcher, config, None, flags)
    }

    /// Format files under specific directories using all profiles.
    pub fn fmt_dirs(
        &self,
        dirs: &[PathBuf],
        matcher: &ProfileMatcher,
        config: &DprintxConfig,
        flags: &[String],
    ) -> Result<()> {
        self.run_all("fmt", matcher, config, Some(dirs), flags)
    }

    /// Check all files using all profiles.
    /// If diff_pager is configured, produces unified diff output.
    pub fn check_all(
        &self,
        matcher: &ProfileMatcher,
        config: &DprintxConfig,
        flags: &[String],
    ) -> Result<()> {
        if config.diff_pager.is_some() {
            return self.check_diff_all(matcher, config, None, flags);
        }
        self.run_all("check", matcher, config, None, flags)
    }

    /// Check files under specific directories using all profiles.
    pub fn check_dirs(
        &self,
        dirs: &[PathBuf],
        matcher: &ProfileMatcher,
        config: &DprintxConfig,
        flags: &[String],
    ) -> Result<()> {
        if config.diff_pager.is_some() {
            return self.check_diff_all(matcher, config, Some(dirs), flags);
        }
        self.run_all("check", matcher, config, Some(dirs), flags)
    }

    /// Run a subcommand (fmt/check) for all profiles.
    /// Files are grouped by effective config (merged local + profile, or just profile).
    /// If `dir_filter` is set, only files under those directories are included.
    fn run_all(
        &self,
        subcmd: &str,
        matcher: &ProfileMatcher,
        config: &DprintxConfig,
        dir_filter: Option<&[PathBuf]>,
        flags: &[String],
    ) -> Result<()> {
        let mut failed = false;

        // Collect unique profile config paths in order.
        let mut seen = std::collections::HashSet::new();
        let mut profile_configs: Vec<(String, std::path::PathBuf)> = Vec::new();

        for (_pattern, profile_name) in config.match_rules_iter() {
            if seen.insert(profile_name.to_string())
                && let Some(ProfileResolution::Config(config_path)) =
                    config.resolve_profile(profile_name)
            {
                profile_configs.push((profile_name.to_string(), config_path));
            }
        }

        // Hold all merged config guards alive until all dprint commands finish.
        let mut _guards: Vec<config::TempConfig> = Vec::new();
        let mut effective_groups: std::collections::HashMap<PathBuf, Vec<String>> =
            std::collections::HashMap::new();

        for (profile_name, profile_config) in &profile_configs {
            // Get file list from dprint for this profile.
            let output = Command::new(&self.dprint_bin)
                .args(["output-file-paths", "--config"])
                .arg(profile_config)
                .output()
                .with_context(|| {
                    format!(
                        "getting file paths for profile {profile_name}: {}",
                        profile_config.display()
                    )
                })?;

            if !output.status.success() {
                eprintln!("dprintx: warning: output-file-paths failed for profile {profile_name}");
                continue;
            }

            let file_list = String::from_utf8_lossy(&output.stdout);
            for line in file_list.lines() {
                // Filter by directory prefixes if specified.
                if let Some(dirs) = dir_filter {
                    let file_path = std::path::Path::new(line);
                    if !dirs.iter().any(|d| file_path.starts_with(d)) {
                        continue;
                    }
                }

                // Only include files that match this profile.
                let resolved = matcher.resolve_config(std::path::Path::new(line), config);
                match resolved {
                    Ok(Some(ProfileResolution::Config(ref p))) if p == profile_config => {}
                    _ => continue,
                }

                // Resolve effective config (merged or profile).
                let file_path = std::path::Path::new(line);
                let effective = if let Some(parent) = file_path.parent() {
                    match config::build_merged_config(parent, profile_config)? {
                        Some(tc) => {
                            let p = tc.path().to_path_buf();
                            _guards.push(tc);
                            p
                        }
                        None => profile_config.clone(),
                    }
                } else {
                    profile_config.clone()
                };
                effective_groups
                    .entry(effective)
                    .or_default()
                    .push(line.to_string());
            }
        }

        // Run dprint once per effective config group.
        for (effective_config, files) in &effective_groups {
            if files.is_empty() {
                continue;
            }

            let mut cmd = Command::new(&self.dprint_bin);
            cmd.arg(subcmd)
                .arg("--config")
                .arg(effective_config)
                .args(flags);
            for f in files {
                cmd.arg(f);
            }
            if let Some(dir) = run_dir(files) {
                cmd.current_dir(dir);
            }

            let status = cmd.status().with_context(|| {
                format!(
                    "running dprint {subcmd} --config {}",
                    effective_config.display()
                )
            })?;

            if !status.success() {
                failed = true;
            }
        }

        if failed {
            std::process::exit(1);
        }

        Ok(())
    }

    /// Check explicit files, grouped by effective config (profile or merged).
    /// If diff_pager is configured, produces unified diff output.
    pub fn check_files(
        &self,
        files: &[String],
        matcher: &ProfileMatcher,
        config: &DprintxConfig,
        flags: &[String],
    ) -> Result<()> {
        if config.diff_pager.is_some() {
            return self.check_diff_files(files, matcher, config);
        }

        let mut _guards: Vec<config::TempConfig> = Vec::new();
        let mut groups: std::collections::HashMap<PathBuf, Vec<&str>> =
            std::collections::HashMap::new();

        for file in files {
            let abs_path = std::fs::canonicalize(file).unwrap_or_else(|_| PathBuf::from(file));
            let resolution = matcher
                .resolve_config(&abs_path, config)
                .with_context(|| format!("resolving config for {file}"))?;
            if let Some(ProfileResolution::Config(profile_config)) = resolution {
                let effective = if let Some(parent) = abs_path.parent() {
                    match config::build_merged_config(parent, &profile_config)? {
                        Some(tc) => {
                            let p = tc.path().to_path_buf();
                            _guards.push(tc);
                            p
                        }
                        None => profile_config,
                    }
                } else {
                    profile_config
                };
                groups.entry(effective).or_default().push(file);
            }
        }

        let mut failed = false;
        for (config_path, group_files) in &groups {
            let mut cmd = Command::new(&self.dprint_bin);
            cmd.arg("check")
                .arg("--config")
                .arg(config_path)
                .args(flags);
            for f in group_files {
                cmd.arg(f);
            }
            if let Some(dir) = run_dir(group_files) {
                cmd.current_dir(dir);
            }

            let status = cmd.status().with_context(|| {
                format!("running dprint check --config {}", config_path.display())
            })?;

            if !status.success() {
                failed = true;
            }
        }

        if failed {
            std::process::exit(1);
        }

        Ok(())
    }

    // ---- diff_pager support ----

    /// Check all files with unified diff output.
    /// If `dir_filter` is set, only files under those directories are included.
    fn check_diff_all(
        &self,
        matcher: &ProfileMatcher,
        config: &DprintxConfig,
        dir_filter: Option<&[PathBuf]>,
        // Diff output is produced by dprintx from `fmt --stdin`, so dprint's own
        // check flags have nothing to act on here. Accepted to keep the call sites
        // uniform with the non-diff path.
        _flags: &[String],
    ) -> Result<()> {
        let mut all_diff = String::new();
        let mut _guards: Vec<config::TempConfig> = Vec::new();

        let mut seen = std::collections::HashSet::new();
        let mut profile_configs: Vec<PathBuf> = Vec::new();
        for (_pattern, profile_name) in config.match_rules_iter() {
            if seen.insert(profile_name.to_string())
                && let Some(ProfileResolution::Config(config_path)) =
                    config.resolve_profile(profile_name)
            {
                profile_configs.push(config_path);
            }
        }

        for profile_config in &profile_configs {
            // Get changed files for this profile.
            let changed = self.list_different(profile_config)?;
            for file in &changed {
                // Filter by directory prefixes if specified.
                if let Some(dirs) = dir_filter {
                    let file_path = std::path::Path::new(file.as_str());
                    if !dirs.iter().any(|d| file_path.starts_with(d)) {
                        continue;
                    }
                }

                // Filter: only files that belong to this profile.
                let resolved = matcher.resolve_config(std::path::Path::new(file), config);
                match resolved {
                    Ok(Some(ProfileResolution::Config(ref p))) if p == profile_config => {}
                    _ => continue,
                }

                // Resolve effective config (merged or profile).
                let file_path = std::path::Path::new(file.as_str());
                let effective = if let Some(parent) = file_path.parent() {
                    match config::build_merged_config(parent, profile_config)? {
                        Some(tc) => {
                            let p = tc.path().to_path_buf();
                            _guards.push(tc);
                            p
                        }
                        None => profile_config.clone(),
                    }
                } else {
                    profile_config.clone()
                };

                if let Some(diff) = self.unified_diff_for_file(file, &effective)? {
                    all_diff.push_str(&diff);
                }
            }
        }

        self.output_diff(&all_diff, config)
    }

    /// Check explicit files with unified diff output.
    fn check_diff_files(
        &self,
        files: &[String],
        matcher: &ProfileMatcher,
        config: &DprintxConfig,
    ) -> Result<()> {
        let mut all_diff = String::new();
        let mut _guards: Vec<config::TempConfig> = Vec::new();

        for file in files {
            let abs_path = std::fs::canonicalize(file).unwrap_or_else(|_| PathBuf::from(file));
            let resolution = matcher
                .resolve_config(&abs_path, config)
                .with_context(|| format!("resolving config for {file}"))?;

            let Some(ProfileResolution::Config(profile_config)) = resolution else {
                continue; // No profile matched or ignore — skip.
            };

            // Resolve effective config (merged or profile).
            let effective = if let Some(parent) = abs_path.parent() {
                match config::build_merged_config(parent, &profile_config)? {
                    Some(tc) => {
                        let p = tc.path().to_path_buf();
                        _guards.push(tc);
                        p
                    }
                    None => profile_config,
                }
            } else {
                profile_config
            };

            if let Some(diff) = self.unified_diff_for_file(file, &effective)? {
                all_diff.push_str(&diff);
            }
        }

        self.output_diff(&all_diff, config)
    }

    /// Get list of files that differ from formatted output.
    fn list_different(&self, config_path: &PathBuf) -> Result<Vec<String>> {
        let output = Command::new(&self.dprint_bin)
            .args(["check", "--list-different", "--config"])
            .arg(config_path)
            .output()
            .with_context(|| {
                format!(
                    "running dprint check --list-different --config {}",
                    config_path.display()
                )
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout.lines().map(String::from).collect())
    }

    /// Generate unified diff for a single file.
    /// Returns None if file is already formatted.
    fn unified_diff_for_file(&self, file: &str, config_path: &PathBuf) -> Result<Option<String>> {
        // Read original.
        let original = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;

        // Format via dprint.
        let mut cmd = Command::new(&self.dprint_bin);
        cmd.args(["fmt", "--stdin", file, "--config"])
            .arg(config_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(dir) = run_dir(&[file]) {
            cmd.current_dir(dir);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning dprint fmt --stdin {file}"))?;

        if let Some(ref mut stdin) = child.stdin {
            stdin.write_all(original.as_bytes())?;
        }
        drop(child.stdin.take());

        let output = child.wait_with_output()?;
        let formatted = String::from_utf8_lossy(&output.stdout);

        if original == formatted.as_ref() {
            return Ok(None);
        }

        // Build unified diff via system diff.
        let label = file.to_string();

        let tmp_dir = std::env::temp_dir();
        let orig_path = tmp_dir.join("dprintx-orig");
        let fmt_path = tmp_dir.join("dprintx-fmt");
        std::fs::write(&orig_path, &original)?;
        std::fs::write(&fmt_path, formatted.as_bytes())?;

        let diff_out = Command::new("diff")
            .args(["-u", "--label", &label, "--label", &label])
            .arg(&orig_path)
            .arg(&fmt_path)
            .output()
            .context("running diff")?;

        let _ = std::fs::remove_file(&orig_path);
        let _ = std::fs::remove_file(&fmt_path);

        let diff_text = String::from_utf8_lossy(&diff_out.stdout);
        if diff_text.is_empty() {
            return Ok(None);
        }

        Ok(Some(diff_text.into_owned()))
    }

    /// Output collected diff: through pager if TTY, raw if pipe.
    fn output_diff(&self, diff: &str, config: &DprintxConfig) -> Result<()> {
        if diff.is_empty() {
            return Ok(());
        }

        let has_diff = !diff.is_empty();

        if io::stdout().is_terminal() {
            // TTY: pipe through diff_pager.
            if let Some(ref pager_cmd) = config.diff_pager {
                let parts: Vec<&str> = pager_cmd.split_whitespace().collect();
                if let Some((cmd, args)) = parts.split_first() {
                    let mut child = Command::new(cmd)
                        .args(args)
                        .stdin(Stdio::piped())
                        .spawn()
                        .with_context(|| format!("spawning pager: {pager_cmd}"))?;

                    if let Some(ref mut stdin) = child.stdin {
                        let _ = stdin.write_all(diff.as_bytes());
                    }
                    drop(child.stdin.take());

                    let _ = child.wait()?;
                    if has_diff {
                        std::process::exit(1);
                    }
                    return Ok(());
                }
            }
        }

        // Not a TTY or no pager: raw unified diff to stdout.
        io::stdout().write_all(diff.as_bytes())?;

        if has_diff {
            std::process::exit(1);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn test_run_dir_single_file() {
        let dir = scratch("dprintx-rundir-single");
        let file = dir.join("a.md");
        std::fs::write(&file, "").unwrap();

        assert_eq!(run_dir(&[&file]), Some(dir.clone()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_run_dir_shared_parent() {
        let dir = scratch("dprintx-rundir-shared");
        let sub = dir.join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        let a = dir.join("a.md");
        let b = sub.join("b.md");
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();

        // Deepest directory holding both, so a config found from there covers
        // every file handed to dprint.
        assert_eq!(run_dir(&[&a, &b]), Some(dir.clone()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_run_dir_missing_file() {
        // Nothing to anchor to: leave the cwd alone rather than guess.
        assert_eq!(run_dir(&[PathBuf::from("/nonexistent/x.md")]), None);
    }

    #[test]
    fn test_run_dir_empty() {
        let empty: [PathBuf; 0] = [];
        assert_eq!(run_dir(&empty), None);
    }
}
