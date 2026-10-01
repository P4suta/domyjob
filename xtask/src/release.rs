use std::ffi::OsString;
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tempfile::{NamedTempFile, TempDir};

const SECURITY: &str = "/usr/bin/security";
const SECRET_ENVIRONMENT: [&str; 6] = [
    "APPLE_CERTIFICATE",
    "APPLE_CERTIFICATE_PASSWORD",
    "APPLE_SIGNING_IDENTITY",
    "APPLE_NOTARY_KEY",
    "APPLE_NOTARY_KEY_ID",
    "APPLE_NOTARY_ISSUER_ID",
];

struct Secret(Vec<u8>);

impl Secret {
    fn text(&self) -> Result<&str, Failure> {
        std::str::from_utf8(&self.0)
            .map_err(|_error| Failure::new("configuration", "a protected input is not UTF-8"))
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[derive(Debug)]
struct Failure {
    stage: &'static str,
    reason: String,
}

impl Failure {
    fn new(stage: &'static str, reason: impl Into<String>) -> Self {
        Self {
            stage,
            reason: reason.into(),
        }
    }
}

#[derive(Debug)]
pub struct SigningError {
    primary: Option<Failure>,
    cleanup: Vec<Failure>,
}

impl fmt::Display for SigningError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(primary) = &self.primary {
            write!(formatter, "{}: {}", primary.stage, primary.reason)?;
        }
        for (index, failure) in self.cleanup.iter().enumerate() {
            if self.primary.is_some() || index != 0 {
                formatter.write_str("; ")?;
            }
            write!(formatter, "cleanup {}: {}", failure.stage, failure.reason)?;
        }
        Ok(())
    }
}

impl std::error::Error for SigningError {}

impl From<Failure> for SigningError {
    fn from(primary: Failure) -> Self {
        Self {
            primary: Some(primary),
            cleanup: Vec::new(),
        }
    }
}

struct Configuration {
    target: String,
    temporary_root: PathBuf,
    certificate: Secret,
    certificate_password: Secret,
    identity: String,
    notary_key: Secret,
    notary_key_id: String,
    notary_issuer: String,
}

fn environment(name: &'static str) -> Result<String, Failure> {
    raw::environment(name)
        .map_err(|_error| Failure::new("configuration", format!("{name} is missing or not UTF-8")))
}

fn configured_identity(value: &str) -> Result<String, Failure> {
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Failure::new(
            "identity",
            "APPLE_SIGNING_IDENTITY must be an exact 40-digit SHA-1",
        ));
    }
    Ok(value.to_ascii_uppercase())
}

impl Configuration {
    fn read() -> Result<Self, Failure> {
        Self::read_with(environment)
    }

    fn read_with(read: impl Fn(&'static str) -> Result<String, Failure>) -> Result<Self, Failure> {
        let target = read("TARGET")?;
        if !matches!(
            target.as_str(),
            "aarch64-apple-darwin" | "x86_64-apple-darwin"
        ) {
            return Err(Failure::new(
                "configuration",
                "TARGET must name a macOS release target",
            ));
        }
        let encoded = Secret(read("APPLE_CERTIFICATE")?.into_bytes());
        let compact = Secret(
            encoded
                .0
                .iter()
                .copied()
                .filter(|byte| !byte.is_ascii_whitespace())
                .collect(),
        );
        let certificate = Secret(data_encoding::BASE64.decode(&compact.0).map_err(|_error| {
            Failure::new("configuration", "APPLE_CERTIFICATE is not valid base64")
        })?);
        let notary_key = Secret(read("APPLE_NOTARY_KEY")?.into_bytes());
        if certificate.0.is_empty() || notary_key.0.is_empty() {
            return Err(Failure::new(
                "configuration",
                "certificate and notarization key must not be empty",
            ));
        }
        let certificate_password = Secret(read("APPLE_CERTIFICATE_PASSWORD")?.into_bytes());
        validate_token(certificate_password.text()?)?;
        let notary_key_id = read("APPLE_NOTARY_KEY_ID")?;
        let notary_issuer = read("APPLE_NOTARY_ISSUER_ID")?;
        if notary_key_id.is_empty() || notary_issuer.is_empty() {
            return Err(Failure::new(
                "configuration",
                "notarization identifiers must not be empty",
            ));
        }
        Ok(Self {
            target,
            temporary_root: PathBuf::from(read("RUNNER_TEMP")?),
            certificate,
            certificate_password,
            identity: configured_identity(&read("APPLE_SIGNING_IDENTITY")?)?,
            notary_key,
            notary_key_id,
            notary_issuer,
        })
    }
}

struct Invocation<'a> {
    stage: &'static str,
    program: &'a str,
    args: &'a [OsString],
    input: Option<&'a Secret>,
}

