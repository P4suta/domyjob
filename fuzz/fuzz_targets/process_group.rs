#![no_main]

use core::str::FromStr;
use domyjob_core::process_group::ProcessGroup;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    if let Ok(text) = core::str::from_utf8(bytes) {
        for candidate in [text, text.trim()] {
            if let Ok(group) = ProcessGroup::from_str(candidate) {
                assert!(group.get() > 0);
                assert_eq!(ProcessGroup::from_str(&group.get().to_string()), Ok(group));
            }
        }
    }
});
