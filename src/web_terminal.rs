use std::io;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use boomux::client::{self, Client};
use boomux::protocol::{AttachFrame, ErrorCode, MAX_ATTACH_FRAME, TerminalProfile};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

const DAEMON_EVENT_QUEUE: usize = 8;
const RECONNECT_ATTEMPTS: usize = 50;
const RECONNECT_DELAY: Duration = Duration::from_millis(100);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const DAEMON_WRITE_TIMEOUT: Duration = Duration::from_millis(100);
const RECONNECT_ACK_TIMEOUT: Duration = Duration::from_secs(1);
const ATTACH_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub(crate) struct Grant {
    pub(crate) shell_id: String,
    pub(crate) run_id: String,
    pub(crate) profile: TerminalProfile,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ClientMessage {
    Resize {
        rows: u16,
        cols: u16,
        pixel_width: u16,
        pixel_height: u16,
    },
    Focus,
    Detach,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage<'a> {
    Attached {
        warning: Option<&'a str>,
        rows: u16,
        cols: u16,
    },
    Resize {
        rows: u16,
        cols: u16,
    },
    Reconnecting,
    Closed {
        reason: &'a str,
    },
    Error {
        code: &'a str,
        message: &'a str,
    },
}

enum DaemonEvent {
    Output(Vec<u8>),
    Resize { rows: u16, cols: u16 },
    Reconnect,
    Detached,
    Failed,
}

enum AttachmentOutcome {
    Reconnect,
    Closed,
}

enum WriterCommand {
    Frame(AttachFrame),
    ReconnectAck(std_mpsc::SyncSender<()>),
}

pub(crate) async fn run(socket: WebSocket, client: Client, grant: Grant) {
    let (mut sender, mut receiver) = socket.split();
    loop {
        let cancellation = Arc::new(AtomicBool::new(false));
        let attachment =
            match attach_while_connected(&mut receiver, &client, &grant, Arc::clone(&cancellation))
                .await
            {
                None => return,
                Some(Ok(attachment)) => attachment,
                Some(Err(error)) => {
                    let (code, message) = attachment_error(&error);
                    send_server_message(&mut sender, ServerMessage::Error { code, message }).await;
                    return;
                }
            };
        if !send_server_message(
            &mut sender,
            ServerMessage::Attached {
                warning: attachment.warning.as_deref(),
                rows: attachment
                    .profile
                    .as_ref()
                    .map_or(grant.profile.rows, |profile| profile.rows),
                cols: attachment
                    .profile
                    .as_ref()
                    .map_or(grant.profile.cols, |profile| profile.cols),
            },
        )
        .await
            || !send_message(
                &mut sender,
                Message::Binary(attachment.reconstruction.into()),
            )
            .await
        {
            return;
        }

        match bridge_attachment(&mut sender, &mut receiver, attachment.stream).await {
            AttachmentOutcome::Closed => return,
            AttachmentOutcome::Reconnect => {
                if !send_server_message(&mut sender, ServerMessage::Reconnecting).await {
                    return;
                }
            }
        }
    }
}

async fn attach_while_connected<R>(
    receiver: &mut R,
    client: &Client,
    grant: &Grant,
    cancellation: Arc<AtomicBool>,
) -> Option<client::Result<client::Attachment>>
where
    R: futures_util::Stream<Item = Result<Message, axum::Error>> + Unpin,
{
    let attachment = attach_exact(client, grant, Arc::clone(&cancellation));
    tokio::pin!(attachment);
    loop {
        tokio::select! {
            result = &mut attachment => return Some(result),
            message = receiver.next() => match message {
                Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                    cancellation.store(true, Ordering::Release);
                    let _ = attachment.await;
                    return None;
                }
                Some(Ok(Message::Binary(_))) | Some(Ok(Message::Text(_))) => {}
            }
        }
    }
}

