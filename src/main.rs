mod cli;
mod config;
mod lsp;
mod matcher;
mod repo_config;
mod runner;

use anyhow::{Context, Result};
use std::path::Path;

use cli::{Cli, CliCommand};
use config::{DprintxConfig, ProfileResolution};
use matcher::ProfileMatcher;
use runner::DprintRunner;

/// Split arguments into plain files and directories.
fn split_files_and_dirs(args: &[String]) -> (Vec<String>, Vec<String>) {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for arg in args {
        if Path::new(arg).is_dir() {
            dirs.push(arg.clone());
        } else {
            files.push(arg.clone());
        }
    }
    (files, dirs)
}

fn main() -> Result<()> {
    // Prevent infinite recursion when symlinked as `dprint` with no config.
    if std::env::var("DPRINTX_ACTIVE").is_ok() {
        anyhow::bail!(
            "dprintx: recursive call detected — \
             create ~/.config/dprint/dprintx.jsonc or ensure the real dprint is in PATH"
        );
    }

    let cli = Cli::parse();
    let config = load_config(cli.config.as_deref())?;

    // No config — passthrough everything to dprint.
    let Some(config) = config else {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let status = std::process::Command::new("dprint")
            .env("DPRINTX_ACTIVE", "1")
            .args(&args)
            .status()
            .context("cannot run dprint (no dprintx config, falling back to dprint in PATH)")?;
        std::process::exit(status.code().unwrap_or(1));
    };

    // Commands that don't need matcher.
    match &cli.command {
        CliCommand::Passthrough { args } => {
            let runner = DprintRunner::new(&config);
            runner.passthrough_raw(args)?;
            return Ok(());
        }
        CliCommand::Completions { shell } => {
            let runner = DprintRunner::new(&config);
            runner.completions(shell)?;
            return Ok(());
        }
        _ => {}
    }

    let matcher = ProfileMatcher::from_config(&config)?;
    let runner = DprintRunner::new(&config);

    match cli.command {
        CliCommand::Fmt {
            stdin,
            files,
            flags,
        } => {
            if let Some(ref filename) = stdin {
                runner.fmt_stdin(filename, &matcher, &config, &flags)?;
            } else if files.is_empty() {
                runner.fmt_all(&matcher, &config, &flags)?;
            } else {
                let (plain_files, dirs) = split_files_and_dirs(&files);
                if !plain_files.is_empty() {
                    runner.fmt_files(&plain_files, &matcher, &config, &flags)?;
                }
                if !dirs.is_empty() {
                    let dir_paths: Vec<_> = dirs
                        .iter()
                        .map(|d| {
                            std::fs::canonicalize(d).unwrap_or_else(|_| std::path::PathBuf::from(d))
                        })
                        .collect();
                    runner.fmt_dirs(&dir_paths, &matcher, &config, &flags)?;
                }
            }
        }
        CliCommand::Check { files, flags } => {
            if files.is_empty() {
                runner.check_all(&matcher, &config, &flags)?;
            } else {
                let (plain_files, dirs) = split_files_and_dirs(&files);
                if !plain_files.is_empty() {
                    runner.check_files(&plain_files, &matcher, &config, &flags)?;
                }
                if !dirs.is_empty() {
                    let dir_paths: Vec<_> = dirs
                        .iter()
                        .map(|d| {
                            std::fs::canonicalize(d).unwrap_or_else(|_| std::path::PathBuf::from(d))
                        })
                        .collect();
                    runner.check_dirs(&dir_paths, &matcher, &config, &flags)?;
                }
            }
        }
        CliCommand::Config { file } => {
            cmd_config(&matcher, &config, file.as_deref())?;
        }
        CliCommand::OutputFilePaths => {
            runner.output_file_paths(&matcher, &config)?;
        }
        CliCommand::Lsp => {
            let proxy = lsp::LspProxy::new(config.dprint_path(), matcher, config);
            proxy.run()?;
        }
        CliCommand::Completions { .. } | CliCommand::Passthrough { .. } => unreachable!(),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_all_files() {
        let args = vec!["foo.go".into(), "bar.rs".into()];
        let (files, dirs) = split_files_and_dirs(&args);
        // Non-existent paths are treated as files (not directories).
        assert_eq!(files, vec!["foo.go", "bar.rs"]);
        assert!(dirs.is_empty());
    }

    #[test]
    fn test_split_all_dirs() {
        let dir = std::env::temp_dir().join("dprintx-test-split-dirs");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let args = vec![dir.to_string_lossy().into_owned()];
        let (files, dirs) = split_files_and_dirs(&args);
        assert!(files.is_empty());
        assert_eq!(dirs.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_split_mixed() {
        let dir = std::env::temp_dir().join("dprintx-test-split-mixed");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let args = vec![
            "explicit.go".into(),
            dir.to_string_lossy().into_owned(),
            "another.rs".into(),
        ];
        let (files, dirs) = split_files_and_dirs(&args);
        assert_eq!(files, vec!["explicit.go", "another.rs"]);
        assert_eq!(dirs.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

fn load_config(config_path: Option<&str>) -> Result<Option<DprintxConfig>> {
    match config_path {
        // Explicit --config path: must exist and be valid.
        Some(path) => DprintxConfig::load(Path::new(path)).map(Some),
        // Default path: None if file doesn't exist, error if invalid.
        None => DprintxConfig::try_load_default(),
    }
}

/// Show which config would be used for a given file.
fn cmd_config(matcher: &ProfileMatcher, config: &DprintxConfig, file: Option<&str>) -> Result<()> {
    match file {
        Some(f) => {
            let abs_path = std::fs::canonicalize(f).unwrap_or_else(|_| std::path::PathBuf::from(f));
            let config_path = matcher
                .resolve_config(&abs_path, config)
                .with_context(|| format!("resolving config for {f}"))?;

            // Report the config that would actually be used, repo routing and
            // all -- printing the profile alone would explain nothing about a
            // file the repository claims or excludes.
            match config_path {
                Some(ProfileResolution::Config(profile)) => {
                    match repo_config::verdict(&config.dprint_path(), &abs_path) {
                        repo_config::Verdict::Excluded => {
                            println!("(excluded by repo config)")
                        }
                        repo_config::Verdict::Owned(repo) => println!("{}", repo.display()),
                        repo_config::Verdict::Unclaimed => println!("{}", profile.display()),
                    }
                }
                Some(ProfileResolution::Ignore) => println!("(ignored)"),
                None => println!("(no matching profile)"),
            }
        }
        None => {
            println!("dprint: {}", config.dprint_path().display());
            println!("profiles:");
            for (name, _) in &config.profiles {
                match config.resolve_profile(name) {
                    Some(ProfileResolution::Config(path)) => {
                        println!("  {name}: {}", path.display());
                    }
                    Some(ProfileResolution::Ignore) => {
                        println!("  {name}: (ignore)");
                    }
                    None => {}
                }
            }
            println!("match rules:");
            for (pattern, profile) in config.match_rules_iter() {
                println!("  {pattern} -> {profile}");
            }
            if config.match_content.is_some() {
                println!("match content rules:");
                for (pattern, profile) in config.match_content_rules_iter() {
                    println!("  /{pattern}/ -> {profile}");
                }
            }
        }
    }
    Ok(())
}
