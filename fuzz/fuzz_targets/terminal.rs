#![no_main]

use std::io::Write;

libfuzzer_sys::fuzz_target!(|chunks: Vec<Vec<u8>>| {
    let mut out = domyjob::terminal::SanitizingWriter::new(Vec::new());
    for chunk in &chunks {
        out.write_all(chunk).unwrap();
    }
    out.flush().unwrap();
    let shown = String::from_utf8(out.into_inner()).unwrap();
    for c in shown.chars() {
        assert!(c == '\t' || !domyjob::terminal::dangerous(c));
    }
});
