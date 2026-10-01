# Engineering rules

Every rule here exists because a class of bugs reached this code once.
Each rule is enforced by the compiler, a lint, the gate, a test, or a hook, so a change cannot quietly break it.
When a new class of bug appears, the fix adds a rule to this list, not only a patch.

| Rule | Bug class it removes | Enforced by |
| --- | --- | --- |
| Each effect has one owner, and only a private `raw` module inside an explicitly approved file calls the reserved API | Effects scattered where no policy applies; exceptions that hide unrelated violations | Clippy `disallowed-methods`, `disallowed-types`, `disallowed-macros`; the ownership and exception gates |
| No lint exception outside a `raw` module, except foreign-function modules and the end-to-end harness; malformed exception declarations are rejected | Convenience exceptions that accumulate and hide bugs; invalid declarations treated as permission | `cargo xtask gates` (`xtask/src/exceptions.rs`) |
| Standard output is reachable only through the `Output` that `main` creates | A stray line corrupting the MCP or node protocol stream; constructor aliases bypassing a spelling check | Resolved Clippy bans and the ownership gate, including aliases and macro tokens |
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
| A `StagedFile` synchronizes and closes its writer before replacing the destination with standard rename | A doorbell or state write refused while a watcher reads it; inconsistent replacement sequences between state, configuration, and executable installation | Private owned staging type; Clippy and non-suppressible ownership checks; live-reader and failure-cleanup contracts |
| Every file lock is acquired and released through an owning `OsLock` guard | A lock remaining held through a duplicated descriptor after the original file closes | Private guard without cloning or raw access; consuming release and explicit unlock in Drop; resolved File lock bans and ownership checks; duplicate-descriptor contracts |
| Windows workflows use kind, access, and ownership capabilities; job configuration, assignment, watch setup, and resumption follow typed transitions | A valid handle used with the wrong rights; borrowed security data outliving its allocation; skipped SDK setup | Checked private kernel and descriptor leaves; shared native contracts; seven required compiler rejection categories with a passing control |
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
The gate refuses an exception anywhere else, refuses a new unregistered `raw` owner, and refuses a `raw` module that grows logic.
Replacement methods that are forbidden even to an owner are checked independently of Clippy expectations.
Closing a staged writer and disabling its cleanup belong only to the replacement leaf.
File locking belongs only to the lock adapter.
The syntax checks protect reserved names and exception scopes, while Clippy resolves the actual types and methods.
Neither check claims to perform a complete Rust type analysis on its own.
Capability owners spell types and traits directly, without type aliases, renamed imports, or globs; extension-trait imports may use `as _`.
Capability declarations cannot conditionally change their attributes, and their only explicit trait implementation is Drop.
The gate rejects default construction, raw resource returns, and cloning of owned resources, permits only the named value and borrowed-view derives, and detects lint exceptions through nested conditional attributes.

| Effect | Owner |
| --- | --- |
| Decoding JSON | `domyjob-core::ingress` |
| Private state files | `state_io` |
| Synchronized file replacement | `platform::replacement::StagedFile` |
| File lock ownership | `lock::OsLock` |
| Installed builds | `builds` |
| Job workspaces | `workspace` |
| User configuration files, permissions, raw file options | `platform` |
| Time | `platform::clock` |
| Child processes, readiness signals | `process` |
| Windows kernel capabilities and SDK results | `process::windows::kernel` |
| Windows security allocations and borrowed ACL/SID data | `platform::windows_acl::descriptor` |
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

- `mise run lint` checks formatting, Clippy for the host, Linux, and Windows, the gate, Windows compiler contracts, spelling, workflows, and duplication.
- `mise run windows-contracts` rejects seven invalid Windows capability categories against the actual pinned SDK metadata and both leaf sources, then confirms a passing control.
  It checks each expected diagnostic code and primary source range, including separate ACL and SID lifetime failures, without executing SDK calls.
  The inventory must contain each required category exactly once and refer to the actual two SDK leaves.
  A diagnostic contributes at most one primary location, and the two lifetime sites must fail independently.
  Captured Cargo JSON, compiler diagnostics, and source digests are written under the active Cargo target directory's `windows-contracts` directory.
- `mise run test` runs unit tests, the format specimens, the exhaustive chat state search, and every end-to-end scenario on one host, each test in its own process.
- `mise run fuzz` explores ingress, job state, and chat ledger histories beyond the exhaustive scenario.
- `mise run mutants` measures changes to the core and xtask, including the repository's policy gates.
  `.rust-mutants.toml` records equivalent mutations with their reasons and checks that those expectations still name the current code.
  The 500 million step budget allows complete core test bundles to finish; exhaustion alone is not counted as detection.
- `mise run mutants:source` rebuilds isolated copies of the binary crate with one source mutation at a time and checks a restored control.
  It requires a source-capable rust-mutants build and runs the ordinary tests, including children and grandchildren that clear their environment.
  `.rust-mutants-source.toml` records individually reviewed native leads with their reasons, separately from the core and xtask expectations.
  These reasons do not turn native observations into sealed verdicts or make an unproven run pass.
  Native observations remain unproven leads and do not count toward the sealed mutation score.
  A native lead gives exit status 2; preserve that status when collecting logs or running multiple groups, and inspect every report rather than treating a completed job as a passing mutation run.
  A mutation rejected by a required compiler or Clippy gate is a static rejection, not a native test detection.
  Native failures, timeouts, failed restored controls, and missing evidence remain inconclusive or unaudited.
  [Effect contracts](verification/effect-contracts.md) define the shared verification categories and the finite completion boundary.
- `mise run check:fleet` runs the checks on Linux and Windows through domyjob.

Either mutation task accepts a dedicated engine through `DOMYJOB_MUTATION_TOOL`, so the default installation can stay unchanged.
Create a separate temporary directory for that engine before running the task to keep incompatible build caches apart:

```sh
mkdir -p /path/to/mutation-tmp
DOMYJOB_MUTATION_TOOL=/path/to/dedicated/rust-mutants TMPDIR=/path/to/mutation-tmp mise run mutants:source
```
