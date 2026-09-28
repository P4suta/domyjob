#[derive(Debug)]
pub(crate) enum Notice {
    Changed,
    Unrelated,
    Failed(notify::Error),
}

/// Reads and opens of a watched file are not changes, so a watcher that reads never wakes itself.
fn classify(event: notify::Result<notify::Event>) -> Notice {
    match event {
        Ok(event) if event.need_rescan() => Notice::Changed,
        Ok(event) if matches!(event.kind, notify::EventKind::Access(_)) => Notice::Unrelated,
        Ok(event) if !event.paths.is_empty() => Notice::Changed,
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
    fn opening_or_reading_a_watched_file_is_not_a_change() {
        let path = std::path::PathBuf::from("watched");
        for access in [
            notify::event::AccessKind::Open(notify::event::AccessMode::Read),
            notify::event::AccessKind::Close(notify::event::AccessMode::Write),
        ] {
            let event =
                notify::Event::new(notify::EventKind::Access(access)).add_path(path.clone());
            assert!(matches!(classify(Ok(event)), Notice::Unrelated));
        }
        let created =
            notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::File))
                .add_path(path);
        assert!(matches!(classify(Ok(created)), Notice::Changed));
    }

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
