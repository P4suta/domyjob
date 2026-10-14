#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffPhase {
    Read,
    Decode,
    Validate,
    Launch,
}

#[derive(Debug)]
pub struct HandoffFailure<E> {
    pub phase: HandoffPhase,
    pub cause: E,
}

pub fn start<B, T, E>(
    read: impl FnOnce() -> Result<B, E>,
    decode: impl FnOnce(B) -> Result<T, E>,
    validate: impl FnOnce(&T) -> Result<(), E>,
    launch: impl FnOnce(T) -> Result<(), E>,
) -> Result<(), HandoffFailure<E>> {
    let bytes = read().map_err(|cause| HandoffFailure {
        phase: HandoffPhase::Read,
        cause,
    })?;
    let request = decode(bytes).map_err(|cause| HandoffFailure {
        phase: HandoffPhase::Decode,
        cause,
    })?;
    validate(&request).map_err(|cause| HandoffFailure {
        phase: HandoffPhase::Validate,
        cause,
    })?;
    launch(request).map_err(|cause| HandoffFailure {
        phase: HandoffPhase::Launch,
        cause,
    })
}

#[cfg(test)]
mod tests {
    use super::{HandoffPhase, start};
    use alloc::string::String;
    use core::cell::Cell;

    #[test]
    fn refusals_preserve_the_first_phase_owned_error_and_call_order() {
        const READ_STEP: u8 = 0;
        const DECODE_STEP: u8 = 1;
        const VALIDATE_STEP: u8 = 2;
        const LAUNCH_STEP: u8 = 3;
        const COMPLETE_STEP: u8 = 4;
        const INPUT: u16 = 513;
        for refused in [
            None,
            Some(HandoffPhase::Read),
            Some(HandoffPhase::Decode),
            Some(HandoffPhase::Validate),
            Some(HandoffPhase::Launch),
        ] {
            let step = Cell::new(READ_STEP);
            let run = |phase, current, next| {
                assert_eq!(step.get(), current);
                step.set(next);
                if refused == Some(phase) {
                    Err(String::from("owned refusal"))
                } else {
                    Ok(())
                }
            };
            let result = start(
                || {
                    run(HandoffPhase::Read, READ_STEP, DECODE_STEP)?;
                    Ok(INPUT)
                },
                |bytes| {
                    assert_eq!(bytes, INPUT);
                    run(HandoffPhase::Decode, DECODE_STEP, VALIDATE_STEP)?;
                    Ok(bytes.to_le_bytes())
                },
                |request| {
                    assert_eq!(*request, INPUT.to_le_bytes());
                    run(HandoffPhase::Validate, VALIDATE_STEP, LAUNCH_STEP)
                },
                |request| {
                    assert_eq!(request, INPUT.to_le_bytes());
                    run(HandoffPhase::Launch, LAUNCH_STEP, COMPLETE_STEP)
                },
            );
            let completed = match refused {
                None | Some(HandoffPhase::Launch) => COMPLETE_STEP,
                Some(HandoffPhase::Read) => DECODE_STEP,
                Some(HandoffPhase::Decode) => VALIDATE_STEP,
                Some(HandoffPhase::Validate) => LAUNCH_STEP,
            };
            assert_eq!(step.get(), completed);
            assert_eq!(result.as_ref().err().map(|error| error.phase), refused);
            if let Err(error) = result {
                assert_eq!(error.cause, "owned refusal");
            }
        }
    }
}
