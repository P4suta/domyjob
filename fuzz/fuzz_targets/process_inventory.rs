#![no_main]

use domyjob_core::process_group::ProcessGroup;
use domyjob_core::process_inventory::{MAX_INVENTORY_BYTES, group_quiescent};

const SELECTED_GROUP: u32 = 17;
const ROW_FIELDS: usize = 3;
const GROUP_FIELD: usize = 1;
const STATE_FIELD: usize = 2;

libfuzzer_sys::fuzz_target!(|bytes: &[u8]| {
    let Ok(text) = core::str::from_utf8(bytes) else {
        return;
    };
    let group = ProcessGroup::new(i32::try_from(SELECTED_GROUP).unwrap()).unwrap();
    if text.len() > MAX_INVENTORY_BYTES || text.trim().is_empty() {
        assert!(group_quiescent(text, group).is_err());
        return;
    }
    if let Ok(quiet) = group_quiescent(text, group) {
        let mut normalized = String::new();
        let mut live = false;
        for row in text.lines().filter(|row| !row.trim().is_empty()) {
            let fields: Vec<_> = row.split_whitespace().collect();
            assert_eq!(fields.len(), ROW_FIELDS);
            let _pid: u32 = fields.first().unwrap().parse().unwrap();
            let reported: u32 = fields.get(GROUP_FIELD).unwrap().parse().unwrap();
            let state = fields.get(STATE_FIELD).unwrap();
            live |= reported == SELECTED_GROUP && !state.starts_with('Z');
            normalized.push_str(&fields.join(" "));
            normalized.push('\n');
        }
        assert_eq!(quiet, !live);
        if normalized.len() <= MAX_INVENTORY_BYTES {
            assert_eq!(group_quiescent(&normalized, group).unwrap(), quiet);
        } else {
            assert!(group_quiescent(&normalized, group).is_err());
        }
    }
});
