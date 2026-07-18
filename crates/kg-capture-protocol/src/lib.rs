//! Architecture-neutral messages exchanged by the x64 host and x86 hook DLL.

use ipc_channel::ipc::{IpcReceiver, IpcSender};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 2;
pub const BOOTSTRAP_ENDPOINT_CAPACITY: usize = 512;
pub const BOOTSTRAP_LOG_PATH_CAPACITY: usize = 512;

/// Pointer-free data written into the target process before invoking the DLL's
/// exported start routine. `repr(C)` keeps its layout identical on x86 and x64.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HookBootstrap {
    pub protocol_version: u16,
    pub endpoint_len: u16,
    pub log_path_len: u16,
    pub reserved: u16,
    pub session_nonce: SessionNonce,
    pub endpoint: [u8; BOOTSTRAP_ENDPOINT_CAPACITY],
    pub log_path: [u8; BOOTSTRAP_LOG_PATH_CAPACITY],
}

impl HookBootstrap {
    pub fn new(
        endpoint: &str,
        session_nonce: SessionNonce,
        log_path: &str,
    ) -> Result<Self, BootstrapError> {
        let endpoint_bytes = endpoint.as_bytes();
        let endpoint_len = u16::try_from(endpoint_bytes.len())
            .map_err(|_| BootstrapError::EndpointTooLong(endpoint_bytes.len()))?;
        if endpoint_bytes.len() > BOOTSTRAP_ENDPOINT_CAPACITY {
            return Err(BootstrapError::EndpointTooLong(endpoint_bytes.len()));
        }
        let log_path_bytes = log_path.as_bytes();
        let log_path_len = u16::try_from(log_path_bytes.len())
            .map_err(|_| BootstrapError::LogPathTooLong(log_path_bytes.len()))?;
        if log_path_bytes.len() > BOOTSTRAP_LOG_PATH_CAPACITY {
            return Err(BootstrapError::LogPathTooLong(log_path_bytes.len()));
        }

        let mut value = Self {
            protocol_version: PROTOCOL_VERSION,
            endpoint_len,
            log_path_len,
            reserved: 0,
            session_nonce,
            endpoint: [0; BOOTSTRAP_ENDPOINT_CAPACITY],
            log_path: [0; BOOTSTRAP_LOG_PATH_CAPACITY],
        };
        value.endpoint[..endpoint_bytes.len()].copy_from_slice(endpoint_bytes);
        value.log_path[..log_path_bytes.len()].copy_from_slice(log_path_bytes);
        Ok(value)
    }

    pub fn endpoint(&self) -> Result<&str, BootstrapError> {
        let length = usize::from(self.endpoint_len);
        if length > BOOTSTRAP_ENDPOINT_CAPACITY {
            return Err(BootstrapError::EndpointTooLong(length));
        }
        std::str::from_utf8(&self.endpoint[..length]).map_err(|_| BootstrapError::InvalidUtf8)
    }

    pub fn log_path(&self) -> Result<&str, BootstrapError> {
        let length = usize::from(self.log_path_len);
        if length > BOOTSTRAP_LOG_PATH_CAPACITY {
            return Err(BootstrapError::LogPathTooLong(length));
        }
        std::str::from_utf8(&self.log_path[..length]).map_err(|_| BootstrapError::InvalidUtf8)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootstrapError {
    EndpointTooLong(usize),
    LogPathTooLong(usize),
    InvalidUtf8,
}

impl std::fmt::Display for BootstrapError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EndpointTooLong(length) => {
                write!(formatter, "IPC endpoint is too long: {length}")
            }
            Self::LogPathTooLong(length) => write!(formatter, "log path is too long: {length}"),
            Self::InvalidUtf8 => formatter.write_str("IPC endpoint is not UTF-8"),
        }
    }
}

impl std::error::Error for BootstrapError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionNonce(pub [u8; 16]);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum LyricSource {
    Standard,
    LiveShow,
    Fixture,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LyricWord {
    pub text: String,
    pub start_ms: f32,
    pub duration_ms: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LyricLine {
    pub index: u32,
    pub text: String,
    pub start_ms: f32,
    pub duration_ms: f32,
    pub words: Vec<LyricWord>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LyricTimeline {
    pub id: u64,
    pub source: LyricSource,
    pub lines: Vec<LyricLine>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlaybackPosition {
    pub timeline_id: u64,
    pub observed_at_micros: u64,
    pub position_ms: f32,
    pub current_line: Option<u32>,
    pub line_progress: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum HostCommand {
    StartCapture,
    StopCapture,
    Ping { sequence: u64 },
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HookHello {
    pub protocol_version: u16,
    pub process_id: u32,
    pub session_nonce: SessionNonce,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum HookEvent {
    CaptureStarted,
    CaptureStopped,
    Timeline(LyricTimeline),
    Playback(PlaybackPosition),
    Warning(String),
    Error(String),
    Pong { sequence: u64 },
}

/// First message sent through the one-shot bootstrap server. Transferring both
/// channel endpoints establishes full-duplex communication afterwards.
#[derive(Serialize, Deserialize)]
pub struct HookHandshake {
    pub hello: HookHello,
    pub command_sender: IpcSender<HostCommand>,
    pub event_receiver: IpcReceiver<HookEvent>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipc_channel::ipc;

    #[test]
    fn semantic_event_round_trip() {
        let (sender, receiver) = ipc::channel().expect("create IPC channel");
        let timeline = LyricTimeline {
            id: 7,
            source: LyricSource::Fixture,
            lines: vec![LyricLine {
                index: 0,
                text: "把爱留在身边".into(),
                start_ms: 1_000.0,
                duration_ms: 2_000.0,
                words: vec![LyricWord {
                    text: "把爱".into(),
                    start_ms: 1_000.0,
                    duration_ms: 500.0,
                }],
            }],
        };
        sender
            .send(HookEvent::Timeline(timeline.clone()))
            .expect("send event");

        let HookEvent::Timeline(received) = receiver.recv().expect("receive event") else {
            panic!("unexpected event")
        };
        assert_eq!(received, timeline);
    }

    #[test]
    fn bootstrap_round_trip() {
        let nonce = SessionNonce([7; 16]);
        let bootstrap = HookBootstrap::new("kg-capture-test", nonce, "C:\\temp\\hook.log")
            .expect("create bootstrap");
        assert_eq!(
            bootstrap.endpoint().expect("read endpoint"),
            "kg-capture-test"
        );
        assert_eq!(bootstrap.session_nonce, nonce);
        assert_eq!(bootstrap.protocol_version, PROTOCOL_VERSION);
        assert_eq!(
            bootstrap.log_path().expect("read log path"),
            "C:\\temp\\hook.log"
        );
    }
}