async fn attach_exact(
    client: &Client,
    grant: &Grant,
    cancellation: Arc<AtomicBool>,
) -> client::Result<client::Attachment> {
    let mut last_error = None;
    for _ in 0..RECONNECT_ATTEMPTS {
        if cancellation.load(Ordering::Acquire) {
            return Err(cancelled_error());
        }
        let client = client.clone();
        let grant = grant.clone();
        let attempt_cancellation = Arc::clone(&cancellation);
        match tokio::task::spawn_blocking(move || {
            let result = client.attach_collaborative_exact_run_with_timeout(
                grant.shell_id,
                grant.run_id,
                grant.profile,
                ATTACH_TIMEOUT,
            );
            if attempt_cancellation.load(Ordering::Acquire) {
                return match result {
                    Ok(mut attachment) => {
                        let _ = AttachFrame::Detached.write_to(&mut attachment.stream);
                        Err(cancelled_error())
                    }
                    Err(error) => Err(error),
                };
            }
            result
        })
        .await
        {
            Ok(Ok(attachment)) => return Ok(attachment),
            Ok(Err(error)) if exact_run_ended(&error) => return Err(error),
            Ok(Err(error)) => last_error = Some(error),
            Err(_) => {
                return Err(client::ClientError::Transport(io::Error::other(
                    "terminal attachment task failed",
                )));
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
    Err(last_error.unwrap_or_else(|| {
        client::ClientError::Transport(io::Error::new(
            io::ErrorKind::TimedOut,
            "terminal attachment did not reconnect",
        ))
    }))
}

async fn bridge_attachment<S, R>(
    sender: &mut S,
    receiver: &mut R,
    stream: UnixStream,
) -> AttachmentOutcome
where
    S: futures_util::Sink<Message, Error = axum::Error> + Unpin,
    R: futures_util::Stream<Item = Result<Message, axum::Error>> + Unpin,
{
    let reader_stream = match stream.try_clone() {
        Ok(stream) => stream,
        Err(_) => return AttachmentOutcome::Closed,
    };
    let writer_stream = match stream.try_clone() {
        Ok(stream) => stream,
        Err(_) => return AttachmentOutcome::Closed,
    };
    if writer_stream
        .set_write_timeout(Some(DAEMON_WRITE_TIMEOUT))
        .is_err()
    {
        return AttachmentOutcome::Closed;
    }
    let (event_tx, mut event_rx) = mpsc::channel(DAEMON_EVENT_QUEUE);
    let (input_tx, input_rx) = std_mpsc::sync_channel(DAEMON_EVENT_QUEUE);
    let reader = spawn_reader(reader_stream, event_tx);
    let writer = spawn_writer(writer_stream, input_rx);

    let outcome = loop {
        tokio::select! {
            event = event_rx.recv() => match event {
                Some(DaemonEvent::Output(bytes)) => {
                    if !send_message(sender, Message::Binary(bytes.into())).await {
                        break AttachmentOutcome::Closed;
                    }
                }
                Some(DaemonEvent::Resize { rows, cols }) => {
                    if !send_server_message(sender, ServerMessage::Resize { rows, cols }).await {
                        break AttachmentOutcome::Closed;
                    }
                }
                Some(DaemonEvent::Reconnect) => {
                    if acknowledge_reconnect(&input_tx).await {
                        break AttachmentOutcome::Reconnect;
                    }
                    break AttachmentOutcome::Closed;
                }
                Some(DaemonEvent::Detached) => {
                    send_server_message(sender, ServerMessage::Closed { reason: "detached" }).await;
                    break AttachmentOutcome::Closed;
                }
                Some(DaemonEvent::Failed) | None => {
                    send_server_message(sender, ServerMessage::Closed { reason: "connection_lost" }).await;
                    break AttachmentOutcome::Closed;
                }
            },
            message = receiver.next() => match message {
                Some(Ok(Message::Binary(bytes))) if bytes.len() <= MAX_ATTACH_FRAME => {
                    if input_tx.try_send(WriterCommand::Frame(AttachFrame::Input(bytes.to_vec()))).is_err() {
                        break AttachmentOutcome::Closed;
                    }
                }
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<ClientMessage>(&text) {
                    Ok(ClientMessage::Resize { rows, cols, pixel_width, pixel_height })
                        if rows > 0 && cols > 0 =>
                    {
                        if input_tx.try_send(WriterCommand::Frame(AttachFrame::Resize {
                            rows,
                            cols,
                            pixel_width,
                            pixel_height,
                        })).is_err() {
                            break AttachmentOutcome::Closed;
                        }
                    }
                    Ok(ClientMessage::Focus) => {
                        if input_tx.try_send(WriterCommand::Frame(AttachFrame::FocusGained)).is_err() {
                            break AttachmentOutcome::Closed;
                        }
                    }
                    Ok(ClientMessage::Detach) => break AttachmentOutcome::Closed,
                    _ => {
                        send_server_message(sender, ServerMessage::Error {
                            code: "invalid_terminal_message",
                            message: "The terminal message was invalid",
                        }).await;
                        break AttachmentOutcome::Closed;
                    }
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break AttachmentOutcome::Closed,
                Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                Some(Ok(Message::Binary(_))) => break AttachmentOutcome::Closed,
            }
        }
    };

    let _ = input_tx.try_send(WriterCommand::Frame(AttachFrame::Detached));
    drop(input_tx);
    drop(event_rx);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = tokio::task::spawn_blocking(move || {
        let _ = reader.join();
        let _ = writer.join();
    })
    .await;
    outcome
}

fn spawn_reader(
    mut stream: UnixStream,
    events: mpsc::Sender<DaemonEvent>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        loop {
            let event = match AttachFrame::read_from(&mut stream) {
                Ok(AttachFrame::Output(bytes)) => DaemonEvent::Output(bytes),
                Ok(AttachFrame::Resize { rows, cols, .. }) => DaemonEvent::Resize { rows, cols },
                Ok(AttachFrame::Reconnect) => DaemonEvent::Reconnect,
                Ok(AttachFrame::Detached) => DaemonEvent::Detached,
                Ok(_) | Err(_) => DaemonEvent::Failed,
            };
            let terminal = !matches!(event, DaemonEvent::Output(_) | DaemonEvent::Resize { .. });
            if events.blocking_send(event).is_err() || terminal {
                return;
            }
        }
    })
}

fn spawn_writer(
    mut stream: UnixStream,
    input: std_mpsc::Receiver<WriterCommand>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while let Ok(command) = input.recv() {
            match command {
                WriterCommand::Frame(frame) => {
                    let detached = frame == AttachFrame::Detached;
                    if frame.write_to(&mut stream).is_err() || detached {
                        return;
                    }
                }
                WriterCommand::ReconnectAck(acknowledged) => {
                    if AttachFrame::ReconnectAck.write_to(&mut stream).is_err() {
                        return;
                    }
                    let _ = acknowledged.send(());
                }
            }
        }
    })
}

