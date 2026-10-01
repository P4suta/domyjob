# Change-scoped validation

`cargo xtask ci plan` owns the scope policy used by CI and the pre-push hook.
Documentation changes run spelling checks.
Workflow changes also run actionlint, repository-task checks and tests, Actions CodeQL, dependency review, and Scorecard.
Rust changes confined to `xtask` run its formatting, Clippy, architecture gates, workflow validation, package tests, and Rust CodeQL.
Changes to platform-sensitive task entry points also test that package on all three native operating systems.
Changes to the Windows compiler-contract task or its CLI entry points also run its actual SDK acceptance and rejection checks on Windows.
Product code, dependencies, toolchains, build configuration, and unrecognized paths retain full validation and native fleet checks.

The planner compares exact Git objects with a NUL-delimited diff and disabled rename detection, so deleted paths and both sides of a rename participate in the decision.
The pre-push hook unions every ref range received on stdin and includes local staged, unstaged, and untracked changes.
A new branch uses the exact merge base with an existing local `main`, or a validated remote's tracking `main` when explicitly supplied to `cargo xtask ci hook REMOTE`.
Missing history, an initial repository without a trusted base, merges, fork pull requests, and malformed inputs select full validation.
Scheduled and manual security scans also select full validation.

Workflows always produce their final required checks.
`cargo xtask ci gate` requires the changes job to succeed and accepts skipped jobs only when its complete, validated plan did not select them.
CI jobs share cached Rust dependencies, separated by operating system, CPU architecture, toolchain, and dependency configuration.
Only `main` saves these caches; pull requests restore them without saving, and release jobs use their independent builds.
Release rehearsals and tagged releases remain independent of this policy and build, sign, and verify all five distribution targets.
