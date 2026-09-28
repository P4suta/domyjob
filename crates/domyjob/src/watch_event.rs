#![expect(
    clippy::redundant_pub_crate,
    reason = "the binary composition root uses this private module"
)]

#[derive(Debug)]
pub(crate) enum Notice {
    Changed,
    Unrelated,
    Failed(notify::Error),
}

fn classify(event: notify::Result<notify::Event>) -> Notice {
    match event {
        Ok(event) if event.need_rescan() || !event.paths.is_empty() => Notice::Changed,
        Ok(_irrelevant) => Notice::Unrelated,
        Err(error) => Notice::Failed(error),
    }
}

pub(crate) fn watcher(
    mut signal: impl FnMut(Notice) + Send + 'static,
) -> notify::Result<notify::RecommendedWatcher> {
    notify::recommended_watcher(move |event| signal(classify(event)))
}

#[cfg(test)]
mod tests {
    use super::{Notice, classify};

    #[test]
    fn rescan_and_errors_are_not_dropped() {
        let rescan =
            notify::Event::new(notify::EventKind::Other).set_flag(notify::event::Flag::Rescan);
        assert!(matches!(classify(Ok(rescan)), Notice::Changed));
        assert!(matches!(
            classify(Err(notify::Error::generic("watcher broke"))),
            Notice::Failed(_)
        ));
    }
}
