use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{Command, JobId, RemoteText, SubmissionId};
use crate::state::JobState;

pub const MAX_CONTROL_BYTES: usize = 1_048_576;
pub const MAX_SNAPSHOT_BYTES: u64 = 67_108_864;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BuildId(u64);

impl BuildId {
    #[must_use]
    pub const fn from_fingerprint(fingerprint: u64) -> Self {
        Self(fingerprint)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlLength(usize);

impl TryFrom<[u8; 4]> for ControlLength {
    type Error = WireError;

    fn try_from(header: [u8; 4]) -> Result<Self, Self::Error> {
        let length =
            usize::try_from(u32::from_be_bytes(header)).map_err(|_length| WireError::TooLarge)?;
        if length > MAX_CONTROL_BYTES {
            return Err(WireError::TooLarge);
        }
        Ok(Self(length))
    }
}

impl ControlLength {
    #[must_use]
    pub const fn bytes(self) -> usize {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    deny_unknown_fields,
    rename_all = "snake_case",
    tag = "request",
    content = "body"
)]
pub enum Request {
    Hello,
    Chat(crate::chat_wire::ChatRequest),
    Run {
        submission: SubmissionId,
        command: Command,
        input: Input,
    },
    List,
    Status {
        job: JobId,
    },
    Logs {
        job: JobId,
    },
    Wait {
        job: JobId,
    },
    Kill {
        job: JobId,
    },
    Clean {
        target: CleanTarget,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    deny_unknown_fields,
    rename_all = "snake_case",
    tag = "target",
    content = "detail"
)]
pub enum CleanTarget {
    Job(JobId),
    Finished,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    deny_unknown_fields,
    rename_all = "snake_case",
    tag = "source",
    content = "detail"
)]
pub enum Input {
    Home,
    Snapshot(Snapshot),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, try_from = "RawSnapshot", into = "RawSnapshot")]
pub struct Snapshot {
    bytes: u64,
    digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSnapshot {
    bytes: u64,
    digest: String,
}

impl TryFrom<RawSnapshot> for Snapshot {
    type Error = WireError;

    fn try_from(raw: RawSnapshot) -> Result<Self, Self::Error> {
        if raw.bytes > MAX_SNAPSHOT_BYTES
            || raw.digest.len() != 64
            || !raw
                .digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(WireError::Snapshot);
        }
        Ok(Self {
            bytes: raw.bytes,
            digest: raw.digest,
        })
    }
}

impl From<Snapshot> for RawSnapshot {
    fn from(snapshot: Snapshot) -> Self {
        Self {
            bytes: snapshot.bytes,
            digest: snapshot.digest,
        }
    }
}

impl Snapshot {
    pub fn new(bytes: u64, digest: String) -> Result<Self, WireError> {
        Self::try_from(RawSnapshot { bytes, digest })
    }

    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    deny_unknown_fields,
    rename_all = "snake_case",
    tag = "reply",
    content = "body"
)]
pub enum Reply {
    Hello { build: BuildId },
    Chat(crate::chat_wire::ChatReply),
    Accepted { job: JobId },
    Jobs { jobs: Vec<JobId> },
    Status { state: JobState },
    Logs { text: RemoteText, omitted: u64 },
    Cleaned { count: u16 },
    Error { code: ErrorCode },
}

/// A reply of another kind than the one its request expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unexpected {
    /// The node refused the request.
    Refused(ErrorCode),
    /// The node answered with a reply of another kind.
    Other,
}

impl Reply {
    fn unexpected(self) -> Unexpected {
        match self {
            Self::Error { code } => Unexpected::Refused(code),
            Self::Hello { .. }
            | Self::Chat(_)
            | Self::Accepted { .. }
            | Self::Jobs { .. }
            | Self::Status { .. }
            | Self::Logs { .. }
            | Self::Cleaned { .. } => Unexpected::Other,
        }
    }
}

macro_rules! reply_variant {
    ($($method:ident: $pattern:pat => $value:expr, $output:ty;)+) => {
        impl Reply {
            $(
                /// The reply's content when it is of this kind.
                pub fn $method(self) -> Result<$output, Unexpected> {
                    if let $pattern = self {
                        Ok($value)
                    } else {
                        Err(self.unexpected())
                    }
                }
            )+
        }
    };
}

