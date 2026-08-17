use std::io;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::daemon::NotificationDeliverySettings;

const MAX_CONTEXT_BYTES: usize = 120;
const NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(2);
const NOTIFICATION_QUEUE_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum NotificationReason {
    Blocked,
    Completed,
    ScheduledDispatchFailed,
    ScheduledInterrupted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NotificationRequest {
    pub(crate) reason: NotificationReason,
    pub(crate) agent: String,
    pub(crate) workspace: String,
    pub(crate) shell: String,
    pub(crate) node: Option<NotificationNodeContext>,
    pub(crate) digest: Option<NotificationDigest>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NotificationNodeContext {
    pub(crate) alias: String,
    pub(crate) node_id: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct NotificationDigest {
    pub(crate) blocked: u16,
    pub(crate) completed: u16,
    pub(crate) scheduled_dispatch_failed: u16,
    pub(crate) scheduled_interrupted: u16,
}

pub(crate) trait NotificationSink: Send + Sync {
    fn notify(&self, request: NotificationRequest);
}

pub(crate) struct DesktopNotificationSink {
    sender: Option<SyncSender<NotificationRequest>>,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

pub(crate) struct DisabledNotificationSink;

impl DesktopNotificationSink {
    pub(crate) fn new(settings: NotificationDeliverySettings) -> Self {
        let (sender, receiver) = sync_channel(NOTIFICATION_QUEUE_CAPACITY);
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_stopping = stopping.clone();
        let worker = thread::Builder::new()
            .name("boomux-notifications".into())
            .spawn(move || {
                while let Ok(request) = receiver.recv()
                    && !worker_stopping.load(Ordering::Acquire)
                {
                    deliver(request, &settings, &worker_stopping);
                }
            })
            .ok();
        Self {
            sender: Some(sender),
            stopping,
            worker,
        }
    }
}

impl Drop for DesktopNotificationSink {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl NotificationSink for DisabledNotificationSink {
    fn notify(&self, _request: NotificationRequest) {}
}

impl NotificationSink for DesktopNotificationSink {
    fn notify(&self, request: NotificationRequest) {
        if let Some(sender) = self.sender.as_ref() {
            let _ = sender.try_send(request);
        }
    }
}

fn deliver(
    request: NotificationRequest,
    settings: &NotificationDeliverySettings,
    stopping: &AtomicBool,
) {
    if settings.desktop.enabled {
        let _ = run_bounded(notify_send_argv(&request), stopping);
    }
    if settings.sound.enabled && !stopping.load(Ordering::Acquire) {
        let _ = run_bounded(sound_argv(settings, request.reason), stopping);
    }
}

fn run_bounded(argv: Vec<String>, stopping: &AtomicBool) -> io::Result<()> {
    if stopping.load(Ordering::Acquire) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "notification delivery stopped",
        ));
    }
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + NOTIFICATION_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(io::Error::other(format!(
                    "{} exited with {status}",
                    argv[0]
                )));
            }
            Ok(None) if stopping.load(Ordering::Acquire) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "notification delivery stopped",
                ));
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{} timed out", argv[0]),
                ));
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
    }
}

pub(crate) fn test_delivery(
    settings: &NotificationDeliverySettings,
    reason: NotificationReason,
) -> io::Result<()> {
    if !category_enabled(settings, reason) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the requested notification category has no enabled delivery channel",
        ));
    }
    let request = NotificationRequest {
        reason,
        agent: "Test Agent".into(),
        workspace: "test-workspace".into(),
        shell: "test-shell".into(),
        node: None,
        digest: None,
    };
    let stopping = AtomicBool::new(false);
    let mut first_error = None;
    if settings.desktop.enabled
        && let Err(error) = run_bounded(notify_send_argv(&request), &stopping)
    {
        first_error = Some(error);
    }
    if settings.sound.enabled
        && let Err(error) = run_bounded(sound_argv(settings, reason), &stopping)
        && first_error.is_none()
    {
        first_error = Some(error);
    }
    first_error.map_or(Ok(()), Err)
}

