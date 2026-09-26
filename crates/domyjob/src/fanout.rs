#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panicked;

pub fn arrivals<T: Sync, R: Send>(
    items: &[T],
    work: impl Fn(&T) -> R + Sync,
    mut each: impl FnMut(&T, Result<R, Panicked>),
) {
    std::thread::scope(|scope| {
        let (done, arrived) = std::sync::mpsc::channel();
        for (index, item) in items.iter().enumerate() {
            let (work, done) = (&work, done.clone());
            scope.spawn(move || {
                let result =
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(item))) {
                        Ok(result) => Ok(result),
                        Err(_payload) => Err(Panicked),
                    };
                match done.send((index, result)) {
                    Ok(()) | Err(_) => {}
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
}
