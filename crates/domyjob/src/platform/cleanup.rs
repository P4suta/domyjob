#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(crate) use unix::{
    capture_directory, child_options, link, open_root, prepare_directory, remove_link,
};
#[cfg(windows)]
pub(crate) use windows::{
    capture_directory, child_options, link, open_root, prepare_directory, remove_link,
};

#[cfg(all(test, unix))]
pub(crate) use unix::{linked_directory_refusal, replace_root};
#[cfg(all(test, windows))]
pub(crate) use windows::{linked_directory_refusal, replace_root};

#[cfg(test)]
fn rename_fixture_directory(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    let parent = from
        .parent()
        .ok_or_else(|| std::io::Error::other("fixture has no parent"))?;
    if to.parent() != Some(parent) {
        return Err(std::io::Error::other(
            "fixture rename must keep its private parent",
        ));
    }
    let retained = crate::state_io::existing_directory(parent)
        .map_err(std::io::Error::other)?
        .ok_or_else(|| std::io::Error::other("fixture parent is missing"))?;
    retained.rename(
        from.file_name()
            .ok_or_else(|| std::io::Error::other("fixture has no name"))?,
        &retained,
        to.file_name()
            .ok_or_else(|| std::io::Error::other("fixture has no replacement name"))?,
    )
}
