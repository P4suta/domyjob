# Isolate Git process context

Status: Accepted.

Git hooks export repository context that can redirect commands started in another directory.
Repository tasks obtain Git commands through one factory that removes repository-local environment variables and namespaces before selecting the working directory.

A subprocess regression verifies that inherited hook context cannot alter a foreign repository's configuration or index.
An independent check compares the factory's removals with `git rev-parse --local-env-vars`, so additional Git context variables require explicit coverage.
