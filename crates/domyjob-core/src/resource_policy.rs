use alloc::string::String;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::resource_budget::Budget;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawPolicy", into = "RawPolicy")]
pub struct Policy(Budget);

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    version: u32,
    max_concurrent_jobs: u32,
    slice: String,
    memory_high_bytes: u64,
    memory_max_bytes: u64,
    memory_swap_max_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error(
    "resource policy must use version 1, domyjob.slice, 1–2 jobs, 0 < high < max <= 11 GiB, and swap <= 2 GiB"
)]
pub struct InvalidPolicy;

impl TryFrom<RawPolicy> for Policy {
    type Error = InvalidPolicy;

    fn try_from(raw: RawPolicy) -> Result<Self, Self::Error> {
        let budget = Budget {
            concurrent: raw.max_concurrent_jobs,
            high: raw.memory_high_bytes,
            max: raw.memory_max_bytes,
            swap: raw.memory_swap_max_bytes,
        };
        if raw.version == 1 && raw.slice == "domyjob.slice" && budget.valid() {
            Ok(Self(budget))
        } else {
            Err(InvalidPolicy)
        }
    }
}

impl From<Policy> for RawPolicy {
    fn from(policy: Policy) -> Self {
        Self {
            version: 1,
            max_concurrent_jobs: policy.0.concurrent,
            slice: String::from("domyjob.slice"),
            memory_high_bytes: policy.0.high,
            memory_max_bytes: policy.0.max,
            memory_swap_max_bytes: policy.0.swap,
        }
    }
}

impl Policy {
    #[must_use]
    pub const fn budget(self) -> Budget {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::{Policy, RawPolicy};
    use alloc::string::String;

    fn raw() -> RawPolicy {
        RawPolicy {
            version: 1,
            max_concurrent_jobs: 2,
            slice: String::from("domyjob.slice"),
            memory_high_bytes: 8 * 1024 * 1024 * 1024,
            memory_max_bytes: 10 * 1024 * 1024 * 1024,
            memory_swap_max_bytes: 2 * 1024 * 1024 * 1024,
        }
    }

    #[test]
    fn invalid_budgets_and_policy_shapes_cannot_grant_admission() {
        let policy = Policy::try_from(raw()).unwrap();
        let encoded = serde_json::to_string(&policy).unwrap();
        assert_eq!(
            crate::ingress::foreign_json::<Policy>(&encoded).unwrap(),
            policy
        );
        for invalid in [
            RawPolicy {
                version: 2,
                ..raw()
            },
            RawPolicy {
                max_concurrent_jobs: 0,
                ..raw()
            },
            RawPolicy {
                max_concurrent_jobs: 3,
                ..raw()
            },
            RawPolicy {
                memory_high_bytes: 0,
                ..raw()
            },
            RawPolicy {
                memory_high_bytes: u64::MAX,
                ..raw()
            },
            RawPolicy {
                memory_max_bytes: u64::MAX,
                ..raw()
            },
            RawPolicy {
                memory_swap_max_bytes: u64::MAX,
                ..raw()
            },
            RawPolicy {
                slice: String::from("unlimited.slice"),
                ..raw()
            },
        ] {
            Policy::try_from(invalid).unwrap_err();
        }
        let unknown = encoded.replace("\"version\":1", "\"version\":1,\"disable\":true");
        crate::ingress::foreign_json::<Policy>(&unknown).unwrap_err();
    }
}
