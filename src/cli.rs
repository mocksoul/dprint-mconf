/// Parsed CLI result.
#[derive(Debug)]
pub struct Cli {
    /// Override config path.
    pub config: Option<String>,
    /// Parsed command.
    pub command: CliCommand,
}

#[derive(Debug)]
pub enum CliCommand {
    /// Format files.
    Fmt {
        stdin: Option<String>,
        files: Vec<String>,
        flags: Vec<String>,
    },
    /// Check if files are formatted.
    Check {
        files: Vec<String>,
        flags: Vec<String>,
    },
    /// Show resolved config for a file.
    Config { file: Option<String> },
    /// List files that would be formatted.
    OutputFilePaths,
    /// Start LSP server.
    Lsp,
    /// Generate shell completions (patched with dprintx extras).
    Completions { shell: String },
    /// Passthrough to real dprint (unknown command or --help etc).
    Passthrough { args: Vec<String> },
}

impl Cli {
    /// Parse CLI from env args.
    /// Known commands are parsed by us; everything else is passthrough.
    pub fn parse() -> Self {
        let args: Vec<String> = std::env::args().skip(1).collect();
        Self::parse_from(&args)
    }

    fn parse_from(args: &[String]) -> Self {
        let mut config: Option<String> = None;
        let mut rest: Vec<String> = Vec::new();

        // Extract --config <path> from anywhere in args.
        let mut i = 0;
        while i < args.len() {
            if args[i] == "--config" {
                if i + 1 < args.len() {
                    config = Some(args[i + 1].clone());
                    i += 2;
                    continue;
                }
            } else if let Some(val) = args[i].strip_prefix("--config=") {
                config = Some(val.to_string());
                i += 1;
                continue;
            }
            rest.push(args[i].clone());
            i += 1;
        }

        // No subcommand or help flags → passthrough.
        if rest.is_empty() {
            return Self {
                config,
                command: CliCommand::Passthrough { args: rest },
            };
        }

        let subcmd = rest[0].as_str();
        let sub_args = &rest[1..];

        let command = match subcmd {
            "fmt" => Self::parse_fmt(sub_args),
            "check" => Self::parse_check(sub_args),
            "config" => CliCommand::Config {
                file: sub_args.first().cloned(),
            },
            "output-file-paths" => CliCommand::OutputFilePaths,
            "lsp" => CliCommand::Lsp,
            "completions" => CliCommand::Completions {
                shell: sub_args.first().cloned().unwrap_or_else(|| "zsh".into()),
            },
            // Everything else: --help, -h, --version, -V, license, completions, etc.
            _ => CliCommand::Passthrough { args: rest },
        };

        Self { config, command }
    }

    fn parse_fmt(args: &[String]) -> CliCommand {
        let mut stdin: Option<String> = None;
        let mut files: Vec<String> = Vec::new();
        let mut flags: Vec<String> = Vec::new();
        let mut positional_only = false;

        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--stdin" if !positional_only => {
                    if i + 1 < args.len() {
                        stdin = Some(args[i + 1].clone());
                        i += 2;
                        continue;
                    }
                }
                // Pass through help to real dprint.
                "-h" | "--help" if !positional_only => {
                    let mut passthrough = vec!["fmt".to_string()];
                    passthrough.extend_from_slice(args);
                    return CliCommand::Passthrough { args: passthrough };
                }
                "--" if !positional_only => positional_only = true,
                other if !positional_only && is_flag(other) => flags.push(other.to_string()),
                other => files.push(other.to_string()),
            }
            i += 1;
        }

        CliCommand::Fmt {
            stdin,
            files,
            flags,
        }
    }

    fn parse_check(args: &[String]) -> CliCommand {
        let mut files: Vec<String> = Vec::new();
        let mut flags: Vec<String> = Vec::new();
        let mut positional_only = false;

        for arg in args {
            match arg.as_str() {
                "-h" | "--help" if !positional_only => {
                    let mut passthrough = vec!["check".to_string()];
                    passthrough.extend_from_slice(args);
                    return CliCommand::Passthrough { args: passthrough };
                }
                "--" if !positional_only => positional_only = true,
                other if !positional_only && is_flag(other) => flags.push(other.to_string()),
                other => files.push(other.to_string()),
            }
        }

        CliCommand::Check { files, flags }
    }
}

