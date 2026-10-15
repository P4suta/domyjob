use std::borrow::Cow;
use std::str::FromStr;

use clap::Args;
use domyjob_core::chat::event::MAX_MEMBERS;
use domyjob_core::chat::handoff::{
    Descriptor, MAX_BODY_BYTES, MAX_CONTEXT_BYTES, Project, ReadLimit, Scope, Target,
};
use domyjob_core::chat::id::{AgentId, Invalid, Text};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::Deserialize;

const MAX_DESCRIPTOR_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub(crate) struct SelectedDescriptor(pub(crate) Descriptor);

impl FromStr for SelectedDescriptor {
    type Err = domyjob_core::ingress::JsonError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        domyjob_core::ingress::json(value.as_bytes(), MAX_DESCRIPTOR_BYTES)
    }
}

impl JsonSchema for SelectedDescriptor {
    fn schema_name() -> Cow<'static, str> {
        "SelectedHandoffDescriptor".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type":"object", "additionalProperties":false,
            "required":["id","metadata","body_bytes"],
            "properties":{
                "id":{"type":"string"},
                "metadata":{
                    "type":"object", "additionalProperties":false,
                    "required":["sender","intended_recipient","project","scope","permitted_readers"],
                    "properties":{
                        "sender":{"type":"string"},
                        "intended_recipient":target_schema(),
                        "project":{"type":"string","minLength":1,"maxLength":MAX_CONTEXT_BYTES},
                        "scope":{"type":"string","minLength":1,"maxLength":MAX_CONTEXT_BYTES},
                        "permitted_readers":{"type":"array","minItems":1,"maxItems":MAX_MEMBERS,"uniqueItems":true,"items":{"type":"string"}}
                    }
                },
                "body_bytes":{"type":"integer","minimum":1,"maximum":MAX_BODY_BYTES}
            }
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub(crate) struct IntendedRecipient(pub(crate) Target);

impl FromStr for IntendedRecipient {
    type Err = domyjob_core::ingress::JsonError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        domyjob_core::ingress::json(value.as_bytes(), MAX_DESCRIPTOR_BYTES)
    }
}

fn target_schema() -> Schema {
    json_schema!({"oneOf":[
        {"type":"object","additionalProperties":false,"required":["kind","agent"],"properties":{"kind":{"const":"agent"},"agent":{"type":"string"}}},
        {"type":"object","additionalProperties":false,"required":["kind","team"],"properties":{"kind":{"const":"team"},"team":{"type":"string"}}}
    ]})
}

impl JsonSchema for IntendedRecipient {
    fn schema_name() -> Cow<'static, str> {
        "HandoffIntendedRecipient".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        target_schema()
    }
}

fn project(value: &str) -> Result<Project, Invalid> {
    Project::try_from(value.to_owned())
}
fn scope(value: &str) -> Result<Scope, Invalid> {
    Scope::try_from(value.to_owned())
}
fn agent(value: &str) -> Result<AgentId, Invalid> {
    AgentId::try_from(value.to_owned())
}
fn text(value: &str) -> Result<Text, Invalid> {
    Text::try_from(value.to_owned())
}
fn read_limit(value: &str) -> Result<ReadLimit, String> {
    let bytes = value.parse::<usize>().map_err(|error| error.to_string())?;
    ReadLimit::try_from(bytes).map_err(|error| error.to_string())
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct HandoffCreateArgs {
    #[arg(long, value_parser=project, help="The exact project of this handoff")]
    #[schemars(with = "String")]
    #[schemars(length(min=1, max=MAX_CONTEXT_BYTES), description="The exact project, at most 256 UTF-8 bytes; no control characters or surrounding whitespace.")]
    pub(crate) project: Project,
    #[arg(long, value_parser=scope, help="The exact scope of this handoff")]
    #[schemars(with = "String")]
    #[schemars(length(min=1, max=MAX_CONTEXT_BYTES), description="The exact scope, at most 256 UTF-8 bytes; no control characters or surrounding whitespace.")]
    pub(crate) scope: Scope,
    #[arg(
        long = "recipient",
        help = "JSON agent or team metadata; it does not implicitly grant access"
    )]
    pub(crate) intended_recipient: IntendedRecipient,
    #[arg(long="reader", value_parser=agent, required=true, help="An explicitly permitted full agent identity; repeat for each reader")]
    #[schemars(with = "Vec<String>")]
    #[schemars(length(min=1, max=MAX_MEMBERS), extend("uniqueItems"=true))]
    pub(crate) permitted_readers: Vec<AgentId>,
    #[arg(value_parser=text, help="The scoped handoff body")]
    #[schemars(with = "String")]
    #[schemars(length(min=1, max=MAX_BODY_BYTES), description="A nonblank body of at most 65536 UTF-8 bytes without NUL.")]
    pub(crate) text: Text,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct HandoffListArgs {
    #[arg(long, value_parser=project)]
    #[schemars(with = "String")]
    #[schemars(length(min=1, max=MAX_CONTEXT_BYTES))]
    pub(crate) project: Project,
    #[arg(long, value_parser=scope)]
    #[schemars(with = "String")]
    #[schemars(length(min=1, max=MAX_CONTEXT_BYTES))]
    pub(crate) scope: Scope,
    #[arg(long)]
    #[serde(default)]
    #[schemars(range(min = 1, max = 32))]
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct HandoffReadArgs {
    #[arg(long, help = "The exact metadata descriptor returned by handoff list")]
    pub(crate) descriptor: SelectedDescriptor,
    #[arg(long, value_parser=read_limit, help="The maximum UTF-8 body bytes to read")]
    #[schemars(with = "usize")]
    #[schemars(range(min=1, max=MAX_BODY_BYTES))]
    pub(crate) max_bytes: ReadLimit,
}
