// Classify the root CLI without rewriting argv or executing Codex. Unknown options
// pass through untracked rather than guessing whether their values are commands.
pub fn is_local_chat<S: AsRef<std::ffi::OsStr>>(arguments: &[S]) -> bool {
    let mut arguments = arguments.iter();
    let mut prompt_seen = false;
    while let Some(argument) = arguments.next() {
        let Some(argument) = argument.as_ref().to_str() else {
            return false;
        };
        if argument == "--" {
            return true; // Remaining tokens are literal prompt text, including command names.
        }
        if argument.starts_with("--") {
            let (option, value) = argument
                .split_once('=')
                .map_or((argument, None), |(k, v)| (k, Some(v)));
            match option {
                "--config" | "--enable" | "--disable" | "--image" | "--model"
                | "--local-provider" | "--profile" | "--sandbox" | "--cd" | "--add-dir"
                | "--ask-for-approval" => {
                    if value.is_none() && arguments.next().is_none() {
                        return false;
                    }
                }
                "--oss"
                | "--strict-config"
                | "--approve-for-me"
                | "--full-auto"
                | "--dangerously-bypass-approvals-and-sandbox"
                | "--dangerously-bypass-hook-trust"
                | "--worktree"
                | "--search"
                | "--no-alt-screen"
                | "--no-daemon"
                    if value.is_none() => {}
                _ => return false, // Includes remote connections, help, and version.
            }
        } else if argument.starts_with('-') && argument != "-" {
            // These short options take one value, either attached or in the next token.
            if !matches!(
                argument.as_bytes().get(1),
                Some(b'c' | b'i' | b'm' | b'p' | b's' | b'C' | b'a')
            ) {
                return false;
            }
            if argument.len() == 2 && arguments.next().is_none() {
                return false;
            }
        } else if !prompt_seen {
            match argument {
                "resume" | "exec" | "e" | "fork" | "review" => {
                    // Global remote options may also follow a chat subcommand.
                    // Conservatively leave ambiguous remote-shaped values untracked.
                    return !arguments.take_while(|arg| arg.as_ref() != "--").any(|arg| {
                        arg.as_ref()
                            .to_str()
                            .is_some_and(|arg| arg == "--remote" || arg.starts_with("--remote="))
                    });
                }
                "agents" | "login" | "logout" | "mcp" | "mcp-server" | "plugin" | "app"
                | "app-server" | "remote-control" | "completion" | "update" | "doctor"
                | "sandbox" | "debug" | "apply" | "a" | "queue" | "archive" | "delete"
                | "migrate-rollouts" | "unarchive" | "cloud" | "exec-server" | "features"
                | "help" => return false,
                _ => prompt_seen = true,
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::is_local_chat;

    #[test]
    fn local_chat_accepts_prompts_and_known_options_without_confusing_values_with_commands() {
        for args in [
            vec![],
            vec!["."],
            vec!["explain this project"],
            vec!["--model", "review", "."],
            vec!["-mreview", "."],
            vec!["--config", "key=value", "--no-alt-screen"],
            vec!["--cd=/tmp/project with spaces", "."],
            vec!["--image", "login", "."],
            vec!["-C", "app-server"],
            vec!["--", "login"],
            vec!["--", "--remote"],
            vec!["resume", "--last"],
            vec!["exec", "-"],
            vec!["e", "prompt"],
            vec!["fork", "--last"],
            vec!["review", "--uncommitted"],
            vec!["--profile", "work", "resume", "thread"],
        ] {
            assert!(is_local_chat(&args), "{args:?}");
        }
    }

    #[test]
    fn services_remote_connections_and_unrecognized_options_remain_untracked() {
        for args in [
            vec!["login"],
            vec!["--config", "key=value", "app-server", "--stdio"],
            vec!["remote-control", "start"],
            vec!["mcp-server"],
            vec!["resume", "--remote", "unix:///tmp/socket"],
            vec!["fork", "--remote=unix:///tmp/socket"],
            vec!["features", "list"],
            vec!["--remote", "unix:///tmp/socket"],
            vec!["--remote=unix:///tmp/socket"],
            vec![".", "--remote", "unix:///tmp/socket"],
            vec!["--model", "model", "--remote", "unix:///tmp/socket"],
            vec!["--help"],
            vec!["-h"],
            vec!["--version"],
            vec!["-V"],
            vec!["--unknown-option", "exec"],
            vec!["--model"],
            vec!["-c"],
        ] {
            assert!(!is_local_chat(&args), "{args:?}");
        }
    }
}