/// Whether an argument is a flag rather than a path.
///
/// A bare `-` is a path by convention (stdin), and dprint has no single-dash
/// flag that takes a separate value, so every other leading dash is dprint's
/// to interpret. Guessing which flags exist would make dprintx go stale each
/// time dprint gains one; treating them as files silently formatted the
/// current directory instead of the requested path.
fn is_flag(arg: &str) -> bool {
    arg.starts_with('-') && arg != "-"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn test_fmt_stdin() {
        let cli = Cli::parse_from(&args("fmt --stdin test.yaml"));
        assert!(matches!(
            cli.command,
            CliCommand::Fmt { stdin: Some(_), .. }
        ));
    }

    #[test]
    fn test_fmt_files() {
        let cli = Cli::parse_from(&args("fmt a.go b.go"));
        if let CliCommand::Fmt {
            stdin,
            files,
            flags,
        } = &cli.command
        {
            assert!(stdin.is_none());
            assert_eq!(files, &["a.go", "b.go"]);
            assert!(flags.is_empty());
        } else {
            panic!("expected Fmt");
        }
    }

    #[test]
    fn test_check_files() {
        let cli = Cli::parse_from(&args("check a.yaml"));
        if let CliCommand::Check { files, flags } = &cli.command {
            assert_eq!(files, &["a.yaml"]);
            assert!(flags.is_empty());
        } else {
            panic!("expected Check");
        }
    }

    #[test]
    fn test_unknown_passthrough() {
        let cli = Cli::parse_from(&args("license"));
        assert!(matches!(cli.command, CliCommand::Passthrough { .. }));
    }

    #[test]
    fn test_help_passthrough() {
        let cli = Cli::parse_from(&args("--help"));
        assert!(matches!(cli.command, CliCommand::Passthrough { .. }));
    }

    #[test]
    fn test_version_passthrough() {
        let cli = Cli::parse_from(&args("-V"));
        assert!(matches!(cli.command, CliCommand::Passthrough { .. }));
    }

    #[test]
    fn test_config_extracted() {
        let cli = Cli::parse_from(&args("--config /tmp/test.jsonc fmt a.go"));
        assert_eq!(cli.config.as_deref(), Some("/tmp/test.jsonc"));
        assert!(matches!(cli.command, CliCommand::Fmt { .. }));
    }

    #[test]
    fn test_config_equals() {
        let cli = Cli::parse_from(&args("--config=/tmp/test.jsonc check"));
        assert_eq!(cli.config.as_deref(), Some("/tmp/test.jsonc"));
        assert!(matches!(cli.command, CliCommand::Check { .. }));
    }

    #[test]
    fn test_no_args_passthrough() {
        let cli = Cli::parse_from(&args(""));
        assert!(matches!(cli.command, CliCommand::Passthrough { .. }));
    }

    // A flag counted as a file is not merely dropped later: with no path left that
    // matches a profile, dprint is handed the caller's cwd and formats the tree it
    // happens to be standing in.
    #[test]
    fn test_fmt_flag_is_not_a_file() {
        let cli = Cli::parse_from(&args("fmt --allow-no-files a.go"));
        if let CliCommand::Fmt { files, flags, .. } = &cli.command {
            assert_eq!(files, &["a.go"]);
            assert_eq!(flags, &["--allow-no-files"]);
        } else {
            panic!("expected Fmt");
        }
    }

    #[test]
    fn test_check_flag_is_not_a_file() {
        let cli = Cli::parse_from(&args("check --incremental a.yaml"));
        if let CliCommand::Check { files, flags } = &cli.command {
            assert_eq!(files, &["a.yaml"]);
            assert_eq!(flags, &["--incremental"]);
        } else {
            panic!("expected Check");
        }
    }

    #[test]
    fn test_short_flag_is_not_a_file() {
        let cli = Cli::parse_from(&args("fmt -L a.go"));
        if let CliCommand::Fmt { files, flags, .. } = &cli.command {
            assert_eq!(files, &["a.go"]);
            assert_eq!(flags, &["-L"]);
        } else {
            panic!("expected Fmt");
        }
    }

    // `--` is how a path that looks like a flag gets through.
    #[test]
    fn test_double_dash_forces_files() {
        let cli = Cli::parse_from(&args("fmt --allow-no-files -- --weird.go"));
        if let CliCommand::Fmt { files, flags, .. } = &cli.command {
            assert_eq!(files, &["--weird.go"]);
            assert_eq!(flags, &["--allow-no-files"]);
        } else {
            panic!("expected Fmt");
        }
    }

    // A lone dash means stdin to every tool that reads paths, so it stays a file.
    #[test]
    fn test_bare_dash_is_a_file() {
        let cli = Cli::parse_from(&args("fmt -"));
        if let CliCommand::Fmt { files, flags, .. } = &cli.command {
            assert_eq!(files, &["-"]);
            assert!(flags.is_empty());
        } else {
            panic!("expected Fmt");
        }
    }

    #[test]
    fn test_fmt_help_passthrough() {
        let cli = Cli::parse_from(&args("fmt --help"));
        assert!(matches!(cli.command, CliCommand::Passthrough { .. }));
    }
}
