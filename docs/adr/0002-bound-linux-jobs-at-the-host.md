# Bound Linux jobs at the host

Status: Accepted.

## Context

An unbounded verification command exhausted a 16 GiB host's memory and swap.
earlyoom terminated NetworkManager, whose successful exit did not trigger `Restart=on-failure`.
Expired DHCP leases later removed connectivity, including SSH and Tailscale.
Client-only command wrappers would leave other clients and direct SSH commands unbounded.

## Decision

A Linux host may install a root-owned `/etc/domyjob/resource-policy.json` and `domyjob.slice`.
Every job on that host must acquire an owned admission permit and execute in a memory-limited transient scope under the aggregate slice.
There is no per-job opt-out.
Policy absence preserves existing behavior; malformed, untrusted, unsupported, or ineffective policy fails closed.
The host profile bounds job commands for one workspace UID together to 8 GiB soft, 10 GiB hard, and 2 GiB swap, with two active jobs across clients and builds and no CPU quota.
The workspace user's parent slice provides a separate 9 GiB soft, 11 GiB hard, and 2 GiB swap backstop for direct SSH work.
Management daemons remain outside that user slice and receive independent OOM protection and restart policy.

Admission uses shared OS locks under `/run/user/<uid>/domyjob/admission`, independently of home-directory, state-directory, and build-specific stores.
Workers remain alive while queued and honor cancellation before acquiring a permit and before spawning a command.
File-lock release has no portable notification, including a holder's abrupt death.
A narrow 250 ms retry rechecks the actual lock predicate; elapsed time never grants admission or means success.
Cancellation notifications remain event-driven.
FIFO order and a finite wait for an indefinitely running job are not promised.
The scope name is recorded under the permit before startup, so a new holder stops any previous scope before reusing its slot.
The permit also stops its recorded scope before releasing its lock, including failed startup paths.
[Kernel `cgroup.events`](https://docs.kernel.org/admin-guide/cgroup-v2.html#un-populated-notification) must report an empty group before cleanup succeeds; an inactive or failed systemd state alone never proves that descendants are gone.
The worker's reaper also stops the scope on supervisor loss, including descendants that leave the original process group.
Normal completion observes systemd's retained `oom-kill` result before cleanup and reports memory exhaustion separately from command exit.
Cancellation retains its own outcome.
The in-scope launcher records an actual spawn result and the original command's exit status using the existing validated job-state codec.
This preserves launch failures, nonzero exits, and signal termination even though the launcher itself exits successfully after recording them.

## Assurance boundary

[The resource proof gate](../../xtask/src/resource_proofs.rs) imports production `Budget::valid`, `JobState::advance`, and `JobState::command_completion` through the actual core crate.
It quantifies all budget integers, phase/event combinations, spawn PIDs, and exit codes, checks invalid-transition immutability, and requires reachable queue cancellation, OOM, and terminal rejection.
The state proof includes every terminal outcome and uses a one-byte valid launch-error payload; it does not claim exhaustive string-library verification.
Its unwind bound is 16 with unwinding assertions enabled.
Kani 0.68.0 is pinned, every expected harness and cover is required, and a separately compiled false budget assertion must produce its specific counterexample.
Every invocation uses a fresh generated-model directory.

[Kani does not model concurrency](https://model-checking.github.io/kani/rust-feature-support.html), systemd, kernel cgroups, user-manager failure, or Ansible deployment.
Those are external semantic boundaries, not additional pure proof claims.
Admission tests use real OS locks; cancellation and failure tests drive the production worker; native resource contracts exercise 64–96 MiB scopes, local OOM classification, and session-escaped descendant cleanup.
When a host policy is installed, the node-wrapper integration scenario also requires shared admission across separate client homes and stores, queued cancellation without execution, and child cleanup and slot reuse after forcibly terminating the actual job worker.
The OOM fixture raises its soft limit to the unchanged 96 MiB hard limit so reclaim throttling cannot delay that separate hard-limit contract.
Linux CI requires both the native contracts and the source-bound proof gate; Mac and Windows retain native tests and cross-target type checks without requiring a Linux user manager or a Windows Kani installation.
The Ansible slice asserts the reviewed host, quiescent domyjob workers, and working-memory headroom before changing limits, then checks actual reclaim before any new hard limit and verifies effective settings and connectivity.
These checks establish observed boundary behavior, not a theorem about the kernel or all deployment interleavings.
Another privileged workload, rootful containers, or a user deliberately launching outside the job scope is outside this admission guarantee.
Revisit these alternatives if a supported verifier can check the external boundary, if the systemd/kernel contract changes, or if a native contract exposes a mismatch.

## Consequences

Heavy jobs can fail locally or wait without taking down the host's management path.
A required but unavailable user manager blocks launching rather than running unrestricted.
All policy-aware builds share two admission slots, but pre-existing binaries that ignore the policy must be drained and updated.
The queued phase and memory-limit outcome change the runner specimen and therefore create a new isolated runner store.
Mac and Windows hosts do not install or load the Linux policy and keep their existing runtime behavior.
The budget prevents the observed failure mechanism; it is not a universal availability guarantee or a substitute for resource coordination.
