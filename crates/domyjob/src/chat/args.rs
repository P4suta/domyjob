//! Arguments shared by the chat CLI and the MCP tools.
//!
//! Each struct parses both as clap arguments and as strict MCP JSON, so both surfaces accept the same shapes.

use clap::{Args, ValueEnum};
use domyjob_core::chat::card::{Access, Tool};
use schemars::JsonSchema;
use serde::Deserialize;

/// An AI command-line client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ToolArg {
    Claude,
    Codex,
    Opencode,
}

impl From<ToolArg> for Tool {
    fn from(tool: ToolArg) -> Self {
        match tool {
            ToolArg::Claude => Self::Claude,
            ToolArg::Codex => Self::Codex,
            ToolArg::Opencode => Self::Opencode,
        }
    }
}

/// Whether an agent may change files while it works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AccessArg {
    Read,
    Write,
}

impl From<AccessArg> for Access {
    fn from(access: AccessArg) -> Self {
        match access {
            AccessArg::Read => Self::Read,
            AccessArg::Write => Self::Write,
        }
    }
}

/// Profile fields; omitted fields keep their current value.
#[derive(Debug, Clone, Default, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProfileArgs {
    /// The name other agents see.
    #[arg(long)]
    #[serde(default)]
    pub(crate) display_name: Option<String>,
    /// A short role such as "reviewer" or "release builder".
    #[arg(long)]
    #[serde(default)]
    pub(crate) role: Option<String>,
    /// What others may ask this agent to do.
    #[arg(long)]
    #[serde(default)]
    pub(crate) description: Option<String>,
    /// Skill tags such as rust, windows, or security; repeat the flag for several.
    #[arg(long = "skill")]
    #[serde(default)]
    pub(crate) skills: Option<Vec<String>>,
    /// The project the agent works on.
    #[arg(long)]
    #[serde(default)]
    pub(crate) project: Option<String>,
    /// A short status, such as what the agent is doing now; an empty value clears it.
    #[arg(long)]
    #[serde(default)]
    pub(crate) status: Option<String>,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct JoinArgs {
    /// This session's agent name: lowercase letters, digits, `.`, `-`, or `_`.
    pub(crate) name: String,
    /// The AI client of this session; MCP clients that identify themselves may omit it.
    #[arg(long, value_enum)]
    #[serde(default)]
    pub(crate) tool: Option<ToolArg>,
    /// What this session may change.
    #[arg(long, value_enum)]
    #[serde(default)]
    pub(crate) access: Option<AccessArg>,
    #[command(flatten)]
    #[serde(default)]
    pub(crate) profile: ProfileArgs,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StartArgs {
    /// The new managed agent's name.
    pub(crate) name: String,
    /// The AI client that runs its turns.
    #[arg(long, value_enum)]
    pub(crate) tool: ToolArg,
    /// The absolute working directory of its turns.
    #[arg(long)]
    pub(crate) cwd: String,
    /// Whether its turns may change files; read-only by default.
    #[arg(long, value_enum)]
    #[serde(default)]
    pub(crate) access: Option<AccessArg>,
    /// An existing session ID of the same client to continue.
    #[arg(long)]
    #[serde(default)]
    pub(crate) resume: Option<String>,
    #[command(flatten)]
    #[serde(default)]
    pub(crate) profile: ProfileArgs,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateArgs {
    /// The local agent to change.
    pub(crate) name: String,
    /// A new absolute working directory.
    #[arg(long)]
    #[serde(default)]
    pub(crate) cwd: Option<String>,
    /// Start its next turn in a new client session.
    #[arg(long)]
    #[serde(default)]
    pub(crate) reset_session: bool,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct NameArgs {
    /// A local agent or room name.
    pub(crate) name: String,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DirectoryArgs {
    /// Words to match against names, roles, skills, projects, and descriptions.
    #[serde(default)]
    pub(crate) query: Option<String>,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RoomArgs {
    /// The new room's name.
    pub(crate) name: String,
    /// Agents to add besides you, as NAME or NAME@MACHINE.
    pub(crate) members: Vec<String>,
    /// What the room is for.
    #[arg(long)]
    #[serde(default)]
    pub(crate) topic: Option<String>,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemberArgs {
    /// A room owned by this machine.
    pub(crate) room: String,
    /// The agent to add or remove.
    pub(crate) member: String,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TopicArgs {
    /// A room owned by this machine.
    pub(crate) room: String,
    /// The new topic; an empty value clears it.
    pub(crate) topic: String,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendArgs {
    /// An agent (NAME or NAME@MACHINE) for a direct message, or a room.
    pub(crate) target: String,
    /// The message.
    pub(crate) text: String,
}

pub(crate) const fn default_timeout() -> u64 {
    120
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AskArgs {
    /// An agent (NAME or NAME@MACHINE), or a room together with `to`.
    pub(crate) target: String,
    /// The question.
    pub(crate) text: String,
    /// The room member who should answer.
    #[arg(long)]
    #[serde(default)]
    pub(crate) to: Option<String>,
    /// Seconds to wait for the ending, at most 600; 0 returns at once.
    #[arg(long, default_value_t = default_timeout())]
    #[serde(default = "default_timeout")]
    pub(crate) timeout: u64,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplyArgs {
    /// The exact message ID being answered.
    pub(crate) message: String,
    /// The reply.
    pub(crate) text: String,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct MessageArgs {
    /// The exact message ID of an ask.
    pub(crate) message: String,
    /// Seconds to wait for the ending, at most 600; 0 returns at once.
    #[arg(long, default_value_t = 0)]
    #[serde(default)]
    pub(crate) timeout: u64,
}

pub(crate) const fn default_limit() -> usize {
    50
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct InboxArgs {
    /// Show read messages too.
    #[arg(long)]
    #[serde(default)]
    pub(crate) all: bool,
    /// At most this many messages, up to 500.
    #[arg(long, default_value_t = default_limit())]
    #[serde(default = "default_limit")]
    pub(crate) limit: usize,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ThreadArgs {
    /// An agent for your direct conversation, a room, or a conversation ID.
    pub(crate) target: String,
    /// At most this many events, up to 500.
    #[arg(long, default_value_t = default_limit())]
    #[serde(default = "default_limit")]
    pub(crate) limit: usize,
    /// Only events before this message ID.
    #[arg(long)]
    #[serde(default)]
    pub(crate) before: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct EmptyArgs {}