reply_variant! {
    into_hello: Self::Hello { build } => build, BuildId;
    into_accepted: Self::Accepted { job } => job, JobId;
    into_jobs: Self::Jobs { jobs } => jobs, Vec<JobId>;
    into_status: Self::Status { state } => state, JobState;
    into_logs: Self::Logs { text, omitted } => (text, omitted), (RemoteText, u64);
    into_cleaned: Self::Cleaned { count } => count, u16;
    into_chat: Self::Chat(reply) => reply, crate::chat_wire::ChatReply;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    MissingJob,
    ConflictingSubmission,
    Unsupported,
    CorruptState,
    ResourceLimit,
    Internal,
}

#[derive(Debug, Error)]
pub enum WireError {
    #[error("control frame is incomplete")]
    Incomplete,
    #[error("control frame has trailing bytes")]
    Trailing,
    #[error("control frame exceeds 1 MiB")]
    TooLarge,
    #[error("invalid snapshot size or digest")]
    Snapshot,
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn payload(frame: &[u8]) -> Result<&[u8], WireError> {
    let Some(header) = frame.get(..4) else {
        return Err(WireError::Incomplete);
    };
    let header = <[u8; 4]>::try_from(header).map_err(|_length| WireError::Incomplete)?;
    let length = ControlLength::try_from(header)?;
    let expected = length.bytes().checked_add(4).ok_or(WireError::TooLarge)?;
    if frame.len() < expected {
        return Err(WireError::Incomplete);
    }
    if frame.len() > expected {
        return Err(WireError::Trailing);
    }
    frame.get(4..).ok_or(WireError::Incomplete)
}

pub fn frame<T: Serialize>(message: &T) -> Result<Vec<u8>, WireError> {
    let json = serde_json::to_vec(message)?;
    if json.len() > MAX_CONTROL_BYTES {
        return Err(WireError::TooLarge);
    }
    let length = u32::try_from(json.len()).map_err(|_length| WireError::TooLarge)?;
    let mut framed = Vec::with_capacity(json.len().saturating_add(4));
    framed.extend_from_slice(&length.to_be_bytes());
    framed.extend_from_slice(&json);
    Ok(framed)
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use super::{
        ControlLength, MAX_CONTROL_BYTES, MAX_SNAPSHOT_BYTES, Snapshot, WireError, frame, payload,
    };

    fn header(length: usize) -> [u8; 4] {
        u32::try_from(length).unwrap().to_be_bytes()
    }

    #[test]
    fn frames_and_their_lengths_hold_the_control_limit_exactly() {
        assert_eq!(
            ControlLength::try_from(header(MAX_CONTROL_BYTES))
                .unwrap()
                .bytes(),
            MAX_CONTROL_BYTES
        );
        assert!(matches!(
            ControlLength::try_from(header(MAX_CONTROL_BYTES.saturating_add(1))),
            Err(WireError::TooLarge)
        ));
        assert!(matches!(
            payload(&header(MAX_CONTROL_BYTES.saturating_add(1))),
            Err(WireError::TooLarge)
        ));
        // A JSON string is its text and two quotes.
        let fits = "x".repeat(MAX_CONTROL_BYTES.saturating_sub(2));
        assert_eq!(
            frame(&fits).unwrap().len(),
            MAX_CONTROL_BYTES.saturating_add(4)
        );
        assert!(matches!(
            frame(&"x".repeat(MAX_CONTROL_BYTES.saturating_sub(1))),
            Err(WireError::TooLarge)
        ));
    }

    #[test]
    fn snapshots_hold_their_size_and_digest_rules() {
        let digest = "0123456789abcdef".repeat(4);
        let snapshot = Snapshot::new(MAX_SNAPSHOT_BYTES, digest.clone()).unwrap();
        assert_eq!(
            (snapshot.bytes(), snapshot.digest()),
            (MAX_SNAPSHOT_BYTES, digest.as_str())
        );
        assert!(matches!(
            Snapshot::new(MAX_SNAPSHOT_BYTES.saturating_add(1), digest),
            Err(WireError::Snapshot)
        ));
        let invalid: [String; 3] = ["0".repeat(63), "A".repeat(64), "g".repeat(64)];
        for bad in invalid {
            assert!(matches!(Snapshot::new(1, bad), Err(WireError::Snapshot)));
        }
        Snapshot::new(0, "f".repeat(64)).unwrap();
    }
}
