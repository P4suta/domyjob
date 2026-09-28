#![expect(
    clippy::disallowed_types,
    reason = "the ingress boundary alone owns the editable TOML parser type"
)]

use serde::Deserialize;
use serde::de::DeserializeOwned;

pub(crate) fn bounded_vec<'de, D, T>(
    deserializer: D,
    limit: usize,
    kind: &'static str,
) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Bounded<T> {
        limit: usize,
        kind: &'static str,
        marker: std::marker::PhantomData<T>,
    }

    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Bounded<T> {
        type Value = Vec<T>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "at most {} {}", self.limit, self.kind)
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Self::Value, A::Error> {
            let mut items = Vec::with_capacity(self.limit);
            while items.len() < self.limit {
                match sequence.next_element()? {
                    Some(item) => items.push(item),
                    None => return Ok(items),
                }
            }
            if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(<A::Error as serde::de::Error>::custom(format!(
                    "too many {}",
                    self.kind
                )));
            }
            Ok(items)
        }
    }

    deserializer.deserialize_seq(Bounded {
        limit,
        kind,
        marker: std::marker::PhantomData,
    })
}

pub trait Ingress: DeserializeOwned {}

#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct PeerRequest(crate::protocol::Request);

impl Ingress for PeerRequest {}

impl PeerRequest {
    #[must_use]
    pub(crate) const fn nature(&self) -> crate::authz::Nature {
        crate::authz::nature(&self.0)
    }

    #[must_use]
    pub(crate) fn audit(&self) -> (crate::authz::Nature, Option<String>) {
        use crate::protocol::Request;
        let request = &self.0;
        let subject = match request {
            Request::Kill { job }
            | Request::Status { job }
            | Request::Wait { job, .. }
            | Request::Retry { job }
            | Request::Logs { job, .. }
            | Request::Tail { job, .. }
            | Request::Digest { job, .. }
            | Request::Search { job, .. }
            | Request::Changes { job } => Some(job.to_string()),
            Request::Get { job, path } => Some(format!("{job} {path}")),
            Request::Submit { submission } => Some(submission.command.display()),
            Request::Hello
            | Request::Report
            | Request::Watch
            | Request::Clean { .. }
            | Request::Configure { .. }
            | Request::Hold
            | Request::AuditAt { .. }
            | Request::AuditHead
            | Request::List { .. } => None,
        };
        (self.nature(), subject)
    }

    #[must_use]
    pub(crate) fn into_request(self) -> crate::protocol::Request {
        self.0
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn for_test(request: crate::protocol::Request) -> Self {
        Self(request)
    }
}

#[derive(Debug)]
pub struct EditableToml(toml_edit::DocumentMut);

impl std::ops::Deref for EditableToml {
    type Target = toml_edit::Table;

    fn deref(&self) -> &Self::Target {
        self.0.as_table()
    }
}

impl std::ops::DerefMut for EditableToml {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_table_mut()
    }
}

impl std::fmt::Display for EditableToml {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "this boundary owns JSON byte decoding and requires an Ingress type"
)]
pub fn json<T: Ingress>(bytes: &[u8]) -> Result<T, serde_json::Error> {
    serde_json::from_slice(bytes)
}

#[expect(
    clippy::disallowed_methods,
    reason = "this boundary owns JSON text decoding and requires an Ingress type"
)]
pub fn json_text<T: Ingress>(text: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(text)
}

pub fn json_value<T: Ingress>(value: &serde_json::Value) -> Result<T, serde_json::Error> {
    T::deserialize(value)
}

#[expect(
    clippy::disallowed_methods,
    reason = "this boundary owns the one foreign JSON envelope decoder"
)]
pub fn foreign_json_envelope(text: &str) -> Result<serde_json::Value, serde_json::Error> {
    serde_json::from_str(text)
}

#[expect(
    clippy::disallowed_methods,
    reason = "this boundary owns TOML decoding and requires an Ingress type"
)]
pub fn toml<T: Ingress>(text: &str) -> Result<T, toml::de::Error> {
    toml::from_str(text)
}

pub fn editable_toml(text: &str) -> Result<EditableToml, toml_edit::TomlError> {
    text.parse().map(EditableToml)
}
