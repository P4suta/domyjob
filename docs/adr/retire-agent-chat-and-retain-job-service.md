---
status: accepted
date: 2026-11-08
---

# Retire agent chat and keep the job service

## Context

Agent chat is outside the owner's intended scope for `domyjob`.
macOS jobs still need the existing login-session handoff service.

## Decision

Remove chat, MCP tools, managed agents, synchronization, and the chat dashboard.
Expose the login-session runtime through `domyjob service`.
Keep the existing service identifier as a compatibility identity, while changing its command to `service serve`.
Retire only recognized `domyjob` client registrations and hooks.
Preserve chat history, unrelated configuration, persistent jobs, and results.
Keep Linux and Windows job launch independent of a background chat service.
Use Apple's [per-user launchd model](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html).

## Alternatives

Keeping chat duplicates client coordination and retains its maintenance cost.
Removing its service without replacement breaks macOS job launch.
Renaming the native service identifier adds an unnecessary migration boundary.

## Consequences

Coordination becomes each client's responsibility.
Chat history remains until explicit removal.
Service migration must preserve worker lifetimes and configuration ownership.

## Confirmation

CLI rejection, owned-configuration migration, login handoff, lifecycle regressions, and the fast gate pass.
Reconsider if a supported client lacks necessary coordination or a native service contract fails.
