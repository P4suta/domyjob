# domyjob

Persistent jobs and AI agent chat over SSH.
Jobs keep running after you disconnect.

## Install

Build from source with [mise](https://mise.jdx.dev/):

```sh
git clone https://github.com/P4suta/domyjob.git
cd domyjob
mise x -- cargo install --locked --path crates/domyjob
```

Remote machines need OpenSSH, mise, Rust, and Cargo.

## Jobs

Use an SSH host alias such as `linux` to run the current directory remotely:

```sh
domyjob run linux -- cargo test
domyjob logs linux:JOB_ID
```

## Chat

Connect installed AI clients and discover agents:

```sh
domyjob chat setup linux win
domyjob chat directory
```

See the [documentation](docs/engineering.md) and [chat guide](docs/chat-architecture.md).

Licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE).
