use core::num::NonZeroI32;
use core::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessGroup(NonZeroI32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid process group")]
pub struct InvalidProcessGroup;

impl ProcessGroup {
    pub const fn new(value: i32) -> Result<Self, InvalidProcessGroup> {
        match NonZeroI32::new(value) {
            Some(group) if group.get() > 0 => Ok(Self(group)),
            Some(_) | None => Err(InvalidProcessGroup),
        }
    }

    #[must_use]
    pub const fn get(self) -> i32 {
        self.0.get()
    }
}

impl FromStr for ProcessGroup {
    type Err = InvalidProcessGroup;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::new(text.parse().map_err(|_invalid| InvalidProcessGroup)?)
    }
}

#[cfg(kani)]
mod proofs {
    use super::ProcessGroup;

    #[kani::proof]
    fn only_positive_identifiers_are_admitted() {
        let value: i32 = kani::any();
        let group = ProcessGroup::new(value);
        assert_eq!(group.is_ok(), value > 0);
        if let Ok(group) = group {
            assert_eq!(group.get(), value);
        }
        kani::cover!(value > 0);
        kani::cover!(value < 0);
        kani::cover!(value == 0);
    }

    #[kani::proof]
    fn rejects_unowned_process_groups() {
        kani::assert(
            ProcessGroup::new(0).is_ok(),
            "NEGATIVE_PROCESS_GROUP_CONTROL",
        );
    }
}
