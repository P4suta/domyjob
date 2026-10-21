pub(super) const RETRIES: usize = 3;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Failure<E> {
    Removal(E),
    AfterGrant { denied: E, error: E },
}

pub(super) fn after_denial<E>(
    mut remove: impl FnMut() -> Result<(), E>,
    grant: impl FnOnce() -> Result<(), E>,
    permission_denied: impl FnOnce(&E) -> bool,
) -> Result<(), Failure<E>> {
    match remove() {
        Ok(()) => Ok(()),
        Err(error) if !permission_denied(&error) => Err(Failure::Removal(error)),
        Err(denied) => grant()
            .and_then(|()| remove())
            .map_err(|error| Failure::AfterGrant { denied, error }),
    }
}

pub(super) fn retry<E>(
    mut remove: impl FnMut() -> Result<(), E>,
    busy: impl Fn(&E) -> bool,
    mut pause: impl FnMut(),
) -> Result<(), E> {
    for _ in 0..RETRIES {
        match remove() {
            Ok(()) => return Ok(()),
            Err(error) => {
                if !busy(&error) {
                    return Err(error);
                }
                pause();
            }
        }
    }
    remove()
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    const BUSY: u8 = 1;
    const DENIED: u8 = 2;
    const PERMANENT: u8 = 3;

    #[derive(Debug, PartialEq, Eq)]
    enum Event {
        Remove,
        Grant,
        Pause,
    }

    #[test]
    fn permission_grants_follow_denial_after_busy_retries() {
        let events = RefCell::new(Vec::new());
        let mut outcomes = [Err(BUSY), Err(DENIED), Err(BUSY), Ok(())].into_iter();
        let result = super::after_denial(
            || {
                super::retry(
                    || {
                        events.borrow_mut().push(Event::Remove);
                        outcomes.next().unwrap()
                    },
                    |error| *error == BUSY,
                    || events.borrow_mut().push(Event::Pause),
                )
            },
            || {
                events.borrow_mut().push(Event::Grant);
                Ok(())
            },
            |error| *error == DENIED,
        );
        assert_eq!(result, Ok(()));
        assert_eq!(
            events.into_inner(),
            [
                Event::Remove,
                Event::Pause,
                Event::Remove,
                Event::Grant,
                Event::Remove,
                Event::Pause,
                Event::Remove,
            ]
        );
    }

    #[test]
    fn grant_failure_preserves_both_errors_without_another_removal() {
        let mut attempts = 0_usize;
        let result = super::after_denial(
            || {
                attempts = attempts.saturating_add(1);
                Err(DENIED)
            },
            || Err(BUSY),
            |error| *error == DENIED,
        );
        assert_eq!(
            result,
            Err(super::Failure::AfterGrant {
                denied: DENIED,
                error: BUSY,
            })
        );
        assert_eq!(attempts, 1);
    }

    #[test]
    fn later_denials_and_permanent_errors_do_not_repeat_the_grant() {
        for later in [DENIED, PERMANENT] {
            let mut outcomes = [Err(DENIED), Err(later)].into_iter();
            let mut grants = 0_usize;
            let result = super::after_denial(
                || outcomes.next().unwrap(),
                || {
                    grants = grants.saturating_add(1);
                    Ok(())
                },
                |error| *error == DENIED,
            );
            assert_eq!(
                result,
                Err(super::Failure::AfterGrant {
                    denied: DENIED,
                    error: later,
                })
            );
            assert_eq!(grants, 1);
            assert_eq!(outcomes.len(), 0);
        }
    }

    #[test]
    fn permanent_errors_skip_the_grant_and_keep_the_original_error() {
        let result = super::after_denial(
            || Err(PERMANENT),
            || panic!("a permanent error must not grant permissions"),
            |error| *error == DENIED,
        );
        assert_eq!(result, Err(super::Failure::Removal(PERMANENT)));
    }

    #[test]
    fn busy_retries_are_bounded_before_and_after_the_single_grant() {
        let mut attempts = 0_usize;
        let mut pauses = 0_usize;
        let mut grants = 0_usize;
        let result = super::after_denial(
            || {
                super::retry(
                    || {
                        attempts = attempts.saturating_add(1);
                        Err(BUSY)
                    },
                    |error| *error == BUSY,
                    || pauses = pauses.saturating_add(1),
                )
            },
            || {
                grants = grants.saturating_add(1);
                Ok(())
            },
            |_| true,
        );
        assert_eq!(
            result,
            Err(super::Failure::AfterGrant {
                denied: BUSY,
                error: BUSY,
            })
        );
        assert_eq!(attempts, super::RETRIES.saturating_add(1).saturating_mul(2));
        assert_eq!(pauses, super::RETRIES.saturating_mul(2));
        assert_eq!(grants, 1);
    }

    #[test]
    fn retries_stop_at_success_permanent_failure_or_the_attempt_bound() {
        for transient in 0..=super::RETRIES.saturating_add(1) {
            for outcome in [Ok(()), Err(2_u8)] {
                let mut attempts = 0_usize;
                let mut pauses = 0_usize;
                let result = super::retry(
                    || {
                        attempts = attempts.saturating_add(1);
                        if attempts <= transient {
                            Err(1)
                        } else {
                            outcome
                        }
                    },
                    |error| *error == 1,
                    || pauses = pauses.saturating_add(1),
                );
                assert_eq!(pauses, transient.min(super::RETRIES));
                assert_eq!(attempts, transient.min(super::RETRIES).saturating_add(1));
                assert_eq!(
                    result,
                    if transient > super::RETRIES {
                        Err(1)
                    } else {
                        outcome
                    }
                );
            }
        }
    }
}
