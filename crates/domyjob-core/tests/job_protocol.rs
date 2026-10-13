use domyjob_core::job_protocol::{Action, Refusal, State, step};

#[test]
fn recorded_phases_project_to_the_protocols_execution_state() {
    use domyjob_core::state::PhaseKind;

    for (phase, expected) in [
        (PhaseKind::Accepted, State::Accepted),
        (PhaseKind::Queued, State::Active),
        (PhaseKind::Starting, State::Active),
        (PhaseKind::Running, State::Active),
        (PhaseKind::Finished, State::Finished),
    ] {
        assert_eq!(State::from(phase), expected);
    }
}

#[test]
fn resubmission_and_disconnect_preserve_the_durable_job() {
    let accepted = step(
        State::Absent,
        Action::Submit {
            same_request: false,
        },
    )
    .unwrap();
    assert_eq!(accepted, State::Accepted);
    assert_eq!(
        step(accepted, Action::Submit { same_request: true }),
        Ok(accepted)
    );
    assert_eq!(
        step(
            accepted,
            Action::Submit {
                same_request: false
            }
        ),
        Err(Refusal::Conflict)
    );
    assert_eq!(step(accepted, Action::Attach), Ok(State::Active));
    for state in [
        State::Absent,
        State::Accepted,
        State::Active,
        State::Finished,
    ] {
        assert_eq!(step(state, Action::Disconnect), Ok(state));
    }
    assert_eq!(step(State::Active, Action::Reap), Ok(State::Finished));
    assert_eq!(step(State::Finished, Action::Attach), Ok(State::Finished));
    assert_eq!(step(State::Absent, Action::Attach), Err(Refusal::Missing));
}