trait Driver {
    fn invoke(&self, invocation: Invocation<'_>) -> Result<Vec<u8>, Failure>;

    fn run(
        &self,
        stage: &'static str,
        program: &str,
        args: &[OsString],
    ) -> Result<Vec<u8>, Failure> {
        self.invoke(Invocation {
            stage,
            program,
            args,
            input: None,
        })
    }

    fn protected(&self, stage: &'static str, input: &Secret) -> Result<Vec<u8>, Failure> {
        self.invoke(Invocation {
            stage,
            program: SECURITY,
            args: &arguments(&["-i"]),
            input: Some(input),
        })
    }
}

fn arguments(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

fn validate_token(value: &str) -> Result<(), Failure> {
    if value.bytes().any(|byte| matches!(byte, 0 | b'\n' | b'\r')) {
        return Err(Failure::new(
            "protected security input",
            "security command inputs cannot contain NUL, CR, or LF",
        ));
    }
    Ok(())
}

fn security_input(args: &[&str]) -> Result<Secret, Failure> {
    if args.len() > 31 {
        return Err(Failure::new(
            "protected security input",
            "security accepts at most 31 command arguments",
        ));
    }
    let mut line = Secret(Vec::new());
    for (index, value) in args.iter().enumerate() {
        validate_token(value)?;
        if index != 0 {
            line.0.push(b' ');
        }
        line.0.push(b'"');
        for byte in value.bytes() {
            if matches!(byte, b'\\' | b'"') {
                line.0.push(b'\\');
            }
            line.0.push(byte);
        }
        line.0.push(b'"');
    }
    if line.0.len() > 4094 {
        return Err(Failure::new(
            "protected security input",
            "security command exceeds its protected line limit",
        ));
    }
    line.0.push(b'\n');
    Ok(line)
}

fn path_text(path: &Path) -> Result<&str, Failure> {
    path.to_str()
        .ok_or_else(|| Failure::new("configuration", "release paths must be UTF-8"))
}

struct PrivateFiles {
    directory: TempDir,
    certificate: Option<NamedTempFile>,
    notary_key: Option<NamedTempFile>,
    archive: Option<NamedTempFile>,
}

impl PrivateFiles {
    fn create(configuration: &Configuration) -> Result<Self, SigningError> {
        let directory =
            raw::temporary_directory(&configuration.temporary_root).map_err(|_error| {
                Failure::new(
                    "private directory",
                    "could not create the owned temporary directory",
                )
            })?;
        let mut files = Self {
            directory,
            certificate: None,
            notary_key: None,
            archive: None,
        };
        match files.prepare(configuration) {
            Ok(()) => Ok(files),
            Err(primary) => Err(SigningError {
                primary: Some(primary),
                cleanup: files.finish(),
            }),
        }
    }

    fn prepare(&mut self, configuration: &Configuration) -> Result<(), Failure> {
        private_write(
            &mut self.certificate,
            self.directory.path(),
            ".p12",
            &configuration.certificate,
        )?;
        private_write(
            &mut self.notary_key,
            self.directory.path(),
            ".p8",
            &configuration.notary_key,
        )?;
        self.archive = Some(raw::private_file(self.directory.path(), ".zip").map_err(
            |_error| {
                Failure::new(
                    "notarization archive",
                    "could not create the owned notarization archive",
                )
            },
        )?);
        Ok(())
    }

    fn certificate_path(&self) -> Result<&Path, Failure> {
        private_path(self.certificate.as_ref())
    }

    fn notary_key_path(&self) -> Result<&Path, Failure> {
        private_path(self.notary_key.as_ref())
    }

    fn archive_path(&self) -> Result<&Path, Failure> {
        private_path(self.archive.as_ref())
    }

    fn finish(self) -> Vec<Failure> {
        let mut failures = Vec::new();
        for (stage, file) in [
            ("private certificate", self.certificate),
            ("private notarization key", self.notary_key),
            ("notarization archive", self.archive),
        ] {
            if let Some(file) = file
                && file.close().is_err()
            {
                failures.push(Failure::new(
                    stage,
                    "could not remove the owned temporary resource",
                ));
            }
        }
        if self.directory.close().is_err() {
            failures.push(Failure::new(
                "private directory",
                "could not remove the owned temporary directory",
            ));
        }
        failures
    }
}

fn private_write(
    slot: &mut Option<NamedTempFile>,
    root: &Path,
    suffix: &str,
    secret: &Secret,
) -> Result<(), Failure> {
    *slot = Some(raw::private_file(root, suffix).map_err(|_error| {
        Failure::new(
            "private credentials",
            "could not create an owned private file",
        )
    })?);
    let file = slot.as_mut().ok_or_else(|| {
        Failure::new(
            "private credentials",
            "the owned private file was not created",
        )
    })?;
    file.write_all(&secret.0)
        .and_then(|()| file.flush())
        .map_err(|_error| {
            Failure::new(
                "private credentials",
                "could not write an owned private file",
            )
        })
}

fn private_path(slot: Option<&NamedTempFile>) -> Result<&Path, Failure> {
    slot.map(NamedTempFile::path).ok_or_else(|| {
        Failure::new(
            "private credentials",
            "the owned private file is not prepared",
        )
    })
}

fn original_keychains(output: &[u8]) -> Result<Vec<OsString>, Failure> {
    let text = std::str::from_utf8(output).map_err(|_error| {
        Failure::new("keychain search list", "security returned non-UTF-8 paths")
    })?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            line.trim()
                .strip_prefix('"')
                .and_then(|path| path.strip_suffix('"'))
                .map(OsString::from)
                .ok_or_else(|| {
                    Failure::new(
                        "keychain search list",
                        "security returned an unrecognized path record",
                    )
                })
        })
        .collect()
}

