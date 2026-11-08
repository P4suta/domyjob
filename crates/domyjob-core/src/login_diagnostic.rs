#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum RequestPhase {
    Read,
    Decode,
    Validate,
    Launch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum ReplyPhase {
    Write,
    ShutdownWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    Request {
        phase: RequestPhase,
        reply: Option<ReplyPhase>,
    },
    Reply(ReplyPhase),
}

#[must_use]
pub const fn select_failure(
    request: Option<RequestPhase>,
    reply: Option<ReplyPhase>,
) -> Option<Failure> {
    match request {
        Some(phase) => Some(Failure::Request { phase, reply }),
        None => match reply {
            Some(phase) => Some(Failure::Reply(phase)),
            None => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{Failure, ReplyPhase, RequestPhase, select_failure};

    #[test]
    fn every_receive_disposition_preserves_its_first_failure_and_secondary_phase() {
        let requests = [
            None,
            Some(RequestPhase::Read),
            Some(RequestPhase::Decode),
            Some(RequestPhase::Validate),
            Some(RequestPhase::Launch),
        ];
        let replies = [
            None,
            Some(ReplyPhase::Write),
            Some(ReplyPhase::ShutdownWrite),
        ];
        for request in requests {
            for reply in replies {
                let selected = select_failure(request, reply);
                match (request, reply) {
                    (Some(phase), _) => {
                        assert_eq!(selected, Some(Failure::Request { phase, reply }));
                    }
                    (None, Some(phase)) => assert_eq!(selected, Some(Failure::Reply(phase))),
                    (None, None) => assert_eq!(selected, None),
                }
            }
        }
    }
}

#[cfg(kani)]
mod proofs {
    use super::{Failure, ReplyPhase, RequestPhase, select_failure};

    #[kani::proof]
    #[kani::unwind(2)]
    fn first_failure_retains_its_request_phase_and_reply_context() {
        let request: Option<RequestPhase> = kani::any();
        let reply: Option<ReplyPhase> = kani::any();
        let selected = select_failure(request, reply);
        match request {
            Some(phase) => assert_eq!(selected, Some(Failure::Request { phase, reply })),
            None => match reply {
                Some(phase) => assert_eq!(selected, Some(Failure::Reply(phase))),
                None => assert_eq!(selected, None),
            },
        }
        kani::cover!(request.is_none() && reply.is_none());
        kani::cover!(request.is_some() && reply.is_none());
        kani::cover!(request.is_some() && reply.is_some());
        kani::cover!(request.is_none() && reply == Some(ReplyPhase::Write));
        kani::cover!(request.is_none() && reply == Some(ReplyPhase::ShutdownWrite));
    }

    #[kani::proof]
    #[kani::unwind(2)]
    fn rejects_reply_failure_overwriting_request() {
        kani::assert(
            select_failure(Some(RequestPhase::Decode), Some(ReplyPhase::Write))
                == Some(Failure::Reply(ReplyPhase::Write)),
            "NEGATIVE_LOGIN_DIAGNOSTIC_CONTROL",
        );
    }
}
