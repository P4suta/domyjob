# Persistent jobs

Configure an OpenSSH host entry such as `linux` for the destination.
The remote account needs OpenSSH, mise, Rust, and Cargo.
On macOS, install the [login service](installing.md#login-service) from the desktop session before submitting jobs.

Run the current directory as a source snapshot:

```sh
domyjob run linux --wait -- cargo test
```

`run` includes tracked working-tree changes and visible untracked files.
`on` executes in the remote account's existing working environment.
Both commands print a job reference and keep the job running after the client disconnects.
Use `--submission ID` with the same command and snapshot to retry an uncertain submission.
The node refuses a conflicting request with the same submission identity.

`status MACHINE:JOB_ID` prints the recorded state.
`wait MACHINE:JOB_ID` waits for completion and prints the outcome and logs.
`logs MACHINE:JOB_ID` prints the available log.
`kill MACHINE:JOB_ID` requests termination of the owned process tree.
`ls MACHINE` lists retained jobs.

Reading a terminal result consumes its stored result and logs after the requested output succeeds and the client acknowledges delivery.
Add `--keep` to a read to preserve the result and logs.
The flag also works with `run --wait` and `on --wait`.
An interrupted connection or failed output leaves the result available for another retrieval.
Completion receipts preserve idempotent submission and repeated status or wait queries after consumption.
The runner refuses retrieval of consumed log bytes.

`clean MACHINE --job JOB_ID` explicitly removes a finished job.
`clean MACHINE` removes finished jobs that no longer have active users.
The worker releases its disposable workspace only after the process tree is quiescent.
Source jobs share a Cargo cache by project and build inputs.
Active cache users hold leases.
The cache manager retires obsolete idle entries after their users finish.