struct SigningKeychain<'a, D: Driver> {
    driver: &'a D,
    path: PathBuf,
    original: Vec<OsString>,
    attempted: bool,
    finished: bool,
}

impl<'a, D: Driver> SigningKeychain<'a, D> {
    fn capture(driver: &'a D, path: PathBuf) -> Result<Self, Failure> {
        let output = driver.run(
            "capture keychain search list",
            SECURITY,
            &arguments(&["list-keychains", "-d", "user"]),
        )?;
        Ok(Self {
            driver,
            path,
            original: original_keychains(&output)?,
            attempted: false,
            finished: false,
        })
    }

    fn prepare(
        &mut self,
        configuration: &Configuration,
        files: &PrivateFiles,
    ) -> Result<(), Failure> {
        let password = keychain_password()?;
        let path = path_text(&self.path)?;
        let create = security_input(&["create-keychain", "-p", password.text()?, path])?;
        let unlock = security_input(&["unlock-keychain", "-p", password.text()?, path])?;
        let import = security_input(&[
            "import",
            path_text(files.certificate_path()?)?,
            "-k",
            path,
            "-P",
            configuration.certificate_password.text()?,
            "-T",
            "/usr/bin/codesign",
        ])?;
        let partition = security_input(&[
            "set-key-partition-list",
            "-S",
            "apple-tool:,apple:",
            "-s",
            "-k",
            password.text()?,
            path,
        ])?;
        self.attempted = true;
        self.protected("create keychain", &create)?;
        self.driver.run(
            "configure keychain",
            SECURITY,
            &arguments(&["set-keychain-settings", "-lut", "21600", path]),
        )?;
        self.protected("unlock keychain", &unlock)?;
        let mut search = arguments(&["list-keychains", "-d", "user", "-s"]);
        search.push(self.path.as_os_str().to_owned());
        search.extend(self.original.iter().cloned());
        self.driver
            .run("configure keychain search list", SECURITY, &search)?;
        self.protected("import certificate", &import)?;
        self.protected("configure private key access", &partition)?;
        let identities = self.driver.run(
            "validate signing identity",
            SECURITY,
            &arguments(&["find-identity", "-v", "-p", "codesigning", path]),
        )?;
        require_identity(&identities, &configuration.identity)
    }

    fn protected(&self, stage: &'static str, input: &Secret) -> Result<(), Failure> {
        self.driver.protected(stage, input)?;
        Ok(())
    }

    fn cleanup(&mut self) -> Vec<Failure> {
        let mut failures = Vec::new();
        if self.attempted {
            let mut restore = arguments(&["list-keychains", "-d", "user", "-s"]);
            restore.extend(self.original.iter().cloned());
            if let Err(error) = self
                .driver
                .run("restore keychain search list", SECURITY, &restore)
            {
                failures.push(error);
            }
            match self.path.try_exists() {
                Ok(false) => {}
                Ok(true) => {
                    let mut delete = arguments(&["delete-keychain"]);
                    delete.push(self.path.as_os_str().to_owned());
                    if let Err(error) =
                        self.driver
                            .run("delete signing keychain", SECURITY, &delete)
                    {
                        failures.push(error);
                    }
                }
                Err(_error) => failures.push(Failure::new(
                    "delete signing keychain",
                    "could not inspect the owned keychain",
                )),
            }
        }
        self.finished = true;
        failures
    }

    fn finish(mut self) -> Vec<Failure> {
        self.cleanup()
    }
}

impl<D: Driver> Drop for SigningKeychain<'_, D> {
    fn drop(&mut self) {
        if !self.finished {
            drop(self.cleanup());
        }
    }
}

fn require_identity(output: &[u8], identity: &str) -> Result<(), Failure> {
    let text = std::str::from_utf8(output).map_err(|_error| {
        Failure::new("identity", "security returned an invalid identity listing")
    })?;
    let found = text.lines().any(|line| {
        let mut words = line.split_whitespace();
        words.next().is_some_and(|ordinal| {
            ordinal.strip_suffix(')').is_some_and(|number| {
                !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
            })
        }) && words
            .next()
            .is_some_and(|value| value.len() == 40 && value.eq_ignore_ascii_case(identity))
    });
    if found {
        Ok(())
    } else {
        Err(Failure::new(
            "identity",
            "the configured SHA-1 is not a valid native code-signing identity",
        ))
    }
}

