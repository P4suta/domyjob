use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write as _,
    process::{Child, Command, Stdio},
};
use zeroize::Zeroizing;

type Result<T> = std::result::Result<T, TransferError>;

#[derive(Debug, PartialEq, Eq)]
enum TransferError {
    InvalidTarget,
    WrongInvocation,
    MissingInput,
    UnresolvedInput,
    DestinationConflict,
    VerificationFailed,
    InvalidResponse,
    RuntimeArguments,
    MissingToken,
    InvalidToken,
    InvalidEncoding,
    WriteFailed,
    ProcessFailed,
    ResponseTooLarge,
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidTarget => "Invalid fixed transfer target",
            Self::WrongInvocation => "Transfer invocation outside the fixed GitHub scope",
            Self::MissingInput => "Required source field missing; values withheld",
            Self::UnresolvedInput => "Source field empty or unresolved; values withheld",
            Self::DestinationConflict => {
                "Destination field missing or different; existing values preserved"
            }
            Self::VerificationFailed => {
                "Private source/destination comparison failed; values withheld"
            }
            Self::InvalidResponse => "Doppler response invalid; response withheld",
            Self::RuntimeArguments => "Transfer accepts no runtime target arguments",
            Self::MissingToken => "Scoped migration token missing",
            Self::InvalidToken => "Config-scoped service token required; value withheld",
            Self::InvalidEncoding => "Source environment encoding invalid; values withheld",
            Self::WriteFailed => "Doppler transfer failed; response withheld",
            Self::ProcessFailed => "Pinned Doppler CLI failed; output withheld",
            Self::ResponseTooLarge => "Doppler response exceeds the private buffer bound",
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    repository: String,
    repository_id: String,
    #[serde(rename = "ref")]
    git_ref: String,
    project: String,
    config: String,
    keys: BTreeSet<String>,
}

struct Inputs(BTreeMap<String, Zeroizing<String>>);

#[derive(Deserialize)]
struct Secret {
    raw: String,
    computed: String,
}

impl Drop for Secret {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.raw.zeroize();
        self.computed.zeroize();
    }
}

fn valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte))
}

fn valid_key(value: &str) -> bool {
    value
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_uppercase())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        && !value.starts_with("DOPPLER_")
        && value != "GITHUB_TOKEN"
}

impl Target {
    #[expect(
        clippy::disallowed_methods,
        reason = "this fixed-target ingress alone decodes the embedded public transfer scope"
    )]
    fn embedded() -> Result<Self> {
        serde_json::from_str(include_str!("../target.json"))
            .map_err(|_| TransferError::InvalidTarget)
    }

    fn validate(&self) -> Result<()> {
        let Some((owner, repository)) = self.repository.split_once('/') else {
            return Err(TransferError::InvalidTarget);
        };
        if owner != "P4suta"
            || repository.is_empty()
            || !repository
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
            || !matches!(self.repository_id.parse::<u64>(), Ok(id) if id != 0)
            || !matches!(
                self.git_ref.as_str(),
                "refs/heads/main" | "refs/heads/doppler-secret-import"
            )
            || !valid_slug(&self.project)
            || !valid_slug(&self.config)
            || self.keys.is_empty()
            || !self.keys.iter().all(|key| valid_key(key))
        {
            return Err(TransferError::InvalidTarget);
        }
        Ok(())
    }

    fn authorize(&self, environment: impl Fn(&str) -> Result<Option<String>>) -> Result<Inputs> {
        self.validate()?;
        for (key, expected) in [
            ("GITHUB_REPOSITORY", self.repository.as_str()),
            ("GITHUB_REPOSITORY_ID", self.repository_id.as_str()),
            ("GITHUB_REF", self.git_ref.as_str()),
            ("GITHUB_EVENT_NAME", "workflow_dispatch"),
        ] {
            if environment(key)?.as_deref() != Some(expected) {
                return Err(TransferError::WrongInvocation);
            }
        }
        let mut values = BTreeMap::new();
        for key in &self.keys {
            let value = Zeroizing::new(environment(key)?.ok_or(TransferError::MissingInput)?);
            if value.trim().is_empty() || value.contains("${") {
                return Err(TransferError::UnresolvedInput);
            }
            values.insert(key.clone(), value);
        }
        Ok(Inputs(values))
    }
}

