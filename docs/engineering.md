# Engineering rules

Every rule here exists because a class of bugs reached this code once.
Each rule is enforced by the compiler, a lint, the gate, a test, or a hook, so a change cannot quietly break it.
When a new class of bug appears, the fix adds a rule to this list, not only a patch.

| Rule | Bug class it removes | Enforced by |
| --- | --- | --- |
| Each effect has one owner, and only a private `raw` module inside it calls the reserved API | Effects scattered where no policy applies; exceptions that hide unrelated violations | Clippy `disallowed-methods`, `disallowed-types`, `disallowed-macros`; `cargo xtask gates` |
| No lint exception outside a `raw` module, except foreign-function modules and the end-to-end harness | Convenience exceptions that accumulate and hide bugs | `cargo xtask gates` (`xtask/src/exceptions.rs`) |
| Standard output is reachable only through the `Output` that `main` creates | A stray line corrupting the MCP or node protocol stream | `disallowed-macros`, `disallowed-methods`, gate on `Output::of_process` |
| Every child process starts from `process::command`, with standard input and output closed | A child writing into its parent's protocol stream | `disallowed-methods` on `Command::new` and `Command::output` |
| Every read and queue has a bound | Memory growth driven by a peer, a child, or a watcher | `disallowed-methods` on unbounded reads and `mpsc::channel`; `bounded` |
| Errors are matched, never turned into absence | A failure mistaken for "nothing there" | `disallowed-methods` on `Result::ok`, `unwrap_or`, `Path::exists`, and similar |
| Every match is exhaustive | A new variant silently taking a default path | `clippy::wildcard_enum_match_arm` |
| What differs between systems is a `System` trait that every system implements; everything else uses `cfg!` | A capability or fix present on one system and missing on another | The traits in `platform` and `process`; `mise run clippy:targets` lints Linux and Windows from any host |
| Every path under the state root comes from `layout` | Two modules spelling one file differently | `layout.rs`; review |
| A stored format is named by the digest of its specimen, never by a hand-written version | Forgotten version bumps and stores misread after a change | The specimen tests in `formats/`; the store records and checks the digest |
| Every wait drives this machine's progress at each wake-up | A wait that never ends because nothing reacts to a dead worker | `chat::pulse::Pulse` is the only way to watch the doorbell |
| Setup changes a user's file where it lives, keeping its link and permissions | A dotfiles link replaced by a copy, or a configuration's mode changed behind the user's back | `platform::user_files`, the only writer of user files, and its test |
| Removing what a job or another build left never stops new work: the tree leaves its place in one rename and is then removed as far as it can be | A file this user cannot delete, such as one a container wrote as root, stopping every later job | `state_io::set_aside` and `state_io::empty`, the only ways to remove a tree; the store's test |
| A file is replaced with the standard library's rename, which Windows performs even while another process holds the file | A doorbell or state write refused on Windows while a watcher reads the file it replaces | `disallowed-methods` on tempfile's `persist` and `keep`; the state file test |
| A stop or kill treats "already gone" as success | A finished process reported as a failure | Tests for each such case |
| The chat protocol is safe and converges in every interleaving of an ask | Arrival-order bugs between machines | The exhaustive state search in `domyjob-core` |
| No decision reads the wall clock: it comes out only as a `Stamp`, which can only be printed | Builds and stores removed after a period, and anything that behaves differently on a machine left idle or with a wrong clock | `platform::clock` hands out wall-clock time only as `Stamp`, which has no comparison, arithmetic, or number to read |
| What stays installed or stored follows use and reference, never age: idle builds and stores keep the most recently used few, and setup pins the build that services and AI clients run | A build that an AI client or service still needs removed because a week passed | `builds::prune`, `store::prune_other_formats`, and their tests |
| Each test runs in its own process, and a wait in a test ends when its condition holds, its bound only turning a hang into a failure | A lock that one test released still held by a child that another test forked, and checks that pass only when the machine is fast | `mise run test` runs `cargo nextest`, in CI too; the end-to-end harness asks the operating system directly rather than through WMI |
| Code and configuration carry no comments, except what a tool needs: a license header, a generated file's own, and a pinned action's version | Comments that drift from the code and mislead the next change | `cargo xtask gates` (`xtask/src/comments.rs`) |
| A fix lands with a test that fails without it | The same bug returning in a rewrite | The `commit-msg` hook (`cargo xtask commit-msg`) |

## Effects

An effect is anything the program does outside its own memory: files, processes, the clock, standard output, JSON from outside.
The lints forbid the standard library's entry points for each effect.
The one module that owns an effect wraps those entry points in a private module named `raw`,
whose functions forward in a single statement and whose first attribute expects the reserved lints.
The owner adds its policy around them, such as private permissions for state files or closed streams for children,
and everything outside `raw`, including the rest of the owner, stays fully linted.
The gate refuses an exception anywhere else, and refuses a `raw` module that grows logic.

| Effect | Owner |
| --- | --- |
| Decoding JSON | `domyjob-core::ingress` |
| Private state files | `state_io` |
| Installed builds | `builds` |
| Job workspaces | `workspace` |
| User configuration files, permissions, raw file options | `platform` |
| Time | `platform::clock` |
| Child processes, readiness signals | `process` |
| Standard output | `output` |
| Reading to the end of a stream | `bounded` |
| Test fixtures | `testing` |

## Operating systems

Only what an operating system's own interfaces provide goes behind a `System` trait, in `platform` and `process`.
Each system implements every method, so the compiler reports a capability that one system lacks.
All other code compiles on every system and chooses with `cfg!`,
so the Linux and Windows lints of `mise run lint` type-check every system's paths from any host.

## Formats

A stored format has no version number.
A test encodes one specimen of everything the store keeps and compares it with the checked-in file under `crates/domyjob/formats/`.
A change that would leave existing data unreadable changes the specimen, which fails the test until the file is regenerated with `DOMYJOB_UPDATE_FORMATS=1 cargo test`.
The digest of that file is the format: the chat store records it and refuses another,
and the job runner keeps each format's store in its own directory.
The wire protocol has no format of its own, because a client only ever talks to a node of its own build.

## Verification

- `mise run lint` checks formatting, Clippy for the host, Linux, and Windows, the gate, spelling, workflows, and duplication.
- `mise run test` runs unit tests, the format specimens, the exhaustive chat state search, and every end-to-end scenario on one host, each test in its own process.
- `mise run fuzz` explores ingress, job state, and chat ledger histories beyond the exhaustive scenario.
- `mise run mutants` changes the core's code one mutation at a time and requires the tests to notice each change;
  `.rust-mutants.toml` lists the few mutations that change nothing observable, each with its reason.
- `mise run check:fleet` runs the checks on Linux and Windows through domyjob.
