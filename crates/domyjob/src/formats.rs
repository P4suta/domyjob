//! The identity of each stored format: a digest of one specimen of everything the store keeps.
//!
//! No format carries a hand-written version.
//! A test encodes the specimen and compares it with the checked-in file under `formats/`,
//! so a changed format fails the tests until its file is regenerated,
//! and the regenerated file changes the identity that every store records and checks.
//! `DOMYJOB_UPDATE_FORMATS=1 cargo test` rewrites the files.

const CHAT: &[u8] = include_bytes!("../formats/chat.txt");
const RUNNER: &[u8] = include_bytes!("../formats/runner.txt");

fn identity(specimen: &[u8]) -> String {
    blake3::hash(specimen).to_hex().chars().take(16).collect()
}

/// The format of the chat store.
pub(crate) fn chat() -> String {
    identity(CHAT)
}

/// The format of the job runner's store.
pub(crate) fn runner() -> String {
    identity(RUNNER)
}

/// Fail unless `specimen` is the checked-in specimen of `name`, or rewrite it on request.
#[cfg(test)]
pub(crate) fn check(name: &str, specimen: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("formats")
        .join(format!("{name}.txt"));
    if std::env::var_os("DOMYJOB_UPDATE_FORMATS").is_some() {
        crate::testing::write(&path, format!("{specimen}\n"));
        return;
    }
    let checked_in = crate::testing::read(&path);
    assert!(
        checked_in == format!("{specimen}\n"),
        "the {name} format changed, so stores written before this change cannot be read; \
         review the difference, then run `DOMYJOB_UPDATE_FORMATS=1 cargo test` to accept it\n\
         checked in:\n{checked_in}\nnow:\n{specimen}"
    );
}
