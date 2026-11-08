#![no_main]

use domyjob_core::client_cleanup::{JsonClient, retire_hooks, retire_mcp};

libfuzzer_sys::fuzz_target!(|bytes: &[u8]| {
    let Ok(document) = core::str::from_utf8(bytes) else {
        return;
    };
    if let Ok(Some(update)) = retire_hooks(document) {
        assert!(retire_hooks(update.as_str()).unwrap().is_none());
    }
    for client in [JsonClient::Claude, JsonClient::OpenCode] {
        if let Ok(Some(update)) = retire_mcp(document, client) {
            assert!(retire_mcp(update.as_str(), client).unwrap().is_none());
        }
    }
});
