#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum NamespaceMatch {
    Same,
    Foreign,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub struct Binding {
    pub job_matches: bool,
    pub namespace: NamespaceMatch,
    pub identity_matches: bool,
    pub slot_matches: bool,
}

#[must_use]
pub const fn authorize(binding: Binding) -> bool {
    binding.job_matches
        && matches!(binding.namespace, NamespaceMatch::Same)
        && binding.identity_matches
        && binding.slot_matches
}

#[cfg(kani)]
mod proofs {
    use super::{Binding, NamespaceMatch, authorize};

    #[kani::proof]
    #[kani::unwind(2)]
    fn scope_binding_requires_matching_job_identity_and_slot() {
        let binding: Binding = kani::any();
        let authorized = authorize(binding);
        assert_eq!(
            authorized,
            binding.job_matches
                && matches!(binding.namespace, NamespaceMatch::Same)
                && binding.identity_matches
                && binding.slot_matches
        );
        kani::cover!(authorized);
        kani::cover!(!binding.job_matches);
        kani::cover!(binding.namespace == NamespaceMatch::Foreign);
        kani::cover!(!binding.identity_matches);
        kani::cover!(!binding.slot_matches);
    }

    #[kani::proof]
    #[kani::unwind(2)]
    fn rejects_foreign_scope_binding() {
        kani::assert(
            authorize(Binding {
                job_matches: true,
                namespace: NamespaceMatch::Foreign,
                identity_matches: true,
                slot_matches: true,
            }),
            "NEGATIVE_SCOPE_BINDING_CONTROL",
        );
    }
}
