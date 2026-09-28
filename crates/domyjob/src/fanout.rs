#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panicked;

const MAX_WORKERS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooMany {
    pub actual: usize,
    pub limit: usize,
}

#[derive(Debug)]
pub struct ConcurrentBatch<'a, T, const MAX: usize> {
    items: &'a [T],
}

impl<'a, T, const MAX: usize> ConcurrentBatch<'a, T, MAX> {
    pub const fn new(items: &'a [T]) -> Result<Self, TooMany> {
        if items.len() > MAX {
            return Err(TooMany {
                actual: items.len(),
                limit: MAX,
            });
        }
        Ok(Self { items })
    }

    #[must_use]
    pub const fn items(&self) -> &'a [T] {
        self.items
    }
}

pub fn try_arrivals<T: Sync, R: Send, E>(
    items: &[T],
    work: impl Fn(&T) -> R + Sync,
    mut each: impl FnMut(&T, Result<R, Panicked>) -> Result<(), E>,
) -> Result<(), E> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let (done, arrived) = std::sync::mpsc::sync_channel(MAX_WORKERS);
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
            if let Some(item) = items.get(index)
                && let Err(error) = each(item, result)
            {
                next.store(items.len(), std::sync::atomic::Ordering::Relaxed);
                return Err(error);
            }
        }
        Ok(())
    })
}

pub fn arrivals<T: Sync, R: Send>(
    items: &[T],
    work: impl Fn(&T) -> R + Sync,
    mut each: impl FnMut(&T, Result<R, Panicked>),
) {
    let result: Result<(), std::convert::Infallible> = try_arrivals(items, work, |item, result| {
        each(item, result);
        Ok(())
    });
    match result {
        Ok(()) => {}
        Err(never) => match never {},
    }
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

    #[test]
    fn a_fallible_consumer_stops_receiving_after_its_first_error() {
        let items = [1, 2, 3];
        let mut received = 0;
        let outcome = try_arrivals(
            &items,
            |item| item * 2,
            |_item, _result| {
                received += 1;
                Err("stop")
            },
        );
        assert_eq!(outcome, Err("stop"));
        assert_eq!(received, 1);
    }

    #[test]
    fn a_concurrent_batch_requires_its_worker_limit_before_spawning() {
        let items = [1, 2, 3];
        assert_eq!(
            ConcurrentBatch::<_, 2>::new(&items).err(),
            Some(TooMany {
                actual: 3,
                limit: 2,
            })
        );
        assert_eq!(ConcurrentBatch::<_, 3>::new(&items).unwrap().items(), items);
    }
}
