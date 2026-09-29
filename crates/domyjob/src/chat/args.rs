use clap::{Args, ValueEnum};
use domyjob_core::chat::card::{Access, Tool};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(description = "An AI command-line client.")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(description = "Whether an agent may change files while it works.")]
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

#[derive(Debug, Clone, Default, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(description = "Profile fields; omitted fields keep their current value.")]
pub(crate) struct ProfileArgs {
    #[arg(long)]
    #[serde(default)]
    #[arg(help = "The name other agents see")]
    #[schemars(description = "The name other agents see.")]
    pub(crate) display_name: Option<String>,
    #[arg(long)]
    #[serde(default)]
    #[arg(help = "A short role such as \"reviewer\" or \"release builder\"")]
    #[schemars(description = "A short role such as \"reviewer\" or \"release builder\".")]
    pub(crate) role: Option<String>,
    #[arg(long)]
    #[serde(default)]
    #[arg(help = "What others may ask this agent to do")]
    #[schemars(description = "What others may ask this agent to do.")]
    pub(crate) description: Option<String>,
    #[arg(long = "skill")]
    #[serde(default)]
    #[arg(help = "Skill tags such as rust, windows, or security; repeat the flag for several")]
    #[schemars(
        description = "Skill tags such as rust, windows, or security; repeat the flag for several."
    )]
    pub(crate) skills: Option<Vec<String>>,
    #[arg(long)]
    #[serde(default)]
    #[arg(help = "The project the agent works on")]
    #[schemars(description = "The project the agent works on.")]
    pub(crate) project: Option<String>,
    #[arg(long)]
    #[serde(default)]
    #[arg(help = "A short status, such as what the agent is doing now; an empty value clears it")]
    #[schemars(
        description = "A short status, such as what the agent is doing now; an empty value clears it."
    )]
    pub(crate) status: Option<String>,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct JoinArgs {
    #[arg(help = "This session's agent name: lowercase letters, digits, `.`, `-`, or `_`")]
    #[schemars(
        description = "This session's agent name: lowercase letters, digits, `.`, `-`, or `_`."
    )]
    pub(crate) name: String,
    #[arg(long, value_enum)]
    #[serde(default)]
    #[arg(
        help = "The AI client of this session; MCP clients that identify themselves may omit it"
    )]
    #[schemars(
        description = "The AI client of this session; MCP clients that identify themselves may omit it."
    )]
    pub(crate) tool: Option<ToolArg>,
    #[arg(long, value_enum)]
    #[serde(default)]
    #[arg(help = "What this session may change")]
    #[schemars(description = "What this session may change.")]
    pub(crate) access: Option<AccessArg>,
    #[command(flatten)]
    #[serde(default)]
    pub(crate) profile: ProfileArgs,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StartArgs {
    #[arg(help = "The new managed agent's name")]
    #[schemars(description = "The new managed agent's name.")]
    pub(crate) name: String,
    #[arg(long, value_enum)]
    #[arg(help = "The AI client that runs its turns")]
    #[schemars(description = "The AI client that runs its turns.")]
    pub(crate) tool: ToolArg,
    #[arg(long)]
    #[arg(help = "The absolute working directory of its turns")]
    #[schemars(description = "The absolute working directory of its turns.")]
    pub(crate) cwd: String,
    #[arg(long, value_enum)]
    #[serde(default)]
    #[arg(help = "Whether its turns may change files; read-only by default")]
    #[schemars(description = "Whether its turns may change files; read-only by default.")]
    pub(crate) access: Option<AccessArg>,
    #[arg(long)]
    #[serde(default)]
    #[arg(help = "An existing session ID of the same client to continue")]
    #[schemars(description = "An existing session ID of the same client to continue.")]
    pub(crate) resume: Option<String>,
    #[command(flatten)]
    #[serde(default)]
    pub(crate) profile: ProfileArgs,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateArgs {
    #[arg(help = "The local agent to change")]
    #[schemars(description = "The local agent to change.")]
    pub(crate) name: String,
    #[arg(long)]
    #[serde(default)]
    #[arg(help = "A new absolute working directory")]
    #[schemars(description = "A new absolute working directory.")]
    pub(crate) cwd: Option<String>,
    #[arg(long)]
    #[serde(default)]
    #[arg(help = "Start its next turn in a new client session")]
    #[schemars(description = "Start its next turn in a new client session.")]
    pub(crate) reset_session: bool,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct NameArgs {
    #[arg(help = "A local agent or room name")]
    #[schemars(description = "A local agent or room name.")]
    pub(crate) name: String,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DirectoryArgs {
    #[serde(default)]
    #[arg(help = "Words to match against names, roles, skills, projects, and descriptions")]
    #[schemars(
        description = "Words to match against names, roles, skills, projects, and descriptions."
    )]
    pub(crate) query: Option<String>,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RoomArgs {
    #[arg(help = "The new room's name")]
    #[schemars(description = "The new room's name.")]
    pub(crate) name: String,
    #[arg(help = "Agents to add besides you, as NAME or NAME@MACHINE")]
    #[schemars(description = "Agents to add besides you, as NAME or NAME@MACHINE.")]
    pub(crate) members: Vec<String>,
    #[arg(long)]
    #[serde(default)]
    #[arg(help = "What the room is for")]
    #[schemars(description = "What the room is for.")]
    pub(crate) topic: Option<String>,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemberArgs {
    #[arg(help = "A room owned by this machine")]
    #[schemars(description = "A room owned by this machine.")]
    pub(crate) room: String,
    #[arg(help = "The agent to add or remove")]
    #[schemars(description = "The agent to add or remove.")]
    pub(crate) member: String,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TopicArgs {
    #[arg(help = "A room owned by this machine")]
    #[schemars(description = "A room owned by this machine.")]
    pub(crate) room: String,
    #[arg(help = "The new topic; an empty value clears it")]
    #[schemars(description = "The new topic; an empty value clears it.")]
    pub(crate) topic: String,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendArgs {
    #[arg(help = "An agent (NAME or NAME@MACHINE) for a direct message, or a room")]
    #[schemars(description = "An agent (NAME or NAME@MACHINE) for a direct message, or a room.")]
    pub(crate) target: String,
    #[arg(help = "The message")]
    #[schemars(description = "The message.")]
    pub(crate) text: String,
}

pub(crate) const fn default_timeout() -> u64 {
    120
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AskArgs {
    #[arg(help = "An agent (NAME or NAME@MACHINE), or a room together with `to`")]
    #[schemars(description = "An agent (NAME or NAME@MACHINE), or a room together with `to`.")]
    pub(crate) target: String,
    #[arg(help = "The question")]
    #[schemars(description = "The question.")]
    pub(crate) text: String,
    #[arg(long)]
    #[serde(default)]
    #[arg(help = "The room member who should answer")]
    #[schemars(description = "The room member who should answer.")]
    pub(crate) to: Option<String>,
    #[arg(long, default_value_t = default_timeout())]
    #[serde(default = "default_timeout")]
    #[arg(help = "Seconds to wait for the ending, at most 600; 0 returns at once")]
    #[schemars(description = "Seconds to wait for the ending, at most 600; 0 returns at once.")]
    pub(crate) timeout: u64,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplyArgs {
    #[arg(help = "The exact message ID being answered")]
    #[schemars(description = "The exact message ID being answered.")]
    pub(crate) message: String,
    #[arg(help = "The reply")]
    #[schemars(description = "The reply.")]
    pub(crate) text: String,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct MessageArgs {
    #[arg(help = "The exact message ID of an ask")]
    #[schemars(description = "The exact message ID of an ask.")]
    pub(crate) message: String,
    #[arg(long, default_value_t = 0)]
    #[serde(default)]
    #[arg(help = "Seconds to wait for the ending, at most 600; 0 returns at once")]
    #[schemars(description = "Seconds to wait for the ending, at most 600; 0 returns at once.")]
    pub(crate) timeout: u64,
}

pub(crate) const fn default_limit() -> usize {
    50
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct InboxArgs {
    #[arg(long)]
    #[serde(default)]
    #[arg(help = "Show read messages too")]
    #[schemars(description = "Show read messages too.")]
    pub(crate) all: bool,
    #[arg(long, default_value_t = default_limit())]
    #[serde(default = "default_limit")]
    #[arg(help = "At most this many messages, up to 500")]
    #[schemars(description = "At most this many messages, up to 500.")]
    pub(crate) limit: usize,
}

#[derive(Debug, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ThreadArgs {
    #[arg(help = "An agent for your direct conversation, a room, or a conversation ID")]
    #[schemars(
        description = "An agent for your direct conversation, a room, or a conversation ID."
    )]
    pub(crate) target: String,
    #[arg(long, default_value_t = default_limit())]
    #[serde(default = "default_limit")]
    #[arg(help = "At most this many events, up to 500")]
    #[schemars(description = "At most this many events, up to 500.")]
    pub(crate) limit: usize,
    #[arg(long)]
    #[serde(default)]
    #[arg(help = "Only events before this message ID")]
    #[schemars(description = "Only events before this message ID.")]
    pub(crate) before: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct EmptyArgs {}
