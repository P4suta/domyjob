# Temurin lookup through mise's catalog

The repository pins a Temurin HotSpot JDK through mise.
Renovate's built-in `java-version` datasource cannot configure a custom registry for this package, and its Adoptium API lookup currently fails during the TLS handshake.
The custom datasource reuses the first-party catalog that mise itself reads, while keeping the existing pin, vendor, exact normalized version aliases, and available Java majors.
The rule applies only to `java` in this repository's `mise.toml`; changing its Java vendor requires updating this provider rule too.

The transform copies each admitted catalog `version` unchanged and derives its repository and release-tag links from the official Temurin artifact URL.
The official artifact metadata maps `jdk-25.0.4.1+1` to `25.0.4+101.0.LTS`, so stripping the GitHub tag prefix alone would change the comparison value.
The catalog filter selects ordinary Temurin HotSpot JDK archives without optional feature variants; it does not restrict the Java major.

The rule compares major, minor, patch, build, and optional numeric revision explicitly.
The default coerced SemVer comparison drops `+7` and `+101`, although mise uses that build field to order these aliases.
The actual pinned runtime missed the eligible `25.0.4+7.0.LTS` to `25.0.4+101.0.LTS` update with the default comparator.
The explicit comparator reports that same real update as a patch, extracts the current `+101` exactly, and produces no update for the current pin within major 25.
These diagnostics constrain only the test snapshot; the production rule permits every Java major.
LTS describes support and does not become a compatibility restriction or a prerelease marker.

The transform refuses an empty ordinary GA selection or any selected version outside the comparator's admitted format before copying release metadata.
A false format predicate on the same real catalog produces the exact `TEMURIN_MISE_UNSUPPORTED_GA_ALIAS` assertion and a Java lookup warning with no current version.
Renovate exits successfully for this datasource refusal, so the report's warning and missing lookup evidence must refuse the repository gate.
The restored predicate resolves the exact current pin and the Java 27 upgrade without warnings.

The transform passes catalog `created_at` through unchanged as Renovate's `releaseTimestamp`.
The catalog producer stores this as its first observation timestamp, using an insertion default and preserving it on later updates.
It is not the official release publication time.
The shared preset's three-day minimum age remains enabled and starts from catalog observation.
Its conservative interpretation depends on the first-party catalog clock and Renovate's maintained UTC interpretation of the catalog's timezone-free ISO timestamp.
No timezone marker or date is invented in the transform.

Custom datasources are an experimental Renovate feature.
Acceptance requires the actual pinned Renovate 44.52.0 configuration and lookup run to retain nonempty package records, exact current version extraction, usable timestamps, and the normal recent-release age hold.
JSON parsing and a corresponding catalog extraction are preliminary checks and do not substitute for that runtime result.

The runtime control kept the same live catalog and increased only this Java rule's minimum age to thirty days.
Renovate held `27.0.0+35` in `pendingVersions` and selected the older eligible `26.0.2+101` release.
Restoring the inherited three-day minimum admits the same Java 27 release without an age hold.
The control does not invent a timestamp, modify the clock, or disable the shared age policy.

The existing catalog selection yielded 117 ordinary JDK entries, including the current exact pin, with a `created_at` string on every selected row.
The current artifact's catalog observation was `2026-08-21T22:08:02.270960`, while the official GitHub release publication was `2026-08-19T14:49:26Z`.
These are diagnostic observations rather than a claim that the public catalog is immutable or that every future entry will be well formed.

The pinned Renovate and JSONata interpreters, Cargo version extraction, first-party catalog, and their clocks remain external trust boundaries.
Native runtime controls exercise the actual configured provider; they do not prove those interpreters for every future input.
Reassess this provider when the version grammar, vendor, catalog format, timestamp semantics, or pinned interpreter changes.

Primary sources: [Renovate custom datasource](https://docs.renovatebot.com/modules/datasource/custom/), [Renovate datasource override](https://docs.renovatebot.com/configuration-options/#overridedatasource), [Renovate 44.52.0 Java extraction](https://github.com/renovatebot/renovate/blob/44.52.0/lib/modules/manager/mise/upgradeable-tooling.ts), [Renovate regex versioning](https://github.com/renovatebot/renovate/blob/44.52.0/lib/modules/versioning/regex/index.ts), [Renovate transform refusal](https://github.com/renovatebot/renovate/blob/44.52.0/lib/modules/datasource/custom/index.ts), [Renovate 44.52.0 timestamp parsing](https://github.com/renovatebot/renovate/blob/44.52.0/lib/util/timestamp.ts), [mise Java catalog consumer](https://github.com/jdx/mise/blob/main/src/plugins/core/java.rs), [catalog insertion and update](https://github.com/jdx/mise-java/blob/main/src/db/jvm_repository.rs), [catalog timestamp default](https://github.com/jdx/mise-java/blob/main/sql/schema.sql), and [official Temurin release](https://github.com/adoptium/temurin25-binaries/releases/tag/jdk-25.0.4.1%2B1).
