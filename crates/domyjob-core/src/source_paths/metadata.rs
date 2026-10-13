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
    match component {
        [first, second, third] => matches!(
            [
                first.to_ascii_lowercase(),
                second.to_ascii_lowercase(),
                third.to_ascii_lowercase()
            ],
            [b'.', b'j', b'j'] | [b'.', b'h', b'g'] | [b'c', b'v', b's']
        ),
        [first, second, third, fourth] => matches!(
            [
                first.to_ascii_lowercase(),
                second.to_ascii_lowercase(),
                third.to_ascii_lowercase(),
                fourth.to_ascii_lowercase()
            ],
            [b'.', b'g', b'i', b't'] | [b'.', b's', b'v', b'n'] | [b'.', b'b', b'z', b'r']
        ),
        [first, second, third, fourth, fifth, sixth] => matches!(
            [
                first.to_ascii_lowercase(),
                second.to_ascii_lowercase(),
                third.to_ascii_lowercase(),
                fourth.to_ascii_lowercase(),
                fifth.to_ascii_lowercase(),
                sixth.to_ascii_lowercase()
            ],
            [b'.', b'p', b'i', b'j', b'u', b'l'] | [b'_', b'd', b'a', b'r', b'c', b's']
        ),
        _ => false,
    }
}
