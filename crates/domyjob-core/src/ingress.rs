//! The only JSON decoder: framed wire messages, stored records, and text other programs wrote.
//!
//! Every decoder refuses input above a stated number of bytes before parsing it.

use serde::de::DeserializeOwned;

use crate::state::JobState;
use crate::wire::{self, Envelope, Reply, Request, WireError};

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "this module is the one place that decodes JSON"
    )]

    use serde::de::DeserializeOwned;

    pub(super) fn from_slice<T: DeserializeOwned>(bytes: &[u8]) -> serde_json::Result<T> {
        serde_json::from_slice(bytes)
    }

    pub(super) fn from_value<T: DeserializeOwned>(
        value: serde_json::Value,
    ) -> serde_json::Result<T> {
        serde_json::from_value(value)
    }
}

/// The most bytes of text written by another program, such as a client's configuration, that are decoded.
pub const MAX_FOREIGN_BYTES: usize = wire::MAX_CONTROL_BYTES;

/// JSON that did not become the value it was decoded as.
#[derive(Debug, thiserror::Error)]
pub enum JsonError {
    #[error("the JSON input exceeds its {limit}-byte limit")]
    TooLarge { limit: usize },
    #[error(transparent)]
    Invalid(#[from] serde_json::Error),
}

/// Decode at most `limit` bytes of JSON as `T`.
pub fn json<T: DeserializeOwned>(bytes: &[u8], limit: usize) -> Result<T, JsonError> {
    if bytes.len() > limit {
        return Err(JsonError::TooLarge { limit });
    }
    Ok(raw::from_slice(bytes)?)
}

/// Convert JSON that was already decoded within its limit into `T`.
pub fn value<T: DeserializeOwned>(value: serde_json::Value) -> Result<T, serde_json::Error> {
    raw::from_value(value)
}

fn decode<T: DeserializeOwned>(frame: &[u8]) -> Result<T, WireError> {
    let bytes = wire::payload(frame)?;
    decode_payload(bytes)
}

fn decode_payload<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, WireError> {
    if bytes.len() > wire::MAX_CONTROL_BYTES {
        return Err(WireError::TooLarge);
    }
    let envelope: Envelope<T> = raw::from_slice(bytes)?;
    if envelope.version != wire::VERSION {
        return Err(WireError::Version);
    }
    Ok(envelope.message)
}

pub fn request(frame: &[u8]) -> Result<Request, WireError> {
    decode(frame)
}

pub fn reply(frame: &[u8]) -> Result<Reply, WireError> {
    decode(frame)
}

pub fn stored_request(bytes: &[u8]) -> Result<Request, WireError> {
    decode_payload(bytes)
}

pub fn stored_job(bytes: &[u8]) -> Result<JobState, WireError> {
    if bytes.len() > wire::MAX_CONTROL_BYTES {
        return Err(WireError::TooLarge);
    }
    Ok(raw::from_slice(bytes)?)
}

/// Decodes JSON text that another program wrote, such as a client's configuration or command output.
pub fn foreign_json<T: DeserializeOwned>(text: &str) -> Result<T, JsonError> {
    json(text.as_bytes(), MAX_FOREIGN_BYTES)
}

#[cfg(test)]
mod tests {
    use crate::wire::{self, Request, WireError};
    use alloc::vec::Vec;

    use super::request;

    #[test]
    fn framed_requests_have_one_exact_version_and_length() {
        let valid = wire::frame(&Request::Hello).unwrap();
        assert_eq!(request(&valid).unwrap(), Request::Hello);
        for end in 0..valid.len() {
            request(valid.get(..end).unwrap()).unwrap_err();
        }
        let mut trailing = valid;
        trailing.push(0);
        assert!(matches!(request(&trailing), Err(WireError::Trailing)));
        let mut wrong_version = wire::frame(&Request::Hello).unwrap();
        let json = br#"{"version":2,"message":{"request":"hello"}}"#;
        wrong_version.clear();
        wrong_version.extend_from_slice(&u32::try_from(json.len()).unwrap().to_be_bytes());
        wrong_version.extend_from_slice(json);
        assert!(matches!(request(&wrong_version), Err(WireError::Version)));
    }

    #[test]
    fn request_rejects_unknown_fields_and_oversized_snapshots() {
        let unknown = br#"{"version":1,"message":{"request":"hello","extra":true}}"#;
        let mut frame = Vec::from(u32::try_from(unknown.len()).unwrap().to_be_bytes());
        frame.extend_from_slice(unknown);
        request(&frame).unwrap_err();
        let oversized = br#"{"version":1,"message":{"request":"run","body":{"submission":"11111111111111111111111111111111","command":["cargo","test"],"input":{"source":"snapshot","detail":{"bytes":67108865,"digest":"0000000000000000000000000000000000000000000000000000000000000000"}}}}}"#;
        let mut oversized_frame = Vec::from(u32::try_from(oversized.len()).unwrap().to_be_bytes());
        oversized_frame.extend_from_slice(oversized);
        request(&oversized_frame).unwrap_err();
    }
}