fn vacant_values<'a>(
    inputs: &'a Inputs,
    current: &BTreeMap<String, Secret>,
) -> Result<BTreeMap<&'a str, &'a str>> {
    let mut missing = BTreeMap::new();
    for (key, expected) in &inputs.0 {
        match current.get(key) {
            Some(secret) if secret.raw.is_empty() => {
                missing.insert(key.as_str(), expected.as_str());
            }
            Some(secret) if secret.raw == **expected && secret.computed == **expected => {}
            _ => {
                return Err(TransferError::DestinationConflict);
            }
        }
    }
    Ok(missing)
}

fn verify(inputs: &Inputs, current: &BTreeMap<String, Secret>) -> Result<()> {
    if inputs.0.iter().all(|(key, expected)| {
        current
            .get(key)
            .is_some_and(|secret| secret.raw == **expected && secret.computed == **expected)
    }) {
        Ok(())
    } else {
        Err(TransferError::VerificationFailed)
    }
}

const PRIVATE_BUFFER_LIMIT: u64 = 16 * 1024 * 1024;

fn bounded_bytes(reader: impl std::io::Read, limit: u64) -> Result<Zeroizing<Vec<u8>>> {
    let mut bytes = Zeroizing::new(Vec::new());
    let count = std::io::copy(&mut reader.take(limit + 1), &mut *bytes)
        .map_err(|_| TransferError::ProcessFailed)?;
    if count > limit {
        return Err(TransferError::ResponseTooLarge);
    }
    Ok(bytes)
}

#[expect(
    clippy::disallowed_methods,
    reason = "this bounded private CLI ingress alone decodes Doppler JSON without exposing errors"
)]
fn decode_cli(bytes: &[u8]) -> Result<BTreeMap<String, Secret>> {
    serde_json::from_slice(bytes).map_err(|_| TransferError::InvalidResponse)
}

