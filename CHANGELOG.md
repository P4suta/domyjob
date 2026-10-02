# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.0](https://github.com/P4suta/domyjob/releases/tag/v0.0.0) - 2026-10-02

### Added

- *(release)* distribute signed and stapled macOS installers ([#31](https://github.com/P4suta/domyjob/pull/31))
- add an application icon and refresh release tooling
- [**breaking**] rewrite domyjob on a pure core with a chat for AI agents ([#25](https://github.com/P4suta/domyjob/pull/25))
- finish reliability hardening and make agent integration portable ([#24](https://github.com/P4suta/domyjob/pull/24))
- build and install a machine's domyjob from the source here ([#20](https://github.com/P4suta/domyjob/pull/20))
- wire fingerprint, live overview, and job history ([#17](https://github.com/P4suta/domyjob/pull/17))
- run `on` at once and say when a job waits in the queue ([#11](https://github.com/P4suta/domyjob/pull/11))
- send work to any machine and walk away

### Fixed

- machine-owned job limits, one JSON shape, and kill that says when it stopped nothing ([#23](https://github.com/P4suta/domyjob/pull/23))
- say plainly how each job ended, and list machines as they answer ([#22](https://github.com/P4suta/domyjob/pull/22))
- keep job logs, names, workspaces, and results each to their own job ([#21](https://github.com/P4suta/domyjob/pull/21))
- make bug classes unwritable, and give every system one path ([#18](https://github.com/P4suta/domyjob/pull/18))
- give back space before a disk is nearly full
- name the missing node binary and keep ssh noise out of the terminal

### Other

- *(ci)* centralize release tasks and select checks by change scope
- fold duplicated concepts into one implementation each ([#19](https://github.com/P4suta/domyjob/pull/19))
- note that tools which trust directories need the work area trusted ([#14](https://github.com/P4suta/domyjob/pull/14))
- record why the remaining node survivors cannot be caught ([#9](https://github.com/P4suta/domyjob/pull/9))
- teach the skill project jobs, shared workspaces, and setup
- add the README