fn sign_and_notarize<D: Driver>(
    keychain: &SigningKeychain<'_, D>,
    root: &Path,
    configuration: &Configuration,
    files: &PrivateFiles,
) -> Result<(), Failure> {
    let driver = keychain.driver;
    let binary = root
        .join("target")
        .join(&configuration.target)
        .join("release/domyjob");
    let binary = path_text(&binary)?;
    driver.run(
        "sign binary",
        "/usr/bin/codesign",
        &arguments(&[
            "--force",
            "--keychain",
            path_text(&keychain.path)?,
            "--sign",
            &configuration.identity,
            "--options",
            "runtime",
            "--timestamp",
            binary,
        ]),
    )?;
    driver.run(
        "verify signed binary",
        "/usr/bin/codesign",
        &arguments(&["--verify", "--strict", "--verbose=2", binary]),
    )?;
    driver.run(
        "create notarization archive",
        "/usr/bin/ditto",
        &arguments(&[
            "-c",
            "-k",
            "--keepParent",
            binary,
            path_text(files.archive_path()?)?,
        ]),
    )?;
    let authentication = [
        "--key",
        path_text(files.notary_key_path()?)?,
        "--key-id",
        &configuration.notary_key_id,
        "--issuer",
        &configuration.notary_issuer,
    ];
    let mut submit = arguments(&["notarytool", "submit", path_text(files.archive_path()?)?]);
    submit.extend(arguments(&authentication));
    submit.extend(arguments(&["--wait", "--output-format", "json"]));
    let output = driver.run("submit notarization", "/usr/bin/xcrun", &submit)?;
    let response = raw::json(&output)
        .map_err(|_error| Failure::new("notarization", "notarytool did not return valid JSON"))?;
    if response.get("status").and_then(serde_json::Value::as_str) == Some("Accepted") {
        return Ok(());
    }
    if let Some(id) = response.get("id").and_then(serde_json::Value::as_str) {
        let mut log = arguments(&["notarytool", "log", id]);
        log.extend(arguments(&authentication));
        let rejected = driver.run("retrieve rejected notarization log", "/usr/bin/xcrun", &log);
        if rejected.is_err() {
            return Err(Failure::new(
                "notarization",
                "Apple did not accept the signed binary; its rejection log could not be retrieved",
            ));
        }
    }
    Err(Failure::new(
        "notarization",
        "Apple did not accept the signed binary",
    ))
}

fn run_with<D: Driver>(
    root: &Path,
    configuration: &Configuration,
    driver: &D,
) -> Result<(), SigningError> {
    let files = PrivateFiles::create(configuration)?;
    let captured =
        SigningKeychain::capture(driver, files.directory.path().join("signing.keychain-db"));
    let (primary, mut cleanup) = match captured {
        Ok(mut keychain) => {
            let primary = keychain
                .prepare(configuration, &files)
                .and_then(|()| sign_and_notarize(&keychain, root, configuration, &files))
                .err();
            (primary, keychain.finish())
        }
        Err(primary) => (Some(primary), Vec::new()),
    };
    cleanup.extend(files.finish());
    if primary.is_none() && cleanup.is_empty() {
        Ok(())
    } else {
        Err(SigningError { primary, cleanup })
    }
}

pub fn macos(root: &Path) -> Result<(), SigningError> {
    let configuration = Configuration::read()?;
    run_with(root, &configuration, &Native)
}

fn keychain_password() -> Result<Secret, Failure> {
    let mut entropy = Secret(vec![0; 32]);
    raw::random(&mut entropy.0).map_err(|_error| {
        Failure::new(
            "keychain password",
            "could not obtain operating-system randomness",
        )
    })?;
    Ok(Secret(
        data_encoding::HEXLOWER.encode(&entropy.0).into_bytes(),
    ))
}

fn bounded(mut stream: impl Read) -> Result<Vec<u8>, ()> {
    const LIMIT: u64 = 1_048_576;
    let mut bytes = Vec::new();
    raw::read_to_end(
        &mut stream.by_ref().take(LIMIT.saturating_add(1)),
        &mut bytes,
    )
    .map_err(|_error| ())?;
    std::io::copy(&mut stream, &mut std::io::sink()).map_err(|_error| ())?;
    if u64::try_from(bytes.len()).map_err(|_error| ())? > LIMIT {
        Err(())
    } else {
        Ok(bytes)
    }
}

struct Native;

