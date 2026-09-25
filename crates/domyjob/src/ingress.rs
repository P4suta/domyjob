use serde::de::DeserializeOwned;

pub trait Ingress: DeserializeOwned {}

pub fn json<T: Ingress>(bytes: &[u8]) -> Result<T, serde_json::Error> {
    serde_json::from_slice(bytes)
}

pub fn json_text<T: Ingress>(text: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(text)
}

pub fn json_value<T: Ingress>(value: &serde_json::Value) -> Result<T, serde_json::Error> {
    T::deserialize(value)
}

pub fn foreign_json_envelope(text: &str) -> Result<serde_json::Value, serde_json::Error> {
    serde_json::from_str(text)
}

pub fn toml<T: Ingress>(text: &str) -> Result<T, toml::de::Error> {
    toml::from_str(text)
}

impl<K: Ingress + Ord, V: Ingress> Ingress for std::collections::BTreeMap<K, V> {}
impl<T: Ingress> Ingress for Vec<T> {}
impl Ingress for String {}
impl Ingress for u8 {}
