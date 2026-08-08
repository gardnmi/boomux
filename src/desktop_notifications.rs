use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::daemon::NotificationSettings;

const MAX_CONTEXT_BYTES: usize = 120;
const NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(2);
const NOTIFICATION_QUEUE_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum NotificationReason {
    Blocked,
    Completed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NotificationRequest {
    pub(crate) reason: NotificationReason,
    pub(crate) agent: String,
    pub(crate) workspace: String,
    pub(crate) shell: String,
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
    pub(crate) fn new() -> Self {
        let (sender, receiver) = sync_channel(NOTIFICATION_QUEUE_CAPACITY);
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_stopping = stopping.clone();
        let worker = thread::Builder::new()
            .name("boomux-notifications".into())
            .spawn(move || {
                while let Ok(request) = receiver.recv()
                    && !worker_stopping.load(Ordering::Acquire)
                {
                    deliver(request, &worker_stopping);
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

fn deliver(request: NotificationRequest, stopping: &AtomicBool) {
    let argv = notify_send_argv(&request);
    if let Ok(mut child) = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        let deadline = Instant::now() + NOTIFICATION_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if stopping.load(Ordering::Acquire) || Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                Ok(None) => thread::sleep(Duration::from_millis(10)),
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
    }
}

pub(crate) fn category_enabled(settings: NotificationSettings, reason: NotificationReason) -> bool {
    settings.enabled
        && match reason {
            NotificationReason::Blocked => settings.blocked,
            NotificationReason::Completed => settings.completed,
        }
}

fn notify_send_argv(request: &NotificationRequest) -> Vec<String> {
    let reason = match request.reason {
        NotificationReason::Blocked => "blocked",
        NotificationReason::Completed => "completed",
    };
    vec![
        "notify-send".into(),
        "--app-name".into(),
        "Boomux".into(),
        format!("Boomux Agent {reason}"),
        format!(
            "{} in workspace {}, shell {}. Open Boomux or run `boomux attention list`.",
            sanitize(&request.agent),
            sanitize(&request.workspace),
            sanitize(&request.shell)
        ),
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
    fn category_filtering_requires_global_and_category_flags() {
        let enabled = NotificationSettings {
            enabled: true,
            blocked: true,
            completed: false,
        };
        assert!(category_enabled(enabled, NotificationReason::Blocked));
        assert!(!category_enabled(enabled, NotificationReason::Completed));
        assert!(!category_enabled(
            NotificationSettings::default(),
            NotificationReason::Blocked
        ));
    }
}
