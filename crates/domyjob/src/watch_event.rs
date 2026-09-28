use std::path::Path;

pub(crate) enum Notice {
    Relevant,
    Irrelevant,
    Failed(notify::Error),
}

fn classify(
    event: notify::Result<notify::Event>,
    mut relevant: impl FnMut(&Path) -> bool,
) -> Notice {
    match event {
        Ok(event) if event.need_rescan() || event.paths.iter().any(|path| relevant(path)) => {
            Notice::Relevant
        }
        Ok(_) => Notice::Irrelevant,
        Err(error) => Notice::Failed(error),
    }
}

pub(crate) fn watcher(
    mut relevant: impl FnMut(&Path) -> bool + Send + 'static,
    mut signal: impl FnMut(Notice) + Send + 'static,
) -> notify::Result<notify::RecommendedWatcher> {
    notify::recommended_watcher(move |event| signal(classify(event, &mut relevant)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_rescan_notice_still_requires_a_refresh() {
        let event =
            notify::Event::new(notify::EventKind::Other).set_flag(notify::event::Flag::Rescan);
        assert!(matches!(classify(Ok(event), |_| false), Notice::Relevant));
    }

    #[test]
    fn a_watcher_failure_is_never_discarded_as_an_unrelated_path() {
        let error = notify::Error::generic("the event stream stopped");
        assert!(matches!(classify(Err(error), |_| false), Notice::Failed(_)));
    }
}
