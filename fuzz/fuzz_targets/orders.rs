#![no_main]

libfuzzer_sys::fuzz_target!(|bytes: &[u8]| {
    match domyjob::ingress::json::<domyjob::control::Order>(bytes) {
        Ok(_) | Err(_) => {}
    }
    match domyjob::ingress::json::<domyjob::audit::Head>(bytes) {
        Ok(_) | Err(_) => {}
    }
});
