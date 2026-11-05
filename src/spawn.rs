use anyhow::Result;
use crossterm::{
    cursor::Show,
    execute,
    terminal::{disable_raw_mode, LeaveAlternateScreen},
};
use std::io::stdout;
use std::path::Path;
use std::process::Command;

use crate::config::Profile;

/// Build the `Command` configured with optional profile environment variables, working directory, auto_mode, and args
pub fn build_claude_command(
    profile: Option<&Profile>,
    target_dir: &Path,
    auto_mode: bool,
    extra_args: &[String],
) -> Command {
    let mut cmd = Command::new("claude");
    cmd.current_dir(target_dir);

    // If auto_mode is enabled, append auto flag if not already specified by user
    if auto_mode {
        let has_auto_flag = extra_args.iter().any(|a| {
            a == "--dangerously-skip-permissions" || a == "-a" || a == "--auto"
        });
        if !has_auto_flag {
            cmd.arg("--dangerously-skip-permissions");
        }
    }

    if !extra_args.is_empty() {
        cmd.args(extra_args);
    }

    if let Some(p) = profile {
        if let Some(v) = &p.api_key {
            if !v.trim().is_empty() {
                cmd.env("ANTHROPIC_API_KEY", v);
            }
        }
        if let Some(v) = &p.base_url {
            if !v.trim().is_empty() {
                cmd.env("ANTHROPIC_BASE_URL", v);
            }
        }
        if let Some(v) = &p.fable_model {
            if !v.trim().is_empty() {
                cmd.env("ANTHROPIC_DEFAULT_FABLE_MODEL", v);
            }
        }
        if let Some(v) = &p.opus_model {
            if !v.trim().is_empty() {
                cmd.env("ANTHROPIC_DEFAULT_OPUS_MODEL", v);
            }
        }
        if let Some(v) = &p.sonnet_model {
            if !v.trim().is_empty() {
                cmd.env("ANTHROPIC_DEFAULT_SONNET_MODEL", v);
            }
        }
        if let Some(v) = &p.haiku_model {
            if !v.trim().is_empty() {
                cmd.env("ANTHROPIC_DEFAULT_HAIKU_MODEL", v);
            }
        }
    }

    cmd
}

/// Teardown the TUI terminal and spawn the `claude` CLI process.
/// Exits the `ccat` process with the exit code returned by `claude`.
pub fn use_profile(
    profile: Option<&Profile>,
    target_dir: &Path,
    auto_mode: bool,
    extra_args: &[String],
) -> Result<()> {
    // Teardown raw mode and leave alternate screen so claude has full interactive TTY control
    let _ = disable_raw_mode();
    let _ = execute!(stdout(), LeaveAlternateScreen, Show);

    let mut cmd = build_claude_command(profile, target_dir, auto_mode, extra_args);

    let status = match cmd.status() {
        Ok(status) => status,
        Err(e) => {
            eprintln!("Error executing `claude`: {}", e);
            eprintln!("Make sure `claude` CLI is installed and available in your PATH.");
            std::process::exit(1);
        }
    };

    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_build_claude_command_with_auto_mode() {
        let target_dir = PathBuf::from("/tmp");
        let extra_args = vec!["-p".to_string(), "hello".to_string()];

        let cmd = build_claude_command(None, &target_dir, true, &extra_args);
        assert_eq!(cmd.get_program(), "claude");

        let args: Vec<&str> = cmd.get_args().map(|s| s.to_str().unwrap()).collect();
        assert_eq!(args, vec!["--dangerously-skip-permissions", "-p", "hello"]);
    }

    #[test]
    fn test_build_claude_command_with_profile() {
        let profile = Profile {
            api_key: Some("sk-test-key".to_string()),
            base_url: Some("http://localhost:20128".to_string()),
            fable_model: Some("claude-fable-5[1m]".to_string()),
            opus_model: Some("claude-opus-5[1m]".to_string()),
            sonnet_model: Some("claude-sonnet-5".to_string()),
            haiku_model: Some("claude-haiku-5".to_string()),
        };

        let target_dir = PathBuf::from("/tmp");
        let cmd = build_claude_command(Some(&profile), &target_dir, false, &[]);

        let envs: Vec<(&str, Option<&str>)> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_str().unwrap(), v.map(|s| s.to_str().unwrap())))
            .collect();

        assert!(envs.contains(&("ANTHROPIC_API_KEY", Some("sk-test-key"))));
        assert!(envs.contains(&("ANTHROPIC_BASE_URL", Some("http://localhost:20128"))));
    }
}
