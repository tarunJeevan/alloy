// Minimal CLI argument parsing without external crates.
//
// `clap` will be added in Phase 9 (Chunk 9.3) when `--help`, `--version`, `--config`, and `--log-level` are needed. For now, we only need to handle: `alloy [file.md]`
//
// Any unrecognized argument that starts with `-` is treated as an error.

use std::path::PathBuf;
use std::process;

/// Parsed command-line arguments.
#[derive(Debug, Default)]
pub struct CliArgs {
    /// Optional path to a markdown file to open on startup.
    pub file: Option<PathBuf>,
}

impl CliArgs {
    /// Parse `std::env::args()`, exiting with code 1 on invalid input.
    pub fn parse() -> Self {
        // Skip argv[0] (the binary name)
        let args: Vec<String> = std::env::args().skip(1).collect();
        Self::parse_from(&args)
    }

    /// Parse from an explicit slice. Used in unit tests.
    pub fn parse_from(args: &[String]) -> Self {
        let mut file: Option<PathBuf> = None;

        for arg in args {
            if arg.starts_with('-') {
                eprintln!("Error: Unknown option '{arg}'");
                eprintln!("Usage: allow [file.md]");
                process::exit(1);
            }

            if file.is_some() {
                eprintln!("Error: Only one file argument is supported");
                eprintln!("Usage: alloy [file.md]");
                process::exit(1);
            }

            file = Some(PathBuf::from(arg))
        }

        CliArgs { file }
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_args_gives_none_file() {
        let cli = CliArgs::parse_from(&args(&[]));
        assert!(cli.file.is_none());
    }

    #[test]
    fn one_file_arg_is_captured() {
        let cli = CliArgs::parse_from(&args(&["notes.md"]));
        assert_eq!(cli.file, Some(PathBuf::from("notes.md")));
    }

    #[test]
    fn absolute_path_is_captured() {
        let cli = CliArgs::parse_from(&args(&["/home/user/notes.md"]));
        assert_eq!(cli.file, Some(PathBuf::from("/home/user/notes.md")));
    }
}