struct Cli<'a> {
    target: &'a Target,
    token: Zeroizing<String>,
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl<'a> Cli<'a> {
    fn new(target: &'a Target, token: String) -> Result<Self> {
        let token = Zeroizing::new(token);
        if !token.starts_with("dp.st.") {
            return Err(TransferError::InvalidToken);
        }
        Ok(Self { target, token })
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "this private CLI adapter alone creates children with closed standard streams"
    )]
    fn command(&self) -> Command {
        let mut command = Command::new("doppler");
        command.env_clear();
        for name in [
            "PATH",
            "HOME",
            "SYSTEMROOT",
            "USERPROFILE",
            "TMPDIR",
            "TEMP",
            "TMP",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .env("DOPPLER_TOKEN", self.token.as_str())
            .args([
                "--no-check-version",
                "--silent",
                "--api-host",
                "https://api.doppler.com",
                "--timeout",
                "10s",
                "--attempts",
                "1",
                "secrets",
                "--project",
                &self.target.project,
                "--config",
                &self.target.config,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
    }

    fn capture(&self, command: &mut Command, input: Option<&[u8]>) -> Result<Zeroizing<Vec<u8>>> {
        let mut child = ChildGuard(command.spawn().map_err(|_| TransferError::ProcessFailed)?);
        if let Some(input) = input {
            let mut stdin = child.0.stdin.take().ok_or(TransferError::ProcessFailed)?;
            stdin
                .write_all(input)
                .map_err(|_| TransferError::WriteFailed)?;
        }
        let stdout = child.0.stdout.take().ok_or(TransferError::ProcessFailed)?;
        let bytes = bounded_bytes(stdout, PRIVATE_BUFFER_LIMIT)?;
        let status = child.0.wait().map_err(|_| TransferError::ProcessFailed)?;
        if !status.success() {
            return Err(TransferError::ProcessFailed);
        }
        Ok(bytes)
    }

    fn read(&self) -> Result<BTreeMap<String, Secret>> {
        let bytes = self.capture(self.command().args(["--json", "--raw"]), None)?;
        decode_cli(&bytes)
    }

    fn write(&self, missing: &BTreeMap<&str, &str>) -> Result<()> {
        let bytes =
            Zeroizing::new(serde_json::to_vec(missing).map_err(|_| TransferError::WriteFailed)?);
        if bytes.len() as u64 > PRIVATE_BUFFER_LIMIT {
            return Err(TransferError::ResponseTooLarge);
        }
        self.capture(
            self.command()
                .args(["upload", "/dev/stdin"])
                .stdin(Stdio::piped()),
            Some(&bytes),
        )?;
        Ok(())
    }
}

fn run() -> Result<usize> {
    if std::env::args_os().len() != 1 {
        return Err(TransferError::RuntimeArguments);
    }
    let target = Target::embedded()?;
    let inputs = target.authorize(|key| match std::env::var(key) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(TransferError::InvalidEncoding),
    })?;
    let client = Cli::new(
        &target,
        std::env::var("DOPPLER_MIGRATION_TOKEN").map_err(|_| TransferError::MissingToken)?,
    )?;
    let current = client.read()?;
    let missing = vacant_values(&inputs, &current)?;
    if !missing.is_empty() {
        client.write(&missing)?;
    }
    verify(&inputs, &client.read()?)?;
    Ok(inputs.0.len())
}

struct Output(std::io::Stdout);

impl Output {
    #[expect(
        clippy::disallowed_methods,
        reason = "this output capability alone obtains the transfer process standard output"
    )]
    fn of_process() -> Self {
        Self(std::io::stdout())
    }

    fn report(&self, result: &Result<usize>) -> std::io::Result<()> {
        match result {
            Ok(count) => writeln!(
                self.0.lock(),
                "Transferred and privately verified {count} selected fields; values withheld"
            ),
            Err(error) => writeln!(self.0.lock(), "{error}"),
        }
    }
}

