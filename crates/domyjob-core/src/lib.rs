#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

macro_rules! validated_string {
    ($(#[$doc:meta])* $name:ident, $error:ty, $check:expr) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl TryFrom<String> for $name {
            type Error = $error;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                let check: fn(&str) -> Result<(), $error> = $check;
                check(&value)?;
                Ok(Self(value))
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl $name {
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

pub mod chat;
pub mod chat_wire;
pub mod domain;
pub mod ingress;
pub mod jsonc;
pub mod mcp_clients;
pub mod service;
pub mod state;
pub mod wire;
