use crate::scope_ownership::NamespaceMatch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Completion {
    Active,
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub struct ReceiptBinding {
    pub namespace: NamespaceMatch,
    pub job_matches: bool,
    pub request_matches: bool,
    pub state_matches: bool,
    pub completion: Completion,
}

#[must_use]
pub const fn admit_receipt(binding: ReceiptBinding) -> bool {
    matches!(binding.namespace, NamespaceMatch::Same)
        && binding.job_matches
        && binding.request_matches
        && binding.state_matches
        && matches!(binding.completion, Completion::Finished)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Quiescence {
    Unknown,
    Confirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum CacheLease {
    Held,
    Released,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum CacheUses {
    Pending,
    Released,
}

#[must_use]
pub const fn cache_may_retire(superseded: bool, lease: CacheLease, uses: CacheUses) -> bool {
    superseded && matches!(lease, CacheLease::Released) && matches!(uses, CacheUses::Released)
}

#[must_use]
pub const fn cache_use_may_release(quiescence: Quiescence) -> bool {
    matches!(quiescence, Quiescence::Confirmed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidQuiescence;

pub const fn marker_quiescence(bytes: Option<u64>) -> Result<Quiescence, InvalidQuiescence> {
    match bytes {
        None => Ok(Quiescence::Unknown),
        Some(0) => Ok(Quiescence::Confirmed),
        Some(_) => Err(InvalidQuiescence),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Delivery {
    Pending,
    Acknowledged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retention {
    WorkspaceAndResult,
    Result,
    Receipt,
}

#[must_use]
pub const fn retention(
    completion: Completion,
    quiescence: Quiescence,
    delivery: Delivery,
) -> Retention {
    match (completion, quiescence, delivery) {
        (Completion::Active, _, _) | (_, Quiescence::Unknown, _) => Retention::WorkspaceAndResult,
        (Completion::Finished, Quiescence::Confirmed, Delivery::Pending) => Retention::Result,
        (Completion::Finished, Quiescence::Confirmed, Delivery::Acknowledged) => Retention::Receipt,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Completion, Delivery, InvalidQuiescence, Quiescence, Retention, marker_quiescence,
        retention,
    };

    #[test]
    fn only_empty_completion_records_confirm_quiescence() {
        assert_eq!(marker_quiescence(None), Ok(Quiescence::Unknown));
        assert_eq!(marker_quiescence(Some(0)), Ok(Quiescence::Confirmed));
        assert_eq!(marker_quiescence(Some(1)), Err(InvalidQuiescence));
        assert_eq!(marker_quiescence(Some(u64::MAX)), Err(InvalidQuiescence));
    }

    #[test]
    fn execution_and_delivery_each_control_their_own_bytes() {
        assert_eq!(
            retention(
                Completion::Active,
                Quiescence::Confirmed,
                Delivery::Acknowledged
            ),
            Retention::WorkspaceAndResult
        );
        assert_eq!(
            retention(
                Completion::Finished,
                Quiescence::Unknown,
                Delivery::Acknowledged
            ),
            Retention::WorkspaceAndResult
        );
        assert_eq!(
            retention(
                Completion::Finished,
                Quiescence::Confirmed,
                Delivery::Pending
            ),
            Retention::Result
        );
        assert_eq!(
            retention(
                Completion::Finished,
                Quiescence::Confirmed,
                Delivery::Acknowledged
            ),
            Retention::Receipt
        );
    }
}

#[cfg(kani)]
mod proofs {
    use super::{
        CacheLease, CacheUses, Completion, Delivery, InvalidQuiescence, Quiescence, Retention,
        cache_may_retire, cache_use_may_release, marker_quiescence, retention,
    };
    use super::{NamespaceMatch, ReceiptBinding, admit_receipt};

    #[kani::proof]
    fn receipt_binding_requires_the_recorded_execution() {
        let binding: ReceiptBinding = kani::any();
        let admitted = admit_receipt(binding);
        assert_eq!(
            admitted,
            binding.namespace == NamespaceMatch::Same
                && binding.job_matches
                && binding.request_matches
                && binding.state_matches
                && binding.completion == Completion::Finished
        );
        kani::cover!(admitted);
        kani::cover!(binding.namespace == NamespaceMatch::Foreign && !admitted);
        kani::cover!(!binding.job_matches && !admitted);
        kani::cover!(!binding.request_matches && !admitted);
        kani::cover!(!binding.state_matches && !admitted);
        kani::cover!(binding.completion == Completion::Active && !admitted);
    }

    #[kani::proof]
    fn rejects_a_foreign_completion_receipt() {
        kani::assert(
            admit_receipt(ReceiptBinding {
                namespace: NamespaceMatch::Same,
                job_matches: false,
                request_matches: true,
                state_matches: true,
                completion: Completion::Finished,
            }),
            "NEGATIVE_RECEIPT_BINDING_CONTROL",
        );
    }

    #[kani::proof]
    fn cache_retirement_requires_idle_quiesced_users() {
        let superseded: bool = kani::any();
        let lease: CacheLease = kani::any();
        let uses: CacheUses = kani::any();
        let quiescence: Quiescence = kani::any();
        let discarded = cache_may_retire(superseded, lease, uses);
        assert_eq!(
            discarded,
            superseded && lease == CacheLease::Released && uses == CacheUses::Released
        );
        assert_eq!(
            cache_use_may_release(quiescence),
            quiescence == Quiescence::Confirmed
        );
        kani::cover!(discarded);
        kani::cover!(lease == CacheLease::Held && !discarded);
        kani::cover!(lease == CacheLease::Released && uses == CacheUses::Pending && !discarded);
        kani::cover!(!cache_use_may_release(quiescence));
    }

    #[kani::proof]
    fn rejects_cache_removal_with_unconfirmed_usage() {
        kani::assert(
            cache_may_retire(true, CacheLease::Released, CacheUses::Pending),
            "NEGATIVE_CACHE_USAGE_CONTROL",
        );
    }

    #[kani::proof]
    #[kani::unwind(2)]
    fn marker_confirmation_requires_an_empty_record() {
        let bytes: Option<u64> = kani::any();
        let decoded = marker_quiescence(bytes);
        match bytes {
            None => assert_eq!(decoded, Ok(Quiescence::Unknown)),
            Some(0) => assert_eq!(decoded, Ok(Quiescence::Confirmed)),
            Some(_) => assert_eq!(decoded, Err(InvalidQuiescence)),
        }
        kani::cover!(bytes.is_none());
        kani::cover!(bytes == Some(0));
        kani::cover!(bytes.is_some_and(|length| length > 0));
    }

    #[kani::proof]
    #[kani::unwind(2)]
    fn rejects_nonempty_quiescence_confirmation() {
        kani::assert(
            marker_quiescence(Some(1)) == Ok(Quiescence::Confirmed),
            "NEGATIVE_QUIESCENCE_CONTROL",
        );
    }

    #[kani::proof]
    fn storage_is_retired_only_after_execution_and_delivery() {
        let completion: Completion = kani::any();
        let quiescence: Quiescence = kani::any();
        let delivery: Delivery = kani::any();
        let retained = retention(completion, quiescence, delivery);
        assert_eq!(
            retained != Retention::WorkspaceAndResult,
            completion == Completion::Finished && quiescence == Quiescence::Confirmed
        );
        assert_eq!(
            retained == Retention::Receipt,
            completion == Completion::Finished
                && quiescence == Quiescence::Confirmed
                && delivery == Delivery::Acknowledged
        );
        kani::cover!(retained == Retention::WorkspaceAndResult);
        kani::cover!(retained == Retention::Result);
        kani::cover!(retained == Retention::Receipt);
    }

    #[kani::proof]
    fn rejects_unacknowledged_result_loss() {
        kani::assert(
            retention(
                Completion::Finished,
                Quiescence::Confirmed,
                Delivery::Pending,
            ) == Retention::Receipt,
            "NEGATIVE_STORAGE_CONTROL",
        );
    }
}
