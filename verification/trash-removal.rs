#[path = "../crates/domyjob/src/state_io/removal.rs"]
mod removal;

use std::cell::Cell;

const PHASE_ATTEMPTS: usize = 4;
const ATTEMPTS: usize = PHASE_ATTEMPTS * 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, kani::Arbitrary)]
struct RemovalError {
    denied: bool,
    busy: bool,
    payload: u8,
}

fn terminal(outcomes: &[Result<(), RemovalError>; ATTEMPTS], first: usize) -> usize {
    let mut last = first;
    while last < first + PHASE_ATTEMPTS - 1 && outcomes[last].is_err_and(|error| error.busy) {
        last += 1;
    }
    last
}

#[kani::proof]
#[kani::unwind(10)]
fn trash_removal_grants_once_after_denial_with_bounded_retries() {
    assert_eq!(removal::RETRIES, PHASE_ATTEMPTS - 1);
    let outcomes: [Result<(), RemovalError>; ATTEMPTS] = kani::any();
    let grant: Result<(), RemovalError> = kani::any();
    let attempts = Cell::new(0);
    let grants = Cell::new(0);
    let granted_after = Cell::new(0);
    let pauses = Cell::new(0);
    let result = removal::after_denial(
        || {
            removal::retry(
                || {
                    let index = attempts.get();
                    attempts.set(index + 1);
                    outcomes[index]
                },
                |error| error.busy,
                || pauses.set(pauses.get() + 1),
            )
        },
        || {
            grants.set(grants.get() + 1);
            granted_after.set(attempts.get());
            grant
        },
        |error| error.denied,
    );
    let first = terminal(&outcomes, 0);
    let mut last = first;
    let mut expected_grants = 0;
    let expected = match outcomes[first] {
        Ok(()) => Ok(()),
        Err(denied) if denied.denied => {
            expected_grants = 1;
            let later = match grant {
                Ok(()) => {
                    last = terminal(&outcomes, first + 1);
                    outcomes[last]
                }
                Err(error) => Err(error),
            };
            later.map_err(|error| removal::Failure::AfterGrant { denied, error })
        }
        Err(error) => Err(removal::Failure::Removal(error)),
    };
    let retried = expected_grants == 1 && grant.is_ok();
    assert_eq!(result, expected);
    assert_eq!(attempts.get(), last + 1);
    assert_eq!(grants.get(), expected_grants);
    assert_eq!(
        granted_after.get(),
        if expected_grants == 0 { 0 } else { first + 1 }
    );
    assert_eq!(pauses.get(), if retried { last - 1 } else { last });
    assert!(attempts.get() <= ATTEMPTS);
    assert!(pauses.get() <= (PHASE_ATTEMPTS - 1) * 2);
    kani::cover!(grants.get() == 0 && result.is_ok());
    kani::cover!(granted_after.get() > 1);
    kani::cover!(grants.get() == 1 && result.is_ok());
    kani::cover!(grants.get() == 1 && grant.is_err());
    kani::cover!(retried && result.is_err());
    kani::cover!(grants.get() == 0 && result.is_err());
    kani::cover!(attempts.get() == ATTEMPTS && pauses.get() == (PHASE_ATTEMPTS - 1) * 2);
    kani::cover!(
        retried
            && matches!(result, Err(removal::Failure::AfterGrant { error, .. }) if error.denied)
    );
}

#[kani::proof]
#[kani::unwind(6)]
fn rejects_permission_grants_without_denial() {
    const BUSY: u8 = 1;
    const DENIED: u8 = 2;
    let mut attempts = 0;
    let mut grants = 0;
    let result = removal::after_denial(
        || {
            removal::retry(
                || {
                    attempts += 1;
                    if attempts == 1 { Err(BUSY) } else { Ok(()) }
                },
                |error| *error == BUSY,
                || {},
            )
        },
        || {
            grants += 1;
            Ok(())
        },
        |error| *error == DENIED,
    );
    assert!(result.is_ok());
    kani::assert(grants > 0, "NEGATIVE_TRASH_REMOVAL_CONTROL");
}