pub(crate) fn category_enabled(
    settings: &NotificationDeliverySettings,
    reason: NotificationReason,
) -> bool {
    (settings.desktop.enabled || settings.sound.enabled)
        && match reason {
            NotificationReason::Blocked => settings.desktop.blocked,
            NotificationReason::Completed => settings.desktop.completed,
            NotificationReason::ScheduledDispatchFailed => {
                settings.desktop.scheduled_dispatch_failed
            }
            NotificationReason::ScheduledInterrupted => settings.desktop.scheduled_interrupted,
        }
}

fn sound_argv(settings: &NotificationDeliverySettings, reason: NotificationReason) -> Vec<String> {
    let event = match reason {
        NotificationReason::Blocked => &settings.sound.blocked,
        NotificationReason::Completed => &settings.sound.completed,
        NotificationReason::ScheduledDispatchFailed => &settings.sound.scheduled_dispatch_failed,
        NotificationReason::ScheduledInterrupted => &settings.sound.scheduled_interrupted,
    };
    vec![
        "canberra-gtk-play".into(),
        "--id".into(),
        event.clone(),
        "--description".into(),
        "Boomux Agent notification".into(),
    ]
}

fn notify_send_argv(request: &NotificationRequest) -> Vec<String> {
    let (title, mut body) = if let Some(digest) = &request.digest {
        let node = request
            .node
            .as_ref()
            .expect("remote notification digest requires Node context");
        (
            "Boomux remote activity".into(),
            format!(
                "Node {} ({}) reconnected: {} blocked, {} completed, {} dispatch failed, {} interrupted. Open Boomux to inspect.",
                sanitize(&node.alias),
                sanitize(&node.node_id),
                digest.blocked,
                digest.completed,
                digest.scheduled_dispatch_failed,
                digest.scheduled_interrupted,
            ),
        )
    } else {
        match request.reason {
            NotificationReason::Blocked => (
                "Boomux Agent blocked".into(),
                format!(
                    "{} in workspace {}, shell {}. Open Boomux or run `boomux attention list`.",
                    sanitize(&request.agent),
                    sanitize(&request.workspace),
                    sanitize(&request.shell)
                ),
            ),
            NotificationReason::Completed => (
                "Boomux Agent completed".into(),
                format!(
                    "{} in workspace {}, shell {}. Open Boomux or run `boomux attention list`.",
                    sanitize(&request.agent),
                    sanitize(&request.workspace),
                    sanitize(&request.shell)
                ),
            ),
            NotificationReason::ScheduledDispatchFailed => (
                "Boomux scheduled dispatch failed".into(),
                format!(
                    "Schedule {} in workspace {} failed for execution {}. Run `boomux execution inspect {}`.",
                    sanitize(&request.agent),
                    sanitize(&request.workspace),
                    sanitize(&request.shell),
                    sanitize(&request.shell)
                ),
            ),
            NotificationReason::ScheduledInterrupted => (
                "Boomux scheduled execution interrupted".into(),
                format!(
                    "Schedule {} in workspace {} was interrupted for execution {}. Run `boomux execution inspect {}`.",
                    sanitize(&request.agent),
                    sanitize(&request.workspace),
                    sanitize(&request.shell),
                    sanitize(&request.shell)
                ),
            ),
        }
    };
    if request.digest.is_none()
        && let Some(node) = &request.node
    {
        body.push_str(&format!(
            " Node {} ({}).",
            sanitize(&node.alias),
            sanitize(&node.node_id)
        ));
    }
    vec![
        "notify-send".into(),
        "--app-name".into(),
        "Boomux".into(),
        title,
        body,
    ]
}

