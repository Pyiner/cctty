use std::process::Command;

const CURRENT_CLAUDE_HELP_FLAGS: &[&str] = &[
    "--add-dir",
    "--agent",
    "--agents",
    "--allow-dangerously-skip-permissions",
    "--allowedTools",
    "--allowed-tools",
    "--append-system-prompt",
    "--bare",
    "--betas",
    "--brief",
    "--chrome",
    "-c",
    "--continue",
    "--dangerously-skip-permissions",
    "-d",
    "--debug",
    "--debug-file",
    "--disable-slash-commands",
    "--disallowedTools",
    "--disallowed-tools",
    "--effort",
    "--exclude-dynamic-system-prompt-sections",
    "--fallback-model",
    "--file",
    "--fork-session",
    "--from-pr",
    "-h",
    "--help",
    "--ide",
    "--include-hook-events",
    "--include-partial-messages",
    "--input-format",
    "--json-schema",
    "--max-budget-usd",
    "--mcp-config",
    "--mcp-debug",
    "--model",
    "-n",
    "--name",
    "--no-chrome",
    "--no-session-persistence",
    "--output-format",
    "--permission-mode",
    "--plugin-dir",
    "--plugin-url",
    "-p",
    "--print",
    "--remote-control",
    "--remote-control-session-name-prefix",
    "--replay-user-messages",
    "-r",
    "--resume",
    "--session-id",
    "--setting-sources",
    "--settings",
    "--strict-mcp-config",
    "--system-prompt",
    "--tmux",
    "--tools",
    "--verbose",
    "-v",
    "--version",
    "-w",
    "--worktree",
];

const HIDDEN_SDK_AND_NATIVE_FLAGS: &[&str] = &[
    "--advisor",
    "--agent-color",
    "--agent-id",
    "--agent-name",
    "--agent-type",
    "--append-system-prompt-file",
    "--channels",
    "--cowork",
    "--dangerously-load-development-channels",
    "--deep-link-cwd-b64",
    "--deep-link-last-fetch",
    "--deep-link-origin",
    "--deep-link-repo",
    "--enable-auth-status",
    "--enable-auto-mode",
    "--init",
    "--init-only",
    "--maintenance",
    "--managed-settings",
    "--max-thinking-tokens",
    "--max-turns",
    "--parent-session-id",
    "--permission-prompt-tool",
    "--plan-mode-instructions",
    "--plan-mode-required",
    "--prefill",
    "--prefill-b64",
    "--rc",
    "--remote",
    "--resume-session-at",
    "--rewind-files",
    "--sdk-url",
    "--session-mirror",
    "--system-prompt-file",
    "--task-budget",
    "--team-name",
    "--teammate-mode",
    "--teleport",
    "--thinking",
    "--thinking-display",
    "--workload",
    "--xaa",
];

#[test]
fn readme_compatibility_matrix_mentions_every_captured_claude_help_flag() {
    let readme = include_str!("../README.md");
    assert!(
        readme.contains("## Compatibility Matrix"),
        "README must keep an explicit compatibility matrix"
    );
    for flag in CURRENT_CLAUDE_HELP_FLAGS {
        assert!(
            readme.contains(&format!("`{flag}`")),
            "README compatibility matrix does not mention {flag}"
        );
    }
}

#[test]
fn readme_compatibility_matrix_mentions_hidden_sdk_and_native_flags() {
    let readme = include_str!("../README.md");
    for flag in HIDDEN_SDK_AND_NATIVE_FLAGS {
        assert!(
            readme.contains(&format!("`{flag}`")) || readme.contains(&format!("`{flag},")),
            "README hidden compatibility matrix does not mention {flag}"
        );
    }
}

#[test]
#[ignore = "checks local installed claude --help against README; run when updating Claude"]
fn live_claude_help_flags_are_documented() {
    if std::env::var("CCTTY_LIVE_HELP_COVERAGE").ok().as_deref() != Some("1") {
        eprintln!("set CCTTY_LIVE_HELP_COVERAGE=1 to compare local claude --help");
        return;
    }

    let output = Command::new("claude")
        .arg("--help")
        .output()
        .expect("failed to run claude --help");
    assert!(
        output.status.success(),
        "claude --help failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let help = String::from_utf8(output.stdout).unwrap();
    let readme = include_str!("../README.md");
    for flag in parse_help_flags(&help) {
        assert!(
            readme.contains(&format!("`{flag}`")),
            "README compatibility matrix does not mention current claude flag {flag}"
        );
    }
}

fn parse_help_flags(help: &str) -> Vec<String> {
    let mut flags = Vec::new();
    let mut in_options = false;
    for line in help.lines() {
        if line.trim() == "Options:" {
            in_options = true;
            continue;
        }
        if line.trim() == "Commands:" {
            break;
        }
        if !in_options || !line.trim_start().starts_with('-') {
            continue;
        }
        let declaration = option_declaration(line.trim_start());
        for token in declaration.split([',', ' ']) {
            let token = token.trim();
            if token.starts_with('-') {
                flags.push(token.to_owned());
            }
        }
    }
    flags.sort();
    flags.dedup();
    flags
}

fn option_declaration(line: &str) -> String {
    let mut declaration = String::new();
    let mut spaces = 0;
    for ch in line.chars() {
        if ch == ' ' {
            spaces += 1;
            if spaces >= 2 {
                break;
            }
        } else {
            spaces = 0;
        }
        declaration.push(ch);
    }
    declaration.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_help_flags_extracts_aliases_without_values() {
        let help = "\
Options:
  --allowedTools, --allowed-tools <tools...>        Comma-separated
  -p, --print                                       Print response
Commands:
";
        assert_eq!(
            parse_help_flags(help),
            vec!["--allowed-tools", "--allowedTools", "--print", "-p"]
        );
    }
}