impl Driver for Native {
    fn invoke(&self, invocation: Invocation<'_>) -> Result<Vec<u8>, Failure> {
        let Invocation {
            stage,
            program,
            args,
            input,
        } = invocation;
        let mut command = crate::raw::command(program);
        command
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in SECRET_ENVIRONMENT {
            command.env_remove(name);
        }
        let mut child = command
            .spawn()
            .map_err(|_error| Failure::new(stage, "could not start the native command"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Failure::new(stage, "native command has no output pipe"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Failure::new(stage, "native command has no diagnostic pipe"))?;
        std::thread::scope(|scope| {
            let out = scope.spawn(|| bounded(stdout));
            let err = scope.spawn(|| bounded(stderr));
            let written = match input {
                Some(input) => child
                    .stdin
                    .take()
                    .ok_or_else(|| {
                        Failure::new(stage, "native command has no protected input pipe")
                    })?
                    .write_all(&input.0)
                    .map_err(|_error| Failure::new(stage, "could not deliver protected input")),
                None => Ok(()),
            };
            let status = child
                .wait()
                .map_err(|_error| Failure::new(stage, "could not wait for the native command"))?;
            let output = out
                .join()
                .map_err(|_panic| Failure::new(stage, "native output reader stopped"))?
                .map_err(|()| {
                    Failure::new(
                        stage,
                        "native output could not be captured within its bound",
                    )
                })?;
            err.join()
                .map_err(|_panic| Failure::new(stage, "native diagnostic reader stopped"))?
                .map_err(|()| {
                    Failure::new(
                        stage,
                        "native diagnostics could not be captured within their bound",
                    )
                })?;
            written?;
            if !status.success() {
                return Err(Failure::new(
                    stage,
                    format!("native command exited with {status}"),
                ));
            }
            Ok(output)
        })
    }
}

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "the release adapter runs native signing tools with protected input and owns private temporary resources"
    )]

    use std::io;
    use std::path::Path;
    pub(super) fn environment(name: &str) -> Result<String, std::env::VarError> {
        std::env::var(name)
    }

    pub(super) fn temporary_directory(root: &Path) -> io::Result<tempfile::TempDir> {
        tempfile::Builder::new()
            .prefix("domyjob-signing-")
            .tempdir_in(root)
    }

    pub(super) fn private_file(root: &Path, suffix: &str) -> io::Result<tempfile::NamedTempFile> {
        tempfile::Builder::new()
            .prefix("credential-")
            .suffix(suffix)
            .tempfile_in(root)
    }

    pub(super) fn random(bytes: &mut [u8]) -> Result<(), getrandom::Error> {
        getrandom::fill(bytes)
    }

    pub(super) fn read_to_end(
        reader: &mut impl io::Read,
        bytes: &mut Vec<u8>,
    ) -> io::Result<usize> {
        reader.read_to_end(bytes)
    }

    pub(super) fn json(bytes: &[u8]) -> serde_json::Result<serde_json::Value> {
        serde_json::from_slice(bytes)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::io::Read as _;
    use std::path::{Path, PathBuf};

    use super::{
        Configuration, Driver, Failure, Invocation, Native, PrivateFiles, SECURITY, Secret,
        SigningKeychain, arguments, bounded, configured_identity, original_keychains,
        require_identity, run_with, security_input,
    };

    const IDENTITY: &str = "0123456789ABCDEF0123456789ABCDEF01234567";
    const PASSWORD: &str = "synthetic-private-password";
    const CERTIFICATE: &[u8] = b"synthetic-private-certificate";
    const NOTARY_KEY: &[u8] = b"synthetic-private-notary-key";
    const SEARCH_LIST: &str = "    \"/Users/test/quoted\" and \\ path.keychain-db\"\n    \"/Library/Keychains/System.keychain\"\n";

    #[derive(Debug)]
    struct Call {
        stage: &'static str,
        program: String,
        args: Vec<OsString>,
        protected: bool,
    }

    struct Fake {
        calls: RefCell<Vec<Call>>,
        failures: Vec<&'static str>,
        response: Vec<u8>,
        identity: String,
        owned_keychain: RefCell<Option<PathBuf>>,
    }

    impl Fake {
        fn new(failures: Vec<&'static str>) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                failures,
                response: br#"{"status":"Accepted","id":"public-notary-id"}"#.to_vec(),
                identity: IDENTITY.to_owned(),
                owned_keychain: RefCell::new(None),
            }
        }

        fn stages(&self) -> Vec<&'static str> {
            self.calls.borrow().iter().map(|call| call.stage).collect()
        }
    }

    impl Fake {
        fn record(&self, invocation: &Invocation<'_>) {
            let Invocation {
                stage,
                program,
                args,
                input,
            } = *invocation;
            for argument in args {
                for value in [
                    PASSWORD,
                    std::str::from_utf8(CERTIFICATE).unwrap(),
                    std::str::from_utf8(NOTARY_KEY).unwrap(),
                ] {
                    assert!(!argument.to_string_lossy().contains(value));
                }
            }
            if let Some(input) = input {
                assert_eq!(program, SECURITY);
                assert_eq!(args, arguments(&["-i"]));
                if stage == "create keychain" {
                    let path = PathBuf::from(input.text().unwrap().rsplit('"').nth(1).unwrap());
                    crate::raw::write(&path, b"synthetic-owned-keychain").unwrap();
                    *self.owned_keychain.borrow_mut() = Some(path);
                }
                if stage == "import certificate" {
                    assert!(input.text().unwrap().contains(PASSWORD));
                }
            }
            self.calls.borrow_mut().push(Call {
                stage,
                program: program.to_owned(),
                args: args.to_vec(),
                protected: input.is_some(),
            });
        }
    }

    impl Driver for Fake {
        fn invoke(&self, invocation: Invocation<'_>) -> Result<Vec<u8>, Failure> {
            self.record(&invocation);
            let stage = invocation.stage;
            if self.failures.contains(&stage) {
                return Err(Failure::new(stage, "synthetic failure"));
            }
            match stage {
                "capture keychain search list" => Ok(SEARCH_LIST.as_bytes().to_vec()),
                "validate signing identity" => Ok(format!("  1) {} \"Developer ID Application: Synthetic Publisher\"\n     1 valid identities found\n", self.identity).into_bytes()),
                "submit notarization" => Ok(self.response.clone()),
                _ => Ok(Vec::new()),
            }
        }
    }

    fn configuration(root: &Path) -> Configuration {
        Configuration {
            target: "aarch64-apple-darwin".to_owned(),
            temporary_root: root.to_path_buf(),
            certificate: Secret(CERTIFICATE.to_vec()),
            certificate_password: Secret(PASSWORD.as_bytes().to_vec()),
            identity: IDENTITY.to_owned(),
            notary_key: Secret(NOTARY_KEY.to_vec()),
            notary_key_id: "public-key-id".to_owned(),
            notary_issuer: "public-issuer-id".to_owned(),
        }
    }

    fn assert_owned_files_removed(root: &Path, fake: &Fake) {
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
        if let Some(path) = fake.owned_keychain.borrow().as_ref() {
            assert!(!path.try_exists().unwrap());
        }
    }

    #[test]
    fn accepted_signing_restores_search_list_and_removes_private_files() {
        let root = tempfile::tempdir().unwrap();
        let fake = Fake::new(Vec::new());
        run_with(root.path(), &configuration(root.path()), &fake).unwrap();
        assert_eq!(
            fake.stages(),
            [
                "capture keychain search list",
                "create keychain",
                "configure keychain",
                "unlock keychain",
                "configure keychain search list",
                "import certificate",
                "configure private key access",
                "validate signing identity",
                "sign binary",
                "verify signed binary",
                "create notarization archive",
                "submit notarization",
                "restore keychain search list",
                "delete signing keychain",
            ]
        );
        let calls = fake.calls.borrow();
        let sign = calls
            .iter()
            .find(|call| call.stage == "sign binary")
            .unwrap();
        assert_eq!(sign.program, "/usr/bin/codesign");
        assert!(
            sign.args
                .windows(2)
                .any(|pair| pair == arguments(&["--options", "runtime"]))
        );
        assert!(sign.args.contains(&OsString::from("--timestamp")));
        let restore = calls
            .iter()
            .find(|call| call.stage == "restore keychain search list")
            .unwrap();
        let mut expected = arguments(&["list-keychains", "-d", "user", "-s"]);
        expected.extend(original_keychains(SEARCH_LIST.as_bytes()).unwrap());
        assert_eq!(restore.args, expected);
        assert_eq!(calls.iter().filter(|call| call.protected).count(), 4);
        assert_owned_files_removed(root.path(), &fake);
    }

    #[test]
    fn every_native_failure_stops_forward_work_and_still_cleans_up() {
        for stage in [
            "create keychain",
            "configure keychain",
            "unlock keychain",
            "configure keychain search list",
            "import certificate",
            "configure private key access",
            "validate signing identity",
            "sign binary",
            "verify signed binary",
            "create notarization archive",
            "submit notarization",
        ] {
            let root = tempfile::tempdir().unwrap();
            let fake = Fake::new(vec![stage]);
            let error = run_with(root.path(), &configuration(root.path()), &fake).unwrap_err();
            assert_eq!(error.primary.as_ref().unwrap().stage, stage);
            assert!(error.cleanup.is_empty());
            let stages = fake.stages();
            let failure_index = stages.iter().position(|value| *value == stage).unwrap();
            assert_eq!(
                stages
                    .iter()
                    .skip(failure_index.saturating_add(1))
                    .copied()
                    .collect::<Vec<_>>(),
                ["restore keychain search list", "delete signing keychain"]
            );
            assert_owned_files_removed(root.path(), &fake);
        }
    }

    #[test]
    fn capture_failure_never_changes_keychain_configuration() {
        let root = tempfile::tempdir().unwrap();
        let fake = Fake::new(vec!["capture keychain search list"]);
        assert!(run_with(root.path(), &configuration(root.path()), &fake).is_err());
        assert_eq!(fake.stages(), ["capture keychain search list"]);
        assert_owned_files_removed(root.path(), &fake);
    }

    #[test]
    fn cleanup_failure_rejects_success_and_preserves_an_original_failure() {
        for primary in [None, Some("sign binary")] {
            let root = tempfile::tempdir().unwrap();
            let mut failures = vec!["restore keychain search list", "delete signing keychain"];
            failures.extend(primary);
            let fake = Fake::new(failures);
            let error = run_with(root.path(), &configuration(root.path()), &fake).unwrap_err();
            assert_eq!(error.primary.as_ref().map(|failure| failure.stage), primary);
            assert_eq!(
                error
                    .cleanup
                    .iter()
                    .map(|failure| failure.stage)
                    .collect::<Vec<_>>(),
                ["restore keychain search list", "delete signing keychain"]
            );
            assert!(
                error
                    .to_string()
                    .contains("cleanup restore keychain search list")
            );
            if primary.is_some() {
                assert!(
                    error
                        .to_string()
                        .starts_with("sign binary: synthetic failure; cleanup")
                );
            }
            assert_owned_files_removed(root.path(), &fake);
        }
    }

    #[test]
    fn drop_restores_an_unfinished_keychain_owner() {
        let root = tempfile::tempdir().unwrap();
        let configuration = configuration(root.path());
        let files = PrivateFiles::create(&configuration).unwrap();
        let fake = Fake::new(Vec::new());
        {
            let mut keychain =
                SigningKeychain::capture(&fake, files.directory.path().join("signing.keychain-db"))
                    .unwrap();
            keychain.prepare(&configuration, &files).unwrap();
        }
        assert_eq!(
            fake.stages()
                .iter()
                .rev()
                .take(2)
                .copied()
                .collect::<Vec<_>>(),
            ["delete signing keychain", "restore keychain search list"]
        );
        assert!(files.finish().is_empty());
        assert_owned_files_removed(root.path(), &fake);
    }

    #[test]
    fn invalid_native_identity_cannot_reach_signing() {
        let root = tempfile::tempdir().unwrap();
        let mut fake = Fake::new(Vec::new());
        fake.identity = "F".repeat(40);
        let error = run_with(root.path(), &configuration(root.path()), &fake).unwrap_err();
        assert_eq!(error.primary.unwrap().stage, "identity");
        assert!(!fake.stages().contains(&"sign binary"));
        assert_owned_files_removed(root.path(), &fake);
        assert_eq!(
            configured_identity(&IDENTITY.to_ascii_lowercase()).unwrap(),
            IDENTITY
        );
        for invalid in [
            "Developer ID Application: Synthetic",
            "012345",
            &"G".repeat(40),
            &"A".repeat(41),
        ] {
            configured_identity(invalid).unwrap_err();
        }
        for listing in [
            IDENTITY.to_owned(),
            format!("1 valid identities {IDENTITY}"),
            format!("x) {IDENTITY} \"untrusted\""),
        ] {
            assert!(require_identity(listing.as_bytes(), IDENTITY).is_err());
        }
    }

    #[test]
    fn rejected_and_malformed_notarization_never_report_acceptance() {
        for response in [
            br#"{"status":"Invalid","id":"public-notary-id"}"#.as_slice(),
            br#"{"status":"In Progress"}"#,
            b"private-diagnostic-not-json",
        ] {
            let root = tempfile::tempdir().unwrap();
            let mut fake = Fake::new(Vec::new());
            fake.response = response.to_vec();
            let error = run_with(root.path(), &configuration(root.path()), &fake).unwrap_err();
            assert_eq!(error.primary.as_ref().unwrap().stage, "notarization");
            assert!(!error.to_string().contains("private-diagnostic"));
            assert_owned_files_removed(root.path(), &fake);
        }
        let root = tempfile::tempdir().unwrap();
        let mut fake = Fake::new(vec!["retrieve rejected notarization log"]);
        fake.response = br#"{"status":"Invalid","id":"public-notary-id"}"#.to_vec();
        let error = run_with(root.path(), &configuration(root.path()), &fake).unwrap_err();
        assert_eq!(error.primary.as_ref().unwrap().stage, "notarization");
        assert!(error.to_string().contains("Apple did not accept"));
        assert!(
            fake.stages()
                .contains(&"retrieve rejected notarization log")
        );
    }

    #[test]
    fn security_input_quotes_literal_arguments_and_rejects_injection_or_truncation() {
        let input = security_input(&["import", " a'\"\\ b ", ""]).unwrap();
        assert_eq!(
            input.text().unwrap(),
            "\"import\" \" a'\\\"\\\\ b \" \"\"\n"
        );
        for value in ["private\nvalue", "private\rvalue", "private\0value"] {
            let error = security_input(&["create-keychain", value]).err().unwrap();
            assert!(!format!("{error:?}").contains("private"));
        }
        security_input(&[&"a".repeat(4092)]).unwrap();
        assert!(security_input(&[&"a".repeat(4093)]).is_err());
        assert!(security_input(&[&"\\".repeat(2047)]).is_err());
        assert!(security_input(&["arg"; 32]).is_err());
    }

    #[test]
    fn keychain_paths_preserve_literal_quotes_and_backslashes() {
        assert_eq!(
            original_keychains(SEARCH_LIST.as_bytes()).unwrap(),
            arguments(&[
                "/Users/test/quoted\" and \\ path.keychain-db",
                "/Library/Keychains/System.keychain"
            ])
        );
        original_keychains(b"bare-path\n").unwrap_err();
        original_keychains(&[255]).unwrap_err();
        assert!(original_keychains(b"\n \t\n").unwrap().is_empty());
    }

    fn configured_environment() -> BTreeMap<&'static str, String> {
        BTreeMap::from([
            ("TARGET", "aarch64-apple-darwin".to_owned()),
            ("RUNNER_TEMP", "/private/tmp".to_owned()),
            (
                "APPLE_CERTIFICATE",
                data_encoding::BASE64.encode(CERTIFICATE),
            ),
            ("APPLE_CERTIFICATE_PASSWORD", PASSWORD.to_owned()),
            ("APPLE_SIGNING_IDENTITY", IDENTITY.to_owned()),
            (
                "APPLE_NOTARY_KEY",
                std::str::from_utf8(NOTARY_KEY).unwrap().to_owned(),
            ),
            ("APPLE_NOTARY_KEY_ID", "public-key-id".to_owned()),
            ("APPLE_NOTARY_ISSUER_ID", "public-issuer-id".to_owned()),
        ])
    }

    fn read_configuration(
        values: &BTreeMap<&'static str, String>,
    ) -> Result<Configuration, Failure> {
        Configuration::read_with(|name| {
            values
                .get(name)
                .cloned()
                .ok_or_else(|| Failure::new("configuration", "required input is missing"))
        })
    }

    #[test]
    fn configuration_preserves_p12_inputs_and_redacts_invalid_secrets() {
        let mut values = configured_environment();
        let encoded = values.get_mut("APPLE_CERTIFICATE").unwrap();
        encoded.insert_str(4, "\n \t");
        let configuration = read_configuration(&values).unwrap();
        assert_eq!(configuration.certificate.0, CERTIFICATE);
        assert_eq!(configuration.certificate_password.text().unwrap(), PASSWORD);
        for (name, value) in [
            ("APPLE_CERTIFICATE", "private-invalid-base64"),
            ("APPLE_CERTIFICATE_PASSWORD", "private\npassword"),
            ("APPLE_SIGNING_IDENTITY", "private-wrong-identity"),
            ("TARGET", "private-wrong-target"),
        ] {
            let mut invalid = configured_environment();
            invalid.insert(name, value.to_owned());
            let error = read_configuration(&invalid).err().unwrap();
            assert!(!format!("{error:?}").contains(value));
        }
        values.insert("APPLE_CERTIFICATE_PASSWORD", String::new());
        read_configuration(&values).unwrap();
        values.insert("APPLE_NOTARY_KEY", String::new());
        assert!(read_configuration(&values).is_err());
    }

    #[test]
    #[ignore = "runs only as the bounded native subprocess diagnostic fixture"]
    fn native_probe_process() {
        let input = bounded(std::io::stdin().lock()).unwrap();
        if !input.is_empty() {
            eprintln!(
                "synthetic private diagnostic: {}",
                String::from_utf8_lossy(&input)
            );
            panic!("synthetic native failure");
        }
    }

    #[test]
    fn native_command_failure_does_not_forward_private_diagnostics() {
        let executable = std::env::current_exe().unwrap();
        let input = Secret(b"synthetic-private-diagnostic-marker".to_vec());
        let failure = Native
            .invoke(Invocation {
                stage: "synthetic native probe",
                program: executable.to_str().unwrap(),
                args: &arguments(&[
                    "--ignored",
                    "--exact",
                    "release::tests::native_probe_process",
                    "--nocapture",
                ]),
                input: Some(&input),
            })
            .unwrap_err();
        let error = super::SigningError::from(failure);
        assert!(error.to_string().contains("native command exited"));
        assert!(!error.to_string().contains("private-diagnostic-marker"));
        assert!(!format!("{error:?}").contains("private-diagnostic-marker"));
    }

    #[test]
    fn bounded_capture_rejects_oversized_output() {
        assert_eq!(bounded(b"small".as_slice()).unwrap(), b"small");
        bounded(std::io::repeat(b'x').take(1_048_577)).unwrap_err();
        let root = tempfile::tempdir().unwrap();
        let configuration = configuration(&root.path().join("missing"));
        assert!(PrivateFiles::create(&configuration).is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}
