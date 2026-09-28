use serde::de::DeserializeOwned;

use crate::state::JobState;
use crate::wire::{self, Envelope, Reply, Request, WireError};

fn decode<T: DeserializeOwned>(frame: &[u8]) -> Result<T, WireError> {
    let bytes = wire::payload(frame)?;
    decode_payload(bytes)
}

#[expect(
    clippy::disallowed_methods,
    reason = "all external JSON decoding is confined to this ingress boundary"
)]
fn decode_payload<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, WireError> {
    if bytes.len() > wire::MAX_CONTROL_BYTES {
        return Err(WireError::TooLarge);
    }
    let envelope: Envelope<T> = serde_json::from_slice(bytes)?;
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

#[expect(
    clippy::disallowed_methods,
    reason = "stored JSON is decoded only after the state-file size has been checked"
)]
pub fn stored_job(bytes: &[u8]) -> Result<JobState, WireError> {
    if bytes.len() > wire::MAX_CONTROL_BYTES {
        return Err(WireError::TooLarge);
    }
    Ok(serde_json::from_slice(bytes)?)
}

/// Decodes JSON text that another program wrote, such as a client's configuration or command output.
///
/// Callers bound the text they read before handing it here.
#[expect(
    clippy::disallowed_methods,
    reason = "JSON written by other programs is decoded only at this ingress boundary"
)]
pub fn foreign_json<T: DeserializeOwned>(text: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(text)
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
