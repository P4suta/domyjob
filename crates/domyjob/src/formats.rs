const CHAT: &[u8] = include_bytes!("../formats/chat.txt");
const PREVIOUS_CHAT: &[u8] = include_bytes!("../formats/chat-v1.txt");
const RUNNER: &[u8] = include_bytes!("../formats/runner.txt");
const FORMAT_DIGEST_HEX_CHARS: usize = 16;

fn identity(specimen: &[u8]) -> String {
    blake3::hash(specimen)
        .to_hex()
        .chars()
        .take(FORMAT_DIGEST_HEX_CHARS)
        .collect()
}

pub(crate) fn chat() -> String {
    identity(CHAT)
}

pub(crate) fn previous_chat() -> String {
    identity(PREVIOUS_CHAT)
}

pub(crate) fn compatible_chat(format: &str) -> bool {
    format == chat() || format == previous_chat()
}

pub(crate) fn runner() -> String {
    identity(RUNNER)
}

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
