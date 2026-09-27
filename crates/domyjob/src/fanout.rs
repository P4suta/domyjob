#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panicked;

const MAX_WORKERS: usize = 16;

pub fn arrivals<T: Sync, R: Send>(
    items: &[T],
    work: impl Fn(&T) -> R + Sync,
    mut each: impl FnMut(&T, Result<R, Panicked>),
) {
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let (done, arrived) = std::sync::mpsc::channel();
        for _ in 0..items.len().min(MAX_WORKERS) {
            let (work, done) = (&work, done.clone());
            let next = &next;
            scope.spawn(move || {
                while let Ok(index) = next.fetch_update(
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |index| {
                        if index < items.len() {
                            index.checked_add(1)
                        } else {
                            None
                        }
                    },
                ) {
                    let Some(item) = items.get(index) else {
                        break;
                    };
                    let result =
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(item)))
                        {
                            Ok(result) => Ok(result),
                            Err(_payload) => Err(Panicked),
                        };
                    match done.send((index, result)) {
                        Ok(()) | Err(_) => {}
                    }
                }
            });
        }
        drop(done);
        for (index, result) in arrived {
            if let Some(item) = items.get(index) {
                each(item, result);
            }
        }
    });
}

pub fn gathered<T: Sync, R: Send>(
    items: &[T],
    work: impl Fn(&T) -> R + Sync,
) -> Vec<Result<R, Panicked>> {
    let mut slots: Vec<Option<Result<R, Panicked>>> = items.iter().map(|_| None).collect();
    let positions: Vec<usize> = (0..items.len()).collect();
    arrivals(
        &positions,
        |position| items.get(*position).map(&work),
        |position, result| {
            if let Some(slot) = slots.get_mut(*position) {
                *slot = Some(match result {
                    Ok(Some(done)) => Ok(done),
                    Ok(None) | Err(Panicked) => Err(Panicked),
                });
            }
        },
    );
    slots
        .into_iter()
        .map(|slot| slot.unwrap_or(Err(Panicked)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_item_answers_once_in_its_own_place_and_a_panic_is_an_answer() {
        let items = [3u32, 0, 2, 1];
        let gathered = gathered(&items, |n| {
            assert!(*n != 0, "a worker panics");
            n * 10
        });
        assert_eq!(gathered, [Ok(30), Err(Panicked), Ok(20), Ok(10)]);
        let mut seen = Vec::new();
        arrivals(&items, |n| *n, |item, result| seen.push((*item, result)));
        seen.sort_unstable_by_key(|(item, _)| *item);
        assert_eq!(seen, [(0, Ok(0)), (1, Ok(1)), (2, Ok(2)), (3, Ok(3))]);
    }

    #[test]
    fn worker_count_is_bounded_independently_of_item_count() {
        let items: Vec<usize> = (0..MAX_WORKERS * 3).collect();
        let answers = gathered(&items, |_| std::thread::current().id());
        assert_eq!(answers.len(), items.len());
        let workers: std::collections::HashSet<_> =
            answers.into_iter().map(Result::unwrap).collect();
        assert!(workers.len() <= MAX_WORKERS);
    }
}