fn sanitize(value: &str) -> String {
    let value = value
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '<' | '>' | '&' | '\u{061c}'
                        | '\u{200b}'..='\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2060}'..='\u{2069}'
                        | '\u{feff}'
                )
            {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut sanitized = String::new();
    for character in value.chars() {
        if sanitized.len() + character.len_utf8() > MAX_CONTEXT_BYTES {
            break;
        }
        sanitized.push(character);
    }
    if sanitized.is_empty() {
        sanitized.push_str("unnamed");
    }
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> NotificationRequest {
        NotificationRequest {
            reason: NotificationReason::Blocked,
            agent: "OpenCode".into(),
            workspace: "boomux".into(),
            shell: "tests".into(),
            node: None,
            digest: None,
        }
    }

    #[test]
    fn builds_exact_notify_send_arguments() {
        assert_eq!(
            notify_send_argv(&request()),
            [
                "notify-send",
                "--app-name",
                "Boomux",
                "Boomux Agent blocked",
                "OpenCode in workspace boomux, shell tests. Open Boomux or run `boomux attention list`.",
            ]
        );
    }

    #[test]
    fn sanitizes_and_limits_untrusted_context() {
        let mut request = request();
        request.agent = format!(" secret\n\t{}", "x".repeat(200));
        request.workspace = "<b>spoof</b>\u{061c}\u{202e}".into();
        let arguments = notify_send_argv(&request);
        assert!(!arguments[4].contains('\n'));
        assert!(!arguments[4].contains('\t'));
        assert!(!arguments[4].contains(&"x".repeat(121)));
        assert!(!arguments[4].contains(['<', '>', '&', '\u{061c}', '\u{202e}']));
    }

    #[test]
    fn body_contains_no_private_agent_fields() {
        let arguments = notify_send_argv(&request());
        for private in [
            "evidence",
            "cwd",
            "agent-id",
            "external-session",
            "argv",
            "transcript",
        ] {
            assert!(!arguments[4].contains(private));
        }
    }

    #[test]
    fn remote_context_and_digest_are_bounded_and_prompt_free() {
        let mut individual = request();
        individual.node = Some(NotificationNodeContext {
            alias: "work\nnode".into(),
            node_id: "00000000-0000-0000-0000-000000000002".into(),
        });
        let arguments = notify_send_argv(&individual);
        assert!(arguments[4].contains("Node work node (00000000-0000-0000-0000-000000000002)"));

        let digest = NotificationRequest {
            reason: NotificationReason::Blocked,
            agent: String::new(),
            workspace: String::new(),
            shell: String::new(),
            node: individual.node,
            digest: Some(NotificationDigest {
                blocked: 2,
                completed: 3,
                scheduled_dispatch_failed: 4,
                scheduled_interrupted: 5,
            }),
        };
        let arguments = notify_send_argv(&digest);
        assert_eq!(arguments[3], "Boomux remote activity");
        assert!(arguments[4].contains("2 blocked, 3 completed, 4 dispatch failed, 5 interrupted"));
        assert!(!arguments[4].contains("prompt"));
    }

    #[test]
    fn category_filtering_requires_global_and_category_flags() {
        let enabled = NotificationDeliverySettings {
            desktop: crate::daemon::NotificationSettings {
                enabled: true,
                blocked: true,
                completed: false,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(category_enabled(&enabled, NotificationReason::Blocked));
        assert!(!category_enabled(&enabled, NotificationReason::Completed));
        assert!(!category_enabled(
            &NotificationDeliverySettings::default(),
            NotificationReason::Blocked
        ));

        let sound_only = NotificationDeliverySettings {
            sound: crate::daemon::NotificationSoundSettings {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(category_enabled(&sound_only, NotificationReason::Blocked));
    }

    #[test]
    fn builds_exact_sound_arguments_for_each_reason() {
        let settings = NotificationDeliverySettings {
            sound: crate::daemon::NotificationSoundSettings {
                enabled: true,
                blocked: "dialog-warning".into(),
                completed: "complete".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            sound_argv(&settings, NotificationReason::Blocked),
            [
                "canberra-gtk-play",
                "--id",
                "dialog-warning",
                "--description",
                "Boomux Agent notification",
            ]
        );
        assert_eq!(
            sound_argv(&settings, NotificationReason::Completed),
            [
                "canberra-gtk-play",
                "--id",
                "complete",
                "--description",
                "Boomux Agent notification",
            ]
        );
    }
}