fn main() -> std::process::ExitCode {
    let output = Output::of_process();
    let result = run();
    if output.report(&result).is_ok() && result.is_ok() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target {
            repository: "P4suta/test".into(),
            repository_id: "42".into(),
            git_ref: "refs/heads/main".into(),
            project: "test".into(),
            config: "release".into(),
            keys: BTreeSet::from(["CARGO_TOKEN".into()]),
        }
    }

    fn environment(key: &str) -> Option<String> {
        match key {
            "GITHUB_REPOSITORY" => Some("P4suta/test".into()),
            "GITHUB_REPOSITORY_ID" => Some("42".into()),
            "GITHUB_REF" => Some("refs/heads/main".into()),
            "GITHUB_EVENT_NAME" => Some("workflow_dispatch".into()),
            "CARGO_TOKEN" => Some("test-value".into()),
            _ => None,
        }
    }

    #[test]
    fn only_the_fixed_repository_identity_ref_and_manual_event_can_transfer() {
        assert!(target().authorize(|key| Ok(environment(key))).is_ok());
        for key in [
            "GITHUB_REPOSITORY",
            "GITHUB_REPOSITORY_ID",
            "GITHUB_REF",
            "GITHUB_EVENT_NAME",
        ] {
            assert!(
                target()
                    .authorize(|name| Ok(if name == key {
                        Some("different".into())
                    } else {
                        environment(name)
                    }))
                    .is_err()
            );
        }
    }

    #[test]
    fn missing_empty_and_unresolved_values_cannot_be_written() {
        for value in [None, Some(""), Some("  "), Some("${shared.missing.VALUE}")] {
            assert!(
                target()
                    .authorize(|key| Ok(if key == "CARGO_TOKEN" {
                        value.map(str::to_owned)
                    } else {
                        environment(key)
                    }))
                    .is_err()
            );
        }
    }

    #[test]
    fn existing_owner_values_are_preserved_and_conflicts_stop_the_transfer() {
        let inputs = target()
            .authorize(|key| Ok(environment(key)))
            .expect("authorized fixture");
        let current = BTreeMap::from([(
            "CARGO_TOKEN".into(),
            Secret {
                raw: "owner-value".into(),
                computed: "owner-value".into(),
            },
        )]);
        assert!(vacant_values(&inputs, &current).is_err());
        assert!(vacant_values(&inputs, &BTreeMap::new()).is_err());
    }

    #[test]
    fn placeholders_can_be_filled_and_matching_values_are_idempotent() {
        let inputs = target()
            .authorize(|key| Ok(environment(key)))
            .expect("authorized fixture");
        let empty = BTreeMap::from([(
            "CARGO_TOKEN".into(),
            Secret {
                raw: String::new(),
                computed: String::new(),
            },
        )]);
        assert_eq!(
            vacant_values(&inputs, &empty).expect("placeholder").len(),
            1
        );
        let matching = BTreeMap::from([(
            "CARGO_TOKEN".into(),
            Secret {
                raw: "test-value".into(),
                computed: "test-value".into(),
            },
        )]);
        assert!(
            vacant_values(&inputs, &matching)
                .expect("matching")
                .is_empty()
        );
        assert!(verify(&inputs, &matching).is_ok());
        assert!(verify(&inputs, &empty).is_err());
    }

    #[test]
    fn runtime_auth_tokens_and_ephemeral_github_tokens_cannot_be_selected() {
        for key in [
            "DOPPLER_MIGRATION_TOKEN",
            "GITHUB_TOKEN",
            "../TOKEN",
            "token",
        ] {
            let mut target = target();
            target.keys = BTreeSet::from([key.into()]);
            assert!(target.validate().is_err());
        }
    }

    #[test]
    fn the_compiled_transfer_target_is_well_formed() {
        let target = Target::embedded().expect("embedded target");
        assert!(target.validate().is_ok());
    }

    #[test]
    fn invalid_environment_encoding_remains_an_error() {
        assert!(matches!(
            target().authorize(|_| Err(TransferError::InvalidEncoding)),
            Err(TransferError::InvalidEncoding)
        ));
    }

    #[test]
    fn private_response_reads_are_bounded_and_overflow_is_rejected() {
        let bytes = bounded_bytes(&b"abcd"[..], 4).expect("exact bound");
        assert_eq!(&*bytes, b"abcd");
        assert!(matches!(
            bounded_bytes(&b"abcde"[..], 4),
            Err(TransferError::ResponseTooLarge)
        ));
    }

    #[test]
    fn cli_credentials_are_never_arguments_or_unselected_environment_inputs() {
        let target = target();
        let cli = Cli::new(&target, "dp.st.fixture".into()).expect("scoped fixture");
        let command = cli.command();
        assert!(command.get_args().all(|arg| arg != "dp.st.fixture"));
        let names: BTreeSet<_> = command.get_envs().map(|(name, _)| name).collect();
        assert!(!names.contains(std::ffi::OsStr::new("DOPPLER_MIGRATION_TOKEN")));
        assert!(!names.contains(std::ffi::OsStr::new("APPLE_CERTIFICATE")));
        assert!(Cli::new(&target, "dp.ct.invalid".into()).is_err());
    }

    #[test]
    fn json_transport_preserves_certificate_line_endings_and_trailing_newlines() {
        let certificate = "line one\r\nline two\n";
        let encoded = serde_json::to_vec(&BTreeMap::from([("CERTIFICATE", certificate)]))
            .expect("certificate fixture");
        assert_eq!(encoded, br#"{"CERTIFICATE":"line one\r\nline two\n"}"#);
        let decoded = decode_cli(br#"{"CERTIFICATE":{"raw":"line one\r\nline two\n","computed":"line one\r\nline two\n","note":""}}"#)
            .expect("official CLI response fixture");
        assert_eq!(decoded["CERTIFICATE"].raw, certificate);
        assert_eq!(decoded["CERTIFICATE"].computed, certificate);
        assert!(decode_cli(br#"{"CERTIFICATE":{"raw":null,"computed":null}}"#).is_err());
    }
}