async fn acknowledge_reconnect(input: &std_mpsc::SyncSender<WriterCommand>) -> bool {
    let (acknowledged, wait) = std_mpsc::sync_channel(0);
    input
        .try_send(WriterCommand::ReconnectAck(acknowledged))
        .is_ok()
        && tokio::task::spawn_blocking(move || wait.recv_timeout(RECONNECT_ACK_TIMEOUT))
            .await
            .is_ok_and(|result| result.is_ok())
}

async fn send_server_message<S>(sender: &mut S, message: ServerMessage<'_>) -> bool
where
    S: futures_util::Sink<Message, Error = axum::Error> + Unpin,
{
    let Ok(text) = serde_json::to_string(&message) else {
        return false;
    };
    send_message(sender, Message::Text(text.into())).await
}

async fn send_message<S>(sender: &mut S, message: Message) -> bool
where
    S: futures_util::Sink<Message, Error = axum::Error> + Unpin,
{
    tokio::time::timeout(WRITE_TIMEOUT, sender.send(message))
        .await
        .is_ok_and(|result| result.is_ok())
}

fn exact_run_ended(error: &client::ClientError) -> bool {
    matches!(
        error,
        client::ClientError::Remote(client::RemoteError {
            code: Some(ErrorCode::RunChanged | ErrorCode::NotFound),
            ..
        })
    )
}

fn attachment_error(error: &client::ClientError) -> (&'static str, &'static str) {
    if exact_run_ended(error) {
        ("run_changed", "The Agent ShellRun is no longer current")
    } else {
        (
            "attachment_failed",
            "Boomux could not attach to the Agent terminal",
        )
    }
}

fn cancelled_error() -> client::ClientError {
    client::ClientError::Transport(io::Error::new(
        io::ErrorKind::Interrupted,
        "terminal attachment cancelled",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_writer_serializes_input_and_reconnect_acknowledgement() {
        let (writer_stream, mut daemon_stream) = UnixStream::pair().unwrap();
        let (commands, input) = std_mpsc::sync_channel(DAEMON_EVENT_QUEUE);
        let writer = spawn_writer(writer_stream, input);
        commands
            .send(WriterCommand::Frame(AttachFrame::Input(b"input".to_vec())))
            .unwrap();
        let (acknowledged, wait) = std_mpsc::sync_channel(0);
        commands
            .send(WriterCommand::ReconnectAck(acknowledged))
            .unwrap();

        assert_eq!(
            AttachFrame::read_from(&mut daemon_stream).unwrap(),
            AttachFrame::Input(b"input".to_vec())
        );
        assert_eq!(
            AttachFrame::read_from(&mut daemon_stream).unwrap(),
            AttachFrame::ReconnectAck
        );
        wait.recv_timeout(WRITE_TIMEOUT).unwrap();
        commands
            .send(WriterCommand::Frame(AttachFrame::Detached))
            .unwrap();
        writer.join().unwrap();
    }

    #[test]
    fn reader_forwards_authoritative_resize() {
        let (reader_stream, mut daemon_stream) = UnixStream::pair().unwrap();
        let (events, mut received) = mpsc::channel(DAEMON_EVENT_QUEUE);
        let reader = spawn_reader(reader_stream, events);

        AttachFrame::Resize {
            rows: 30,
            cols: 100,
            pixel_width: 1_000,
            pixel_height: 600,
        }
        .write_to(&mut daemon_stream)
        .unwrap();
        AttachFrame::Detached.write_to(&mut daemon_stream).unwrap();

        assert!(matches!(
            received.blocking_recv().unwrap(),
            DaemonEvent::Resize {
                rows: 30,
                cols: 100
            }
        ));
        assert!(matches!(
            received.blocking_recv().unwrap(),
            DaemonEvent::Detached
        ));
        reader.join().unwrap();
    }

    #[tokio::test]
    async fn saturated_input_queue_rejects_reconnect_without_blocking() {
        let (commands, _input) = std_mpsc::sync_channel(1);
        commands
            .send(WriterCommand::Frame(AttachFrame::Input(b"queued".to_vec())))
            .unwrap();

        assert!(!acknowledge_reconnect(&commands).await);
    }

    #[test]
    fn exact_run_disappearance_is_permanent() {
        assert!(exact_run_ended(&client::ClientError::Remote(
            client::RemoteError {
                code: Some(ErrorCode::NotFound),
                message: "gone".into(),
            }
        )));
    }
}
