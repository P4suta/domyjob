//! The only JSON decoder: framed wire messages, stored records, and text other programs wrote.
//!
//! Every decoder refuses input above a stated number of bytes before parsing it.

use serde::de::DeserializeOwned;

use crate::state::JobState;
use crate::wire::{self, Reply, Request, WireError};

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
    Ok(raw::from_slice(bytes)?)
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
    use alloc::vec::Vec;

    use super::{JsonError, json, request, stored_job, stored_request};
    use crate::state::JobState;
    use crate::wire::{self, Request, WireError};

    /// `text` padded with spaces, which JSON ignores, to exactly `length` bytes.
    fn padded(text: &[u8], length: usize) -> Vec<u8> {
        let mut bytes = text.to_vec();
        bytes.resize(length, b' ');
        bytes
    }

    #[test]
    fn every_decoder_refuses_input_past_its_limit_before_parsing() {
        json::<serde_json::Value>(b"[1]", 3).unwrap();
        assert!(matches!(
            json::<serde_json::Value>(b"[1]", 2),
            Err(JsonError::TooLarge { limit: 2 })
        ));
        let hello = br#"{"request":"hello"}"#;
        assert_eq!(
            stored_request(&padded(hello, wire::MAX_CONTROL_BYTES)).unwrap(),
            Request::Hello
        );
        assert!(matches!(
            stored_request(&padded(hello, wire::MAX_CONTROL_BYTES.saturating_add(1))),
            Err(WireError::TooLarge)
        ));
        let running = stored_job(br#"{"phase":{"phase":"running","pid":42}}"#).unwrap();
        assert_eq!(running.pid(), Some(42));
        let accepted = br#"{"phase":{"phase":"accepted"}}"#;
        assert_eq!(
            stored_job(&padded(accepted, wire::MAX_CONTROL_BYTES)).unwrap(),
            JobState::accepted()
        );
        assert!(matches!(
            stored_job(&padded(accepted, wire::MAX_CONTROL_BYTES.saturating_add(1))),
            Err(WireError::TooLarge)
        ));
    }

    #[test]
    fn framed_requests_have_one_exact_length() {
        let valid = wire::frame(&Request::Hello).unwrap();
        assert_eq!(request(&valid).unwrap(), Request::Hello);
        for end in 0..valid.len() {
            assert!(
                matches!(
                    request(valid.get(..end).unwrap()),
                    Err(WireError::Incomplete)
                ),
                "a frame cut at {end} bytes is incomplete"
            );
        }
        let mut trailing = valid;
        trailing.push(0);
        assert!(matches!(request(&trailing), Err(WireError::Trailing)));
    }

    #[test]
    fn request_rejects_unknown_fields_and_oversized_snapshots() {
        let unknown = br#"{"request":"hello","extra":true}"#;
        let mut frame = Vec::from(u32::try_from(unknown.len()).unwrap().to_be_bytes());
        frame.extend_from_slice(unknown);
        request(&frame).unwrap_err();
        let oversized = br#"{"request":"run","body":{"submission":"11111111111111111111111111111111","command":["cargo","test"],"input":{"source":"snapshot","detail":{"bytes":67108865,"digest":"0000000000000000000000000000000000000000000000000000000000000000"}}}}"#;
        let mut oversized_frame = Vec::from(u32::try_from(oversized.len()).unwrap().to_be_bytes());
        oversized_frame.extend_from_slice(oversized);
        request(&oversized_frame).unwrap_err();
    }
}
