use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallTargetKind {
    OpenCode,
    Pi,
    Claude,
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TitleCapability {
    pub provider: TitleProvider,
    pub provides_catalog: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResumeCapability {
    pub executable: &'static str,
    pub session_argument: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptTransport {
    Argument,
    Stdin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScheduleDispatchCapability {
    pub fresh: bool,
    pub continuation: bool,
    pub prompt_transport: PromptTransport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleDispatchCommand {
    pub argv: Vec<String>,
    pub stdin: Option<Vec<u8>>,
}

impl ScheduleDispatchCapability {
    pub fn command(
        self,
        integration: &str,
        session: &crate::protocol::AgentScheduleSession,
        prompt: &str,
    ) -> Option<ScheduleDispatchCommand> {
        use crate::protocol::AgentScheduleSession;

        let mut argv = vec![integration.to_owned()];
        match (integration, session) {
            ("opencode", AgentScheduleSession::Fresh) if self.fresh => {
                argv.extend(["run".into(), "--".into(), prompt.into()]);
            }
            (
                "opencode",
                AgentScheduleSession::Continue {
                    external_session_id,
                },
            ) if self.continuation => {
                argv.extend([
                    "run".into(),
                    "--session".into(),
                    external_session_id.clone(),
                    "--".into(),
                    prompt.into(),
                ]);
            }
            ("pi", AgentScheduleSession::Fresh) if self.fresh => {
                argv.push("--print".into());
            }
            (
                "pi",
                AgentScheduleSession::Continue {
                    external_session_id,
                },
            ) if self.continuation => {
                argv.extend([
                    "--session".into(),
                    external_session_id.clone(),
                    "--print".into(),
                ]);
            }
            ("claude", AgentScheduleSession::Fresh) if self.fresh => {
                argv.push("--print".into());
            }
            (
                "claude",
                AgentScheduleSession::Continue {
                    external_session_id,
                },
            ) if self.continuation => {
                argv.extend([
                    "--print".into(),
                    "--resume".into(),
                    external_session_id.clone(),
                ]);
            }
            _ => return None,
        }
        Some(ScheduleDispatchCommand {
            argv,
            stdin: (self.prompt_transport == PromptTransport::Stdin)
                .then(|| prompt.as_bytes().to_vec()),
        })
    }
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
        Some(vec![
            executable,
            self.session_argument.to_owned(),
            external_session_id.to_owned(),
        ])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ForegroundCapability {
    pub process_name: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntegrationDescriptor {
    pub key: &'static str,
    pub display_name: &'static str,
    pub installation: Option<InstallationCapability>,
    pub titles: Option<TitleCapability>,
    pub resume: Option<ResumeCapability>,
    pub schedule_dispatch: Option<ScheduleDispatchCapability>,
    pub foreground: Option<ForegroundCapability>,
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
        session_argument: "--session",
    }),
    schedule_dispatch: Some(ScheduleDispatchCapability {
        fresh: true,
        continuation: true,
        prompt_transport: PromptTransport::Argument,
    }),
    foreground: Some(ForegroundCapability {
        process_name: "opencode",
    }),
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
        session_argument: "--session",
    }),
    schedule_dispatch: Some(ScheduleDispatchCapability {
        fresh: true,
        continuation: true,
        prompt_transport: PromptTransport::Stdin,
    }),
    foreground: Some(ForegroundCapability { process_name: "pi" }),
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
        session_argument: "--resume",
    }),
    schedule_dispatch: Some(ScheduleDispatchCapability {
        fresh: true,
        continuation: true,
        prompt_transport: PromptTransport::Stdin,
    }),
    foreground: Some(ForegroundCapability {
        process_name: "claude",
    }),
};

pub const ALL: &[IntegrationDescriptor] = &[OPENCODE, PI, CLAUDE];

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
            schedule_dispatch: None,
            foreground: Some(ForegroundCapability {
                process_name: "partial-agent",
            }),
        };

        let descriptors = [OPENCODE, PARTIAL];
        let descriptor = descriptor_by_key(&descriptors, "partial").expect("partial descriptor");
        assert_eq!(descriptor.display_name, "Partial Host");
        assert!(descriptor.installation.is_none());
        assert!(descriptor.titles.is_some());
        assert!(descriptor.resume.is_none());
        assert!(descriptor.schedule_dispatch.is_none());
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
            let dispatch = descriptor.schedule_dispatch.expect("dispatch capability");
            assert!(dispatch.fresh);
            assert!(dispatch.continuation);
        }
    }

    #[test]
    fn resume_capability_does_not_imply_schedule_dispatch() {
        const RESUME_ONLY: IntegrationDescriptor = IntegrationDescriptor {
            key: "resume-only",
            display_name: "Resume Only",
            installation: None,
            titles: None,
            resume: Some(ResumeCapability {
                executable: "resume-only",
                session_argument: "--session",
            }),
            schedule_dispatch: None,
            foreground: None,
        };

        assert!(RESUME_ONLY.resume.is_some());
        assert!(RESUME_ONLY.schedule_dispatch.is_none());
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
    }

    #[test]
    fn scheduled_dispatch_builds_exact_host_argv_and_private_transport() {
        use crate::protocol::AgentScheduleSession;

        let prompt = "-private @prompt";
        assert_eq!(
            OPENCODE
                .schedule_dispatch
                .unwrap()
                .command("opencode", &AgentScheduleSession::Fresh, prompt)
                .unwrap(),
            ScheduleDispatchCommand {
                argv: vec!["opencode".into(), "run".into(), "--".into(), prompt.into()],
                stdin: None,
            }
        );
        assert_eq!(
            PI.schedule_dispatch
                .unwrap()
                .command(
                    "pi",
                    &AgentScheduleSession::Continue {
                        external_session_id: "exact-full-id".into(),
                    },
                    prompt,
                )
                .unwrap(),
            ScheduleDispatchCommand {
                argv: vec![
                    "pi".into(),
                    "--session".into(),
                    "exact-full-id".into(),
                    "--print".into(),
                ],
                stdin: Some(prompt.as_bytes().to_vec()),
            }
        );
        assert_eq!(
            CLAUDE
                .schedule_dispatch
                .unwrap()
                .command(
                    "claude",
                    &AgentScheduleSession::Continue {
                        external_session_id: "exact-id".into(),
                    },
                    prompt,
                )
                .unwrap(),
            ScheduleDispatchCommand {
                argv: vec![
                    "claude".into(),
                    "--print".into(),
                    "--resume".into(),
                    "exact-id".into(),
                ],
                stdin: Some(prompt.as_bytes().to_vec()),
            }
        );
        assert_eq!(
            CLAUDE
                .schedule_dispatch
                .unwrap()
                .command("claude", &AgentScheduleSession::Fresh, prompt)
                .unwrap(),
            ScheduleDispatchCommand {
                argv: vec!["claude".into(), "--print".into()],
                stdin: Some(prompt.as_bytes().to_vec()),
            }
        );
    }
}
