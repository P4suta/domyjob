use super::ReadRefusal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescriptorMatch {
    Exact,
    Foreign,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderGrant {
    Named,
    Absent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOutcome {
    OriginalBody,
    Replaced,
}

#[must_use]
pub const fn observation_after_delivery(delivery: DeliveryOutcome, cancelled: bool) -> bool {
    matches!(delivery, DeliveryOutcome::OriginalBody) && !cancelled
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadFacts {
    pub project_matches: bool,
    pub scope_matches: bool,
    pub descriptor: DescriptorMatch,
    pub reader: ReaderGrant,
}

pub const fn read_decision(
    facts: ReadFacts,
    body_bytes: usize,
    limit: usize,
) -> Result<(), ReadRefusal> {
    if !facts.project_matches {
        return Err(ReadRefusal::Project);
    }
    if !facts.scope_matches {
        return Err(ReadRefusal::Scope);
    }
    if !matches!(facts.descriptor, DescriptorMatch::Exact) {
        return Err(ReadRefusal::Descriptor);
    }
    if !matches!(facts.reader, ReaderGrant::Named) {
        return Err(ReadRefusal::Reader);
    }
    if limit == 0 || limit > super::MAX_BODY_BYTES {
        return Err(ReadRefusal::Limit);
    }
    if body_bytes == 0 || body_bytes > limit {
        return Err(ReadRefusal::Body);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservationFacts {
    pub parent_matches: bool,
    pub issuer_matches: bool,
    pub audience_matches: bool,
    pub reader: ReaderGrant,
    pub parent_clock: u64,
    pub observation_clock: u64,
}

pub const fn observation_decision(facts: ObservationFacts) -> Result<(), ReadRefusal> {
    if !facts.parent_matches {
        return Err(ReadRefusal::Descriptor);
    }
    if !facts.issuer_matches || !matches!(facts.reader, ReaderGrant::Named) {
        return Err(ReadRefusal::Reader);
    }
    if !facts.audience_matches {
        return Err(ReadRefusal::Audience);
    }
    if facts.observation_clock <= facts.parent_clock {
        return Err(ReadRefusal::Order);
    }
    Ok(())
}

#[cfg(kani)]
mod proofs {
    use super::{
        DeliveryOutcome, DescriptorMatch, ObservationFacts, ReadFacts, ReaderGrant,
        observation_after_delivery, observation_decision, read_decision,
    };

    fn grant(named: bool) -> ReaderGrant {
        if named {
            ReaderGrant::Named
        } else {
            ReaderGrant::Absent
        }
    }

    #[kani::proof]
    fn only_a_delivered_uncancelled_body_records_an_observation() {
        let delivered: bool = kani::any();
        let cancelled: bool = kani::any();
        let delivery = if delivered {
            DeliveryOutcome::OriginalBody
        } else {
            DeliveryOutcome::Replaced
        };
        let admitted = observation_after_delivery(delivery, cancelled);
        assert_eq!(admitted, delivered && !cancelled);
        kani::cover!(delivered && !cancelled && admitted);
        kani::cover!(delivered && cancelled && !admitted);
        kani::cover!(!delivered && !cancelled && !admitted);
        kani::cover!(!delivered && cancelled && !admitted);
    }

    #[kani::proof]
    fn rejects_an_observation_for_a_replaced_body() {
        kani::assert(
            observation_after_delivery(DeliveryOutcome::Replaced, false),
            "NEGATIVE_HANDOFF_DELIVERY_CONTROL",
        );
    }

    #[kani::proof]
    fn reads_require_the_exact_grant_scope_and_bound() {
        let project: bool = kani::any();
        let scope: bool = kani::any();
        let descriptor: bool = kani::any();
        let reader: bool = kani::any();
        let bytes: usize = kani::any();
        let limit: usize = kani::any();
        let facts = ReadFacts {
            project_matches: project,
            scope_matches: scope,
            descriptor: if descriptor {
                DescriptorMatch::Exact
            } else {
                DescriptorMatch::Foreign
            },
            reader: grant(reader),
        };
        let result = read_decision(facts, bytes, limit);
        assert_eq!(
            result.is_ok(),
            project
                && scope
                && descriptor
                && reader
                && limit > 0
                && limit <= super::super::MAX_BODY_BYTES
                && bytes > 0
                && bytes <= limit
        );
        kani::cover!(result.is_ok());
        kani::cover!(!project && scope && descriptor && reader);
        kani::cover!(project && !scope && descriptor && reader);
        kani::cover!(project && scope && !descriptor && reader);
        kani::cover!(project && scope && descriptor && !reader);
        kani::cover!(matches!(result, Err(super::ReadRefusal::Body)) && bytes > limit);
        kani::cover!(project && scope && descriptor && reader && limit == 0);
        kani::cover!(
            matches!(result, Err(super::ReadRefusal::Limit))
                && limit > super::super::MAX_BODY_BYTES
        );
        kani::cover!(matches!(result, Err(super::ReadRefusal::Body)) && bytes == 0);
    }

    #[kani::proof]
    fn observations_require_the_actual_author_and_grant() {
        let parent: bool = kani::any();
        let issuer: bool = kani::any();
        let audience: bool = kani::any();
        let reader: bool = kani::any();
        let facts = ObservationFacts {
            parent_matches: parent,
            issuer_matches: issuer,
            audience_matches: audience,
            reader: grant(reader),
            parent_clock: kani::any(),
            observation_clock: kani::any(),
        };
        let accepted = observation_decision(facts).is_ok();
        assert_eq!(
            accepted,
            parent && issuer && audience && reader && facts.observation_clock > facts.parent_clock
        );
        kani::cover!(accepted);
        kani::cover!(parent && !issuer && audience && reader);
        kani::cover!(parent && issuer && audience && !reader);
        kani::cover!(!parent && issuer && audience && reader);
        kani::cover!(parent && issuer && !audience && reader);
        kani::cover!(
            parent && issuer && audience && reader && facts.observation_clock <= facts.parent_clock
        );
    }

    #[kani::proof]
    fn rejects_ungranted_handoff_body_access() {
        kani::assert(
            read_decision(
                ReadFacts {
                    project_matches: true,
                    scope_matches: true,
                    descriptor: DescriptorMatch::Exact,
                    reader: ReaderGrant::Absent,
                },
                1,
                1,
            )
            .is_ok(),
            "NEGATIVE_HANDOFF_READ_CONTROL",
        );
    }

    #[kani::proof]
    fn rejects_a_foreign_handoff_reader_observation() {
        kani::assert(
            observation_decision(ObservationFacts {
                parent_matches: true,
                issuer_matches: false,
                audience_matches: true,
                reader: ReaderGrant::Named,
                parent_clock: 0,
                observation_clock: 1,
            })
            .is_ok(),
            "NEGATIVE_HANDOFF_OBSERVATION_CONTROL",
        );
    }
}
