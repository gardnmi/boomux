use std::error::Error;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::thread;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::client;
use crate::node_identity::NodeIdentity;

const FEDERATION_MAGIC: &[u8; 8] = b"BOOMUXF1";
pub const FEDERATION_VERSION: u32 = 1;
const MAX_HANDSHAKE_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FederationConnectionMode {
    AdHoc,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FederationHandshake {
    pub version: u32,
    pub node_id: String,
    pub helper_version: String,
    pub core_protocol_version: u32,
    pub connection_mode: FederationConnectionMode,
}

pub fn write_handshake(writer: &mut impl Write, handshake: &FederationHandshake) -> io::Result<()> {
    validate_handshake(handshake)?;
    let bytes = serde_json::to_vec(handshake).map_err(io::Error::other)?;
    if bytes.len() > MAX_HANDSHAKE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "federation handshake exceeds the size limit",
        ));
    }
    writer.write_all(FEDERATION_MAGIC)?;
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()
}

pub fn read_handshake(reader: &mut impl Read) -> io::Result<FederationHandshake> {
    let mut magic = [0; FEDERATION_MAGIC.len()];
    reader.read_exact(&mut magic)?;
    if &magic != FEDERATION_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid federation handshake header",
        ));
    }
    let mut length = [0; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_HANDSHAKE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid federation handshake length",
        ));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    let handshake: FederationHandshake = serde_json::from_slice(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid federation handshake: {error}"),
        )
    })?;
    validate_handshake(&handshake)?;
    Ok(handshake)
}

fn validate_handshake(handshake: &FederationHandshake) -> io::Result<()> {
    if handshake.version != FEDERATION_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported federation handshake version {}; expected {FEDERATION_VERSION}",
                handshake.version
            ),
        ));
    }
    let node_id = Uuid::parse_str(&handshake.node_id).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "federation handshake contains an invalid Node ID",
        )
    })?;
    if node_id.to_string() != handshake.node_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "federation handshake contains a noncanonical Node ID",
        ));
    }
    if handshake.helper_version.is_empty()
        || handshake.helper_version.len() > 128
        || !handshake
            .helper_version
            .bytes()
            .all(|byte| byte.is_ascii_graphic())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "federation handshake contains an invalid helper version",
        ));
    }
    Ok(())
}

pub fn run_stdio_helper() -> Result<(), Box<dyn Error>> {
    let expected_identity = NodeIdentity::load_or_create_from_environment()?;
    let client = client::connect_or_start()?;
    let channel = client.open_federation_channel()?;
    if channel.node_id != expected_identity.id() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "daemon Node identity does not match the helper state root",
        )
        .into());
    }

    let handshake = FederationHandshake {
        version: FEDERATION_VERSION,
        node_id: channel.node_id,
        helper_version: env!("CARGO_PKG_VERSION").into(),
        core_protocol_version: channel.protocol_version,
        connection_mode: FederationConnectionMode::AdHoc,
    };
    let mut stdout = io::stdout().lock();
    write_handshake(&mut stdout, &handshake)?;

    let mut daemon_writer = channel.stream.try_clone()?;
    thread::Builder::new()
        .name("boomux-federation-input".into())
        .spawn(move || {
            let _ = io::copy(&mut io::stdin().lock(), &mut daemon_writer);
            let _ = daemon_writer.shutdown(Shutdown::Write);
        })?;

    let mut daemon_reader = channel.stream;
    let mut bytes = [0_u8; 16 * 1024];
    loop {
        let count = daemon_reader.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        stdout.write_all(&bytes[..count])?;
        stdout.flush()?;
    }
    let _ = daemon_reader.shutdown(Shutdown::Both);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handshake() -> FederationHandshake {
        FederationHandshake {
            version: FEDERATION_VERSION,
            node_id: "550e8400-e29b-41d4-a716-446655440000".into(),
            helper_version: "1.2.3".into(),
            core_protocol_version: 29,
            connection_mode: FederationConnectionMode::AdHoc,
        }
    }

    #[test]
    fn handshake_round_trips() {
        let expected = handshake();
        let mut bytes = Vec::new();
        write_handshake(&mut bytes, &expected).unwrap();
        assert_eq!(read_handshake(&mut bytes.as_slice()).unwrap(), expected);
    }

    #[test]
    fn handshake_accepts_unknown_additive_fields() {
        let mut value = serde_json::to_value(handshake()).unwrap();
        value["future"] = serde_json::json!({ "enabled": true });
        let body = serde_json::to_vec(&value).unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(FEDERATION_MAGIC);
        bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&body);
        assert_eq!(read_handshake(&mut bytes.as_slice()).unwrap(), handshake());
    }

    #[test]
    fn handshake_rejects_wrong_version_and_oversized_frames() {
        let mut invalid = handshake();
        invalid.version += 1;
        assert_eq!(
            write_handshake(&mut Vec::new(), &invalid)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let mut bytes = Vec::new();
        bytes.extend_from_slice(FEDERATION_MAGIC);
        bytes.extend_from_slice(&((MAX_HANDSHAKE_BYTES + 1) as u32).to_be_bytes());
        assert_eq!(
            read_handshake(&mut bytes.as_slice()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let mut invalid = handshake();
        invalid.helper_version = "1.2.3\u{1b}[2J".into();
        assert_eq!(
            write_handshake(&mut Vec::new(), &invalid)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
}
