#![no_main]

use domyjob_core::{ingress, wire};

libfuzzer_sys::fuzz_target!(|bytes: &[u8]| {
    if bytes.len() > wire::MAX_CONTROL_BYTES + 4 {
        return;
    }
    if let Ok(request) = ingress::request(bytes) {
        let encoded = wire::frame(&request).unwrap();
        assert_eq!(ingress::request(&encoded).unwrap(), request);
    }
    if let Ok(reply) = ingress::reply(bytes) {
        let encoded = wire::frame(&reply).unwrap();
        assert_eq!(ingress::reply(&encoded).unwrap(), reply);
    }
    if let Ok(request) = ingress::stored_request(bytes) {
        let encoded = wire::frame(&request).unwrap();
        assert_eq!(ingress::request(&encoded).unwrap(), request);
    }
    if let Ok(state) = ingress::stored_job(bytes) {
        let encoded = serde_json::to_vec(&state).unwrap();
        assert_eq!(ingress::stored_job(&encoded).unwrap(), state);
    }
});
