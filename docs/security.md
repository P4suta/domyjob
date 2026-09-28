# Security model

OpenSSH authentication controls who may request work on a machine.
The remote node executes commands as the SSH account and has no separate network listener.
A user who can run `domyjob on` on a host can execute arbitrary commands as that account.

The node treats control frames, snapshots, stored records, and remote output as untrusted data.
Ingress validates bounded wire messages before dispatch.
A source snapshot is checked for size, digest, portable paths, case collisions, and regular file types before confined extraction.
State files are private and checked for owner and access permissions before use.
Log replies read at most the final 16 KiB of the output file and terminal text is neutralized.
Persisted output files can grow with the command's output.
Jobs receive an explicit environment allowlist, so SSH agent and connection variables are not passed to the command.

The client builds and installs remote code from its embedded build source over an authenticated SSH connection.
The same source fingerprint is checked after installation to prevent a stale node from handling a new wire request.
The fingerprint detects changes; it does not certify the source or dependencies.
The remote account and build environment are trusted to compile and execute that checkout.

The first release does not provide peer pairing, a network service, signed release installation, file pullback, or migration from the former state format.
