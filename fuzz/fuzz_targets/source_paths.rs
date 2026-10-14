#![no_main]

use domyjob_core::source_paths;

libfuzzer_sys::fuzz_target!(|bytes: &[u8]| {
    if let Ok(paths) = source_paths::tracked_paths(bytes) {
        let mut canonical = Vec::new();
        for path in paths.paths() {
            assert!(!source_paths::metadata_path(path));
            canonical.extend_from_slice(path.as_str().as_bytes());
            canonical.push(0);
        }
        assert_eq!(source_paths::tracked_paths(&canonical).unwrap(), paths);
        assert!(paths.paths().len() <= source_paths::MAX_FILES);
        assert!(canonical.len() <= source_paths::MAX_TRACKED_BYTES);
    }
});
