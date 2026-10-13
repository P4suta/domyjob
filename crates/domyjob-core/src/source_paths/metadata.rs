#[cfg(test)]
pub(super) const METADATA: &[&str] = &[
    ".git", ".jj", ".hg", ".svn", ".pijul", "_darcs", ".bzr", "CVS",
];

#[must_use]
pub const fn metadata_component(component: &str) -> bool {
    metadata_bytes(component.as_bytes())
}

#[must_use]
pub(super) fn metadata_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    let mut first = 0;
    for end in 0..=bytes.len() {
        match bytes.get(end) {
            Some(b'/') | None => {
                if bytes.get(first..end).is_some_and(metadata_bytes) {
                    return true;
                }
                first = end.saturating_add(1);
            }
            Some(_) => {}
        }
    }
    false
}

const fn metadata_bytes(component: &[u8]) -> bool {
    matches!(
        component,
        [b'.', b'g', b'i', b't']
            | [b'.', b'j', b'j']
            | [b'.', b'h', b'g']
            | [b'.', b's', b'v', b'n']
            | [b'.', b'p', b'i', b'j', b'u', b'l']
            | [b'_', b'd', b'a', b'r', b'c', b's']
            | [b'.', b'b', b'z', b'r']
            | [b'C', b'V', b'S']
    )
}
