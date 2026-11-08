#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildUse {
    pub current: bool,
    pub pinned: bool,
    pub running: bool,
}

impl BuildUse {
    #[must_use]
    pub const fn discard(self) -> bool {
        !self.current && !self.pinned && !self.running
    }
}

#[cfg(kani)]
mod proofs {
    use super::BuildUse;

    #[kani::proof]
    fn only_superseded_idle_builds_are_discarded() {
        let current: bool = kani::any();
        let pinned: bool = kani::any();
        let running: bool = kani::any();
        let discarded = BuildUse {
            current,
            pinned,
            running,
        }
        .discard();
        assert_eq!(discarded, !(current || pinned || running));
        assert!(!running || !discarded);
        kani::cover!(discarded);
        kani::cover!(running && !discarded);
    }

    #[kani::proof]
    fn rejects_active_build_removal() {
        kani::assert(
            BuildUse {
                current: false,
                pinned: false,
                running: true,
            }
            .discard(),
            "NEGATIVE_BUILD_CONTROL",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::BuildUse;

    #[test]
    fn build_lifetime_depends_on_ownership_instead_of_history() {
        for current in [false, true] {
            for pinned in [false, true] {
                for running in [false, true] {
                    assert_eq!(
                        BuildUse {
                            current,
                            pinned,
                            running
                        }
                        .discard(),
                        !(current || pinned || running)
                    );
                }
            }
        }
    }
}
