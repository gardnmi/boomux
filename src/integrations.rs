use std::path::Path;

pub const MAX_EXTERNAL_SESSION_ID_BYTES: usize = 256;

pub fn validate_external_session_id(value: &str) -> Result<(), &'static str> {
    if value.is_empty()
        || value.len() > MAX_EXTERNAL_SESSION_ID_BYTES
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        Err("external session ID is invalid")
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallTargetKind {
    OpenCode,
    Pi,
    Claude,
    Codex,
    Kiro,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InstallationCapability {
    pub package: &'static str,
    pub validated_version: &'static str,
    pub asset_name: &'static str,
    pub content: &'static str,
    pub executable: &'static str,
    pub reload_message: &'static str,
    pub target: InstallTargetKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TitleProvider {
    OpenCode,
    Pi,
    Codex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TitleCapability {
    pub provider: TitleProvider,
    pub provides_catalog: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResumeCapability {
    pub executable: &'static str,
    pub arguments_before_session: &'static [&'static str],
}

impl ResumeCapability {
    pub fn command(
        self,
        stored_command: &[String],
        external_session_id: &str,
    ) -> Option<Vec<String>> {
        let executable = if stored_command.is_empty() {
            self.executable.to_owned()
        } else if stored_command.len() == 1
            && Path::new(&stored_command[0])
                .file_name()
                .and_then(|name| name.to_str())
                == Some(self.executable)
        {
            stored_command[0].clone()
        } else {
            return None;
        };
        let mut command = vec![executable];
        command.extend(
            self.arguments_before_session
                .iter()
                .map(|argument| (*argument).to_owned()),
        );
        command.push(external_session_id.to_owned());
        Some(command)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ForegroundCapability {
    pub process_name: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunScopedLauncher {
    Codex,
    Kiro,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntegrationDescriptor {
    pub key: &'static str,
    pub display_name: &'static str,
    pub installation: Option<InstallationCapability>,
    pub titles: Option<TitleCapability>,
    pub resume: Option<ResumeCapability>,
    pub foreground: Option<ForegroundCapability>,
    pub run_scoped_launcher: Option<RunScopedLauncher>,
}

pub const OPENCODE: IntegrationDescriptor = IntegrationDescriptor {
    key: "opencode",
    display_name: "OpenCode",
    installation: Some(InstallationCapability {
        package: "opencode-ai",
        validated_version: "1.18.18",
        asset_name: "plugin",
        content: include_str!("../integrations/opencode/boomux.js"),
        executable: "opencode",
        reload_message: "Restart any running OpenCode process to activate the plugin",
        target: InstallTargetKind::OpenCode,
    }),
    titles: Some(TitleCapability {
        provider: TitleProvider::OpenCode,
        provides_catalog: true,
    }),
    resume: Some(ResumeCapability {
        executable: "opencode",
        arguments_before_session: &["--session"],
    }),
    foreground: Some(ForegroundCapability {
        process_name: "opencode",
    }),
    run_scoped_launcher: None,
};

pub const PI: IntegrationDescriptor = IntegrationDescriptor {
    key: "pi",
    display_name: "Pi",
    installation: Some(InstallationCapability {
        package: "@earendil-works/pi-coding-agent",
        validated_version: "0.84.1",
        asset_name: "extension",
        content: include_str!("../integrations/pi/boomux.js"),
        executable: "pi",
        reload_message: "Restart any running Pi process to activate the extension",
        target: InstallTargetKind::Pi,
    }),
    titles: Some(TitleCapability {
        provider: TitleProvider::Pi,
        provides_catalog: false,
    }),
    resume: Some(ResumeCapability {
        executable: "pi",
        arguments_before_session: &["--session"],
    }),
    foreground: Some(ForegroundCapability { process_name: "pi" }),
    run_scoped_launcher: None,
};

pub const CLAUDE: IntegrationDescriptor = IntegrationDescriptor {
    key: "claude",
    display_name: "Claude Code",
    installation: Some(InstallationCapability {
        package: "@anthropic-ai/claude-code",
        validated_version: "2.1.236",
        asset_name: "plugin manifest",
        content: include_str!("../integrations/claude/.claude-plugin/plugin.json"),
        executable: "claude",
        reload_message: "Restart any running Claude Code process to activate the plugin",
        target: InstallTargetKind::Claude,
    }),
    titles: None,
    resume: Some(ResumeCapability {
        executable: "claude",
        arguments_before_session: &["--resume"],
    }),
    foreground: Some(ForegroundCapability {
        process_name: "claude",
    }),
    run_scoped_launcher: None,
};

pub const CODEX: IntegrationDescriptor = IntegrationDescriptor {
    key: "codex",
    display_name: "Codex",
    installation: Some(InstallationCapability {
        package: "@openai/codex",
        validated_version: "0.147.0",
        asset_name: "hooks",
        content: include_str!("../integrations/codex/hooks.json"),
        executable: "codex",
        reload_message: "Restart Codex, then review and trust the Boomux hook with /hooks",
        target: InstallTargetKind::Codex,
    }),
    titles: Some(TitleCapability {
        provider: TitleProvider::Codex,
        provides_catalog: true,
    }),
    resume: Some(ResumeCapability {
        executable: "codex",
        arguments_before_session: &["resume"],
    }),
    foreground: Some(ForegroundCapability {
        process_name: "codex",
    }),
    run_scoped_launcher: Some(RunScopedLauncher::Codex),
};

pub const KIRO: IntegrationDescriptor = IntegrationDescriptor {
    key: "kiro",
    display_name: "Kiro CLI",
    installation: Some(InstallationCapability {
        package: "kiro-cli",
        validated_version: "2.18.0",
        asset_name: "hooks",
        content: include_str!("../integrations/kiro/boomux.json"),
        executable: "kiro-cli",
        reload_message: "Reopen its managed ShellRun, then start Kiro CLI in v3 mode to activate the hooks",
        target: InstallTargetKind::Kiro,
    }),
    titles: None,
    resume: Some(ResumeCapability {
        executable: "kiro-cli",
        arguments_before_session: &["--v3", "chat", "--resume-id"],
    }),
    foreground: Some(ForegroundCapability {
        process_name: "kiro-cli",
    }),
    run_scoped_launcher: Some(RunScopedLauncher::Kiro),
};

pub const ALL: &[IntegrationDescriptor] = &[OPENCODE, PI, CLAUDE, CODEX, KIRO];

pub fn by_key(key: &str) -> Option<&'static IntegrationDescriptor> {
    descriptor_by_key(ALL, key)
}

pub fn by_foreground_process(process_name: &str) -> Option<&'static IntegrationDescriptor> {
    descriptor_by_foreground_process(ALL, process_name)
}

pub fn installable() -> impl Iterator<Item = &'static IntegrationDescriptor> {
    installable_in(ALL)
}

fn descriptor_by_foreground_process<'a>(
    descriptors: &'a [IntegrationDescriptor],
    process_name: &str,
) -> Option<&'a IntegrationDescriptor> {
    descriptors.iter().find(|descriptor| {
        descriptor
            .foreground
            .is_some_and(|foreground| foreground.process_name == process_name)
    })
}

fn installable_in(
    descriptors: &[IntegrationDescriptor],
) -> impl Iterator<Item = &IntegrationDescriptor> {
    descriptors
        .iter()
        .filter(|descriptor| descriptor.installation.is_some())
}

pub fn display_name(key: &str) -> &str {
    by_key(key).map_or(key, |descriptor| descriptor.display_name)
}

fn descriptor_by_key<'a>(
    descriptors: &'a [IntegrationDescriptor],
    key: &str,
) -> Option<&'a IntegrationDescriptor> {
    descriptors.iter().find(|descriptor| descriptor.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_capabilities_are_explicit_and_discoverable() {
        const PARTIAL: IntegrationDescriptor = IntegrationDescriptor {
            key: "partial",
            display_name: "Partial Host",
            installation: None,
            titles: Some(TitleCapability {
                provider: TitleProvider::Pi,
                provides_catalog: false,
            }),
            resume: None,
            foreground: Some(ForegroundCapability {
                process_name: "partial-agent",
            }),
            run_scoped_launcher: None,
        };

        let descriptors = [OPENCODE, PARTIAL];
        let descriptor = descriptor_by_key(&descriptors, "partial").expect("partial descriptor");
        assert_eq!(descriptor.display_name, "Partial Host");
        assert!(descriptor.installation.is_none());
        assert!(descriptor.titles.is_some());
        assert!(descriptor.resume.is_none());
        assert_eq!(
            descriptor
                .foreground
                .map(|capability| capability.process_name),
            Some("partial-agent")
        );
        assert_eq!(
            descriptor_by_foreground_process(&descriptors, "partial-agent")
                .map(|descriptor| descriptor.key),
            Some("partial")
        );
        assert_eq!(
            installable_in(&descriptors)
                .map(|descriptor| descriptor.key)
                .collect::<Vec<_>>(),
            ["opencode"]
        );
    }

    #[test]
    fn production_keys_and_foreground_processes_are_unique() {
        for (index, descriptor) in ALL.iter().enumerate() {
            assert!(
                ALL[index + 1..]
                    .iter()
                    .all(|other| other.key != descriptor.key)
            );
            if let Some(foreground) = descriptor.foreground {
                assert!(ALL[index + 1..].iter().all(|other| {
                    other.foreground.map(|value| value.process_name)
                        != Some(foreground.process_name)
                }));
                assert_eq!(
                    by_foreground_process(foreground.process_name).map(|found| found.key),
                    Some(descriptor.key)
                );
            }
        }

        for descriptor in ALL {
            assert_eq!(by_key(descriptor.key), Some(descriptor));
            assert_eq!(display_name(descriptor.key), descriptor.display_name);
        }
    }

    #[test]
    fn resume_capability_preserves_exact_executable_path() {
        let resume = OPENCODE.resume.expect("resume capability");
        assert_eq!(
            resume.command(&["/usr/bin/opencode".into()], "session-1"),
            Some(vec![
                "/usr/bin/opencode".into(),
                "--session".into(),
                "session-1".into()
            ])
        );
        assert!(
            resume
                .command(&["opencode".into(), "--continue".into()], "session-1")
                .is_none()
        );
        assert_eq!(
            CLAUDE.resume.unwrap().command(&[], "session-2"),
            Some(vec!["claude".into(), "--resume".into(), "session-2".into()])
        );
        assert_eq!(
            CODEX.resume.unwrap().command(&[], "thread-literal"),
            Some(vec![
                "codex".into(),
                "resume".into(),
                "thread-literal".into()
            ])
        );
        assert_eq!(
            KIRO.resume.unwrap().command(&[], "session-3"),
            Some(vec![
                "kiro-cli".into(),
                "--v3".into(),
                "chat".into(),
                "--resume-id".into(),
                "session-3".into()
            ])
        );
    }
}
