use std::marker::PhantomData;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Repository;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApprovedRepository;

trait IngressOrigin {}

impl IngressOrigin for Repository {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Labeled<T, Origin> {
    value: T,
    origin: PhantomData<fn() -> Origin>,
}

impl<'de, T: Deserialize<'de>, Origin: IngressOrigin> Deserialize<'de> for Labeled<T, Origin> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self {
            value: T::deserialize(deserializer)?,
            origin: PhantomData,
        })
    }
}

impl<T: Serialize, Origin> Serialize for Labeled<T, Origin> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}

impl Labeled<String, Repository> {
    #[must_use]
    pub(crate) fn after_project_approval(
        &self,
        _targets: &crate::project::ApprovedProjectTargets,
    ) -> Labeled<String, ApprovedRepository> {
        Labeled {
            value: self.value.clone(),
            origin: PhantomData,
        }
    }
}

impl Labeled<String, ApprovedRepository> {
    #[must_use]
    pub(crate) fn as_str(&self) -> &str {
        &self.value
    }

    #[must_use]
    pub(crate) fn into_approved_string(self) -> String {
        self.value
    }
}
