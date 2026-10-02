use reqwest::{blocking::Client, header};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write as _,
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
    FetchFailed,
    FetchRefused,
    InvalidResponse,
    RuntimeArguments,
    MissingToken,
    InvalidToken,
    InvalidEncoding,
    ClientInitialization,
    WriteFailed,
    WriteRefused,
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
            Self::FetchFailed => "Doppler fetch failed; response withheld",
            Self::FetchRefused => "Doppler fetch refused; response withheld",
            Self::InvalidResponse => "Doppler response invalid; response withheld",
            Self::RuntimeArguments => "Transfer accepts no runtime target arguments",
            Self::MissingToken => "Scoped migration token missing",
            Self::InvalidToken => "Config-scoped service token required; value withheld",
            Self::InvalidEncoding => "Source environment encoding invalid; values withheld",
            Self::ClientInitialization => "Secure client initialization failed",
            Self::WriteFailed => "Doppler transfer failed; response withheld",
            Self::WriteRefused => "Doppler transfer refused; response withheld",
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
struct Response {
    success: bool,
    secrets: BTreeMap<String, Secret>,
}

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

fn read(client: &Client, target: &Target) -> Result<BTreeMap<String, Secret>> {
    let response = client
        .get("https://api.doppler.com/v3/configs/config/secrets")
        .query(&[("project", &target.project), ("config", &target.config)])
        .send()
        .map_err(|_| TransferError::FetchFailed)?;
    if !response.status().is_success() {
        return Err(TransferError::FetchRefused);
    }
    let body: Response = response
        .json()
        .map_err(|_| TransferError::InvalidResponse)?;
    if !body.success {
        return Err(TransferError::FetchRefused);
    }
    Ok(body.secrets)
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
    let token = Zeroizing::new(
        std::env::var("DOPPLER_MIGRATION_TOKEN").map_err(|_| TransferError::MissingToken)?,
    );
    if !token.starts_with("dp.st.") {
        return Err(TransferError::InvalidToken);
    }
    let mut headers = header::HeaderMap::new();
    let mut authorization = header::HeaderValue::from_str(&format!("Bearer {}", token.as_str()))
        .map_err(|_| TransferError::InvalidToken)?;
    authorization.set_sensitive(true);
    headers.insert(header::AUTHORIZATION, authorization);
    let client = Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| TransferError::ClientInitialization)?;
    let current = read(&client, &target)?;
    let missing = vacant_values(&inputs, &current)?;
    if !missing.is_empty() {
        let response = client.post("https://api.doppler.com/v3/configs/config/secrets")
            .json(&serde_json::json!({"project": target.project, "config": target.config, "secrets": missing}))
            .send().map_err(|_| TransferError::WriteFailed)?;
        if !response.status().is_success() {
            return Err(TransferError::WriteRefused);
        }
    }
    verify(&inputs, &read(&client, &target)?)?;
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
}
