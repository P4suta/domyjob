#![no_main]

libfuzzer_sys::fuzz_target!(|bytes: &[u8]| {
    if let Ok(request) = domyjob::ingress::json::<domyjob::protocol::Request>(bytes) {
        let again = serde_json::to_vec(&request).unwrap();
        let back: domyjob::protocol::Request = domyjob::ingress::json(&again).unwrap();
        assert_eq!(back, request);
    }
    match domyjob::ingress::json::<domyjob::protocol::Reply>(bytes) {
        Ok(_) | Err(_) => {}
    }
});
