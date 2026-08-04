use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 6;
pub const MAX_CONTROL_FRAME: usize = 8 * 1024 * 1024;
pub const MAX_ATTACH_FRAME: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub version: u32,
    pub message: T,
}

impl<T> Envelope<T> {
    pub fn new(message: T) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            message,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub workspaces: Vec<WorkspaceSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub id: String,
    pub name: String,
    pub shells: Vec<ShellSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellSnapshot {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    pub cwd: PathBuf,
    pub status: ShellStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellStatus {
    Pending,
    Running,
    Exited { code: Option<u32> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalProfile {
    pub term: Option<String>,
    pub colorterm: Option<String>,
    pub term_program: Option<String>,
    pub term_program_version: Option<String>,
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellSpec {
    pub name: String,
    #[serde(default)]
    pub command: Vec<String>,
    pub cwd: PathBuf,
}

impl ShellSpec {
    pub fn login(name: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            command: Vec::new(),
            cwd: cwd.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Restart,
    Shutdown,
    Snapshot,
    GetWorkspace {
        workspace_id: String,
    },
    GetShell {
        shell_id: String,
    },
    CreateWorkspace {
        name: String,
        shells: Vec<ShellSpec>,
    },
    CreateShell {
        #[serde(default)]
        workspace_id: Option<String>,
        shell: ShellSpec,
    },
    ReadShell {
        shell_id: String,
        max_bytes: usize,
    },
    RenameWorkspace {
        workspace_id: String,
        name: String,
    },
    RenameShell {
        shell_id: String,
        name: String,
    },
    CloseWorkspace {
        workspace_id: String,
    },
    CloseShell {
        shell_id: String,
    },
    Attach {
        shell_id: String,
        takeover: bool,
        profile: TerminalProfile,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum Response {
    Pong,
    Snapshot {
        snapshot: Snapshot,
    },
    Workspace {
        workspace: WorkspaceSnapshot,
    },
    Shell {
        shell: ShellSnapshot,
    },
    Output {
        bytes: Vec<u8>,
    },
    Attached {
        token: String,
        reconstruction: Vec<u8>,
        warning: Option<String>,
    },
    Ok,
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachFrame {
    Input(Vec<u8>),
    Output(Vec<u8>),
    Resize {
        rows: u16,
        cols: u16,
        pixel_width: u16,
        pixel_height: u16,
    },
    Detached,
    Reconnect,
    ReconnectAck,
}

impl AttachFrame {
    const INPUT: u8 = 1;
    const OUTPUT: u8 = 2;
    const RESIZE: u8 = 3;
    const DETACHED: u8 = 4;
    const RECONNECT: u8 = 5;
    const RECONNECT_ACK: u8 = 6;

    pub fn write_to(&self, writer: &mut impl Write) -> io::Result<()> {
        let (kind, payload): (u8, &[u8]) = match self {
            Self::Input(bytes) => (Self::INPUT, bytes),
            Self::Output(bytes) => (Self::OUTPUT, bytes),
            Self::Resize {
                rows,
                cols,
                pixel_width,
                pixel_height,
            } => {
                writer.write_all(&[Self::RESIZE])?;
                writer.write_all(&8_u32.to_be_bytes())?;
                writer.write_all(&rows.to_be_bytes())?;
                writer.write_all(&cols.to_be_bytes())?;
                writer.write_all(&pixel_width.to_be_bytes())?;
                return writer.write_all(&pixel_height.to_be_bytes());
            }
            Self::Detached => (Self::DETACHED, &[]),
            Self::Reconnect => (Self::RECONNECT, &[]),
            Self::ReconnectAck => (Self::RECONNECT_ACK, &[]),
        };
        if payload.len() > MAX_ATTACH_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "attach frame too large",
            ));
        }
        writer.write_all(&[kind])?;
        writer.write_all(&(payload.len() as u32).to_be_bytes())?;
        writer.write_all(payload)
    }

    pub fn read_from(reader: &mut impl Read) -> io::Result<Self> {
        let mut kind = [0];
        reader.read_exact(&mut kind)?;
        let mut length = [0; 4];
        reader.read_exact(&mut length)?;
        let length = u32::from_be_bytes(length) as usize;
        if length > MAX_ATTACH_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "attach frame too large",
            ));
        }
        let mut payload = vec![0; length];
        reader.read_exact(&mut payload)?;
        match kind[0] {
            Self::INPUT => Ok(Self::Input(payload)),
            Self::OUTPUT => Ok(Self::Output(payload)),
            Self::RESIZE if payload.len() == 8 => Ok(Self::Resize {
                rows: u16::from_be_bytes([payload[0], payload[1]]),
                cols: u16::from_be_bytes([payload[2], payload[3]]),
                pixel_width: u16::from_be_bytes([payload[4], payload[5]]),
                pixel_height: u16::from_be_bytes([payload[6], payload[7]]),
            }),
            Self::DETACHED if payload.is_empty() => Ok(Self::Detached),
            Self::RECONNECT if payload.is_empty() => Ok(Self::Reconnect),
            Self::RECONNECT_ACK if payload.is_empty() => Ok(Self::ReconnectAck),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid attach frame",
            )),
        }
    }
}

pub fn write_message<T: Serialize>(writer: &mut impl Write, message: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(message).map_err(io::Error::other)?;
    if bytes.len() > MAX_CONTROL_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "control frame too large",
        ));
    }
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)
}

pub fn read_message<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<T> {
    let mut length = [0; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_CONTROL_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control frame too large",
        ));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_frame_round_trips() {
        let value = Envelope::new(Request::Attach {
            shell_id: "s1".into(),
            takeover: false,
            profile: TerminalProfile {
                term: Some("xterm-256color".into()),
                colorterm: Some("truecolor".into()),
                term_program: Some("test".into()),
                term_program_version: Some("1".into()),
                rows: 24,
                cols: 80,
                pixel_width: 800,
                pixel_height: 600,
            },
        });
        let mut bytes = Vec::new();
        write_message(&mut bytes, &value).unwrap();
        assert_eq!(
            read_message::<Envelope<Request>>(&mut bytes.as_slice()).unwrap(),
            value
        );
    }

    #[test]
    fn shell_spec_requires_cwd_on_the_wire() {
        let request = r#"{"request":"create_shell","workspace_id":"w1","shell":{"name":"shell","command":[]}}"#;

        assert!(serde_json::from_str::<Request>(request).is_err());
    }

    #[test]
    fn attach_frames_round_trip() {
        let frames = [
            AttachFrame::Input(vec![0, 1, 255]),
            AttachFrame::Resize {
                rows: 24,
                cols: 80,
                pixel_width: 1920,
                pixel_height: 1080,
            },
            AttachFrame::Detached,
            AttachFrame::Reconnect,
            AttachFrame::ReconnectAck,
        ];
        for frame in frames {
            let mut bytes = Vec::new();
            frame.write_to(&mut bytes).unwrap();
            assert_eq!(
                AttachFrame::read_from(&mut bytes.as_slice()).unwrap(),
                frame
            );
        }
    }

    #[test]
    fn rejects_protocol_two_resize_frame() {
        let bytes = [AttachFrame::RESIZE, 0, 0, 0, 4, 0, 24, 0, 80];

        assert!(AttachFrame::read_from(&mut bytes.as_slice()).is_err());
    }
}
