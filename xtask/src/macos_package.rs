use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::release_queue::PendingNotarization;

pub(crate) const IDENTIFIER: &str = "io.github.p4suta.domyjob";
const PATH_LINE: &[u8] = b"/Library/domyjob/bin\n";
const LIMIT: u64 = 64 * 1024 * 1024;
const OUTPUT_LIMIT: u64 = 1024 * 1024;
const FILES: [&str; 5] = [
    "Library/domyjob/bin/domyjob",
    "Library/domyjob/share/README.md",
    "Library/domyjob/share/LICENSE-MIT",
    "Library/domyjob/share/LICENSE-APACHE",
    "etc/paths.d/domyjob",
];
const DIRECTORIES: [&str; 6] = [
    "Library",
    "Library/domyjob",
    "Library/domyjob/bin",
    "Library/domyjob/share",
    "etc",
    "etc/paths.d",
];

#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    #[error("invalid macOS package: {0}")]
    Invalid(&'static str),
    #[error("native macOS package step failed: {0}")]
    Native(&'static str),
    #[error("macOS packaging I/O: {0}")]
    Io(#[from] std::io::Error),
}

const fn require(condition: bool, reason: &'static str) -> Result<(), PackageError> {
    if condition {
        Ok(())
    } else {
        Err(PackageError::Invalid(reason))
    }
}

pub(crate) fn architecture(target: &str) -> Result<&'static str, PackageError> {
    match target {
        "aarch64-apple-darwin" => Ok("arm64"),
        "x86_64-apple-darwin" => Ok("x86_64"),
        _ => Err(PackageError::Invalid("unsupported package target")),
    }
}

pub(crate) fn package_name(version: &str, target: &str) -> Result<String, PackageError> {
    architecture(target)?;
    require(
        version.len() <= 128
            && semver::Version::parse(version).is_ok_and(|parsed| parsed.to_string() == version),
        "package version must be canonical SemVer",
    )?;
    Ok(format!("domyjob-{version}-{target}.pkg"))
}

fn text(path: &Path) -> Result<&str, PackageError> {
    path.to_str()
        .ok_or(PackageError::Invalid("package path is not UTF-8"))
}

fn arguments(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

fn regular(path: &Path, maximum: u64) -> Result<(), PackageError> {
    let metadata = raw::metadata(path)?;
    require(
        metadata.file_type().is_file() && (1..=maximum).contains(&metadata.len()),
        "package input must be a bounded regular file",
    )
}

fn read(path: &Path, maximum: u64) -> Result<Vec<u8>, PackageError> {
    regular(path, maximum)?;
    let mut bytes = Vec::new();
    raw::read(
        &mut raw::open(path)?.take(maximum.saturating_add(1)),
        &mut bytes,
    )?;
    require(
        u64::try_from(bytes.len()).is_ok_and(|length| length <= maximum),
        "package input changed size",
    )?;
    Ok(bytes)
}

fn hash(path: &Path) -> Result<String, PackageError> {
    crate::release_queue::sha256_file_bounded(path)
        .map_err(|_error| PackageError::Invalid("could not hash the package input"))
}

fn directory(path: &Path) -> Result<(), PackageError> {
    match raw::metadata(path) {
        Ok(metadata) => require(
            metadata.file_type().is_dir(),
            "package directory must not be a link",
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                directory(parent)?;
            }
            raw::create_directory(path)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn absent(path: &Path) -> Result<(), PackageError> {
    match raw::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err(PackageError::Invalid("package destination already exists")),
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PackageSigning<'a> {
    pub(crate) keychain: &'a Path,
    pub(crate) identity: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PackageBuild<'a> {
    pub(crate) root: &'a Path,
    pub(crate) target: &'a str,
    pub(crate) version: &'a str,
    pub(crate) binary: &'a Path,
    pub(crate) signing: Option<PackageSigning<'a>>,
}

fn stage_payload(root: &Path, binary: &Path, payload: &Path) -> Result<(), PackageError> {
    for name in DIRECTORIES {
        let destination = payload.join(name);
        directory(&destination)?;
        #[cfg(unix)]
        raw::permissions(&destination, true)?;
    }
    for name in FILES {
        let file = payload.join(name);
        directory(
            file.parent()
                .ok_or(PackageError::Invalid("payload has no parent"))?,
        )?;
        if name == "etc/paths.d/domyjob" {
            raw::write(&file, PATH_LINE)?;
        } else {
            let source = if name == FILES[0] {
                binary.to_path_buf()
            } else {
                root.join(
                    Path::new(name)
                        .file_name()
                        .ok_or(PackageError::Invalid("invalid payload filename"))?,
                )
            };
            regular(&source, LIMIT)?;
            let before = hash(&source)?;
            raw::copy(&source, &file)?;
            require(
                hash(&source)? == before && hash(&file)? == before,
                "payload changed while copying",
            )?;
        }
        #[cfg(unix)]
        raw::permissions(&file, name == FILES[0])?;
    }
    Ok(())
}

pub(crate) fn build_with(
    build: PackageBuild<'_>,
    execute: &mut impl FnMut(&'static str, &str, &[OsString]) -> Result<Vec<u8>, PackageError>,
) -> Result<PathBuf, PackageError> {
    let PackageBuild {
        root,
        target,
        version,
        binary,
        signing,
    } = build;
    let name = package_name(version, target)?;
    let output = if signing.is_some() {
        root.join("dist")
    } else {
        root.join("target/package-preview")
    };
    directory(&output)?;
    let destination = output.join(&name);
    absent(&destination)?;
    let staging = raw::temporary_directory(&output)?;
    let payload = staging.path().join("root");
    stage_payload(root, binary, &payload)?;
    let component = staging.path().join("payload.pkg");
    execute(
        "build component package",
        "/usr/bin/pkgbuild",
        &arguments(&[
            "--root",
            text(&payload)?,
            "--identifier",
            IDENTIFIER,
            "--version",
            version,
            "--install-location",
            "/",
            "--ownership",
            "recommended",
            text(&component)?,
        ]),
    )?;
    let distribution = staging.path().join("distribution.xml");
    raw::write(&distribution, format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<installer-gui-script minSpecVersion=\"2\"><title>domyjob</title><options customize=\"never\" require-scripts=\"false\" hostArchitectures=\"{}\" rootVolumeOnly=\"true\"/><domains enable_anywhere=\"false\" enable_currentUserHome=\"false\" enable_localSystem=\"true\"/><choices-outline><line choice=\"default\"/></choices-outline><choice id=\"default\" visible=\"false\"><pkg-ref id=\"{IDENTIFIER}\"/></choice><pkg-ref id=\"{IDENTIFIER}\" version=\"{version}\" onConclusion=\"none\">#payload.pkg</pkg-ref></installer-gui-script>\n",
        architecture(target)?,
    ).as_bytes())?;
    let completed = staging.path().join(&name);
    let mut product = arguments(&[
        "--distribution",
        text(&distribution)?,
        "--package-path",
        text(staging.path())?,
    ]);
    if let Some(signing) = signing {
        product.extend(arguments(&[
            "--sign",
            signing.identity,
            "--keychain",
            text(signing.keychain)?,
            "--timestamp",
        ]));
    }
    product.push(completed.as_os_str().to_owned());
    execute("build product package", "/usr/bin/productbuild", &product)?;
    inspect_payload(&completed, target, version, execute)?;
    regular(&completed, LIMIT)?;
    raw::hard_link(&completed, &destination)?;
    staging.close()?;
    Ok(destination)
}

pub fn preview(root: &Path, target: &str, version: &str) -> Result<PathBuf, PackageError> {
    let binary = root.join("target").join(target).join("release/domyjob");
    build_with(
        PackageBuild {
            root,
            target,
            version,
            binary: &binary,
            signing: None,
        },
        &mut native,
    )
}

fn xml_elements(
    xml: &[u8],
) -> Result<impl Iterator<Item = Result<(&str, &str), PackageError>>, PackageError> {
    let xml = std::str::from_utf8(xml)
        .map_err(|_error| PackageError::Invalid("package XML is not UTF-8"))?;
    require(
        !xml.contains("<!") && !xml.contains('&'),
        "package XML contains declarations or entities",
    )?;
    Ok(xml.split('<').skip(1).map(|suffix| {
        suffix
            .split_once('>')
            .ok_or(PackageError::Invalid("unterminated XML tag"))
    }))
}

fn attributes(xml: &[u8], element: &str) -> Result<Vec<BTreeMap<String, String>>, PackageError> {
    let mut result = Vec::new();
    for entry in xml_elements(xml)? {
        let (tag, _body) = entry?;
        if let Some(rest) = tag.strip_prefix(element)
            && (rest.is_empty()
                || rest.starts_with(|character: char| {
                    character.is_ascii_whitespace() || character == '/'
                }))
        {
            let mut remaining = rest.trim();
            let mut values = BTreeMap::new();
            while !remaining.is_empty() && remaining != "/" {
                let (name, next) = remaining
                    .split_once('=')
                    .ok_or(PackageError::Invalid("invalid XML attribute"))?;
                let name = name.trim();
                require(
                    !name.is_empty()
                        && name.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
                        }),
                    "invalid XML attribute name",
                )?;
                let next = next.trim_start();
                let quote = next
                    .chars()
                    .next()
                    .ok_or(PackageError::Invalid("missing XML quote"))?;
                require(matches!(quote, '\'' | '"'), "unquoted XML attribute")?;
                let (value, next) = next
                    .get(1..)
                    .and_then(|value| value.split_once(quote))
                    .ok_or(PackageError::Invalid("unterminated XML value"))?;
                require(
                    values.insert(name.to_owned(), value.to_owned()).is_none(),
                    "duplicate XML attribute",
                )?;
                remaining = next.trim_start();
            }
            result.push(values);
        }
    }
    Ok(result)
}

fn exact_attribute(
    values: &BTreeMap<String, String>,
    key: &str,
    value: &str,
) -> Result<(), PackageError> {
    require(
        values.get(key).is_some_and(|actual| actual == value),
        "package metadata differs from its contract",
    )
}

fn permitted_tags(xml: &[u8], allowed: &[&str]) -> Result<(), PackageError> {
    for entry in xml_elements(xml)? {
        let (tag, _body) = entry?;
        if tag.starts_with("?xml ") {
            continue;
        }
        let name = tag
            .trim_start_matches('/')
            .split(|character: char| character.is_ascii_whitespace() || character == '/')
            .next()
            .ok_or(PackageError::Invalid("empty XML tag"))?;
        require(
            allowed.contains(&name),
            "package XML contains executable or unexpected elements",
        )?;
    }
    Ok(())
}

fn walk(directory: &Path, prefix: &str, files: &mut BTreeSet<String>) -> Result<(), PackageError> {
    for item in raw::read_dir(directory)? {
        let item = item?;
        let name = item
            .file_name()
            .into_string()
            .map_err(|_name| PackageError::Invalid("payload name is not UTF-8"))?;
        require(
            !matches!(name.as_str(), "." | "..") && !name.contains(['/', '\\']),
            "unsafe payload name",
        )?;
        let relative = format!("{prefix}{name}");
        let kind = item.file_type()?;
        if kind.is_dir() {
            require(
                FILES
                    .iter()
                    .any(|file| file.starts_with(&format!("{relative}/"))),
                "unexpected payload directory",
            )?;
            #[cfg(unix)]
            require(
                raw::mode(&raw::metadata(&item.path())?) & 0o7777 == 0o755,
                "payload directory permissions differ",
            )?;
            walk(&item.path(), &format!("{relative}/"), files)?;
        } else {
            require(
                kind.is_file() && FILES.contains(&relative.as_str()),
                "unexpected payload file or link",
            )?;
            regular(&item.path(), LIMIT)?;
            #[cfg(unix)]
            require(
                raw::mode(&raw::metadata(&item.path())?) & 0o7777
                    == if relative == FILES[0] { 0o755 } else { 0o644 },
                "payload file permissions differ",
            )?;
            require(files.insert(relative), "duplicate payload file")?;
        }
    }
    Ok(())
}

fn inventory(path: &Path, expected: &[(&str, bool)]) -> Result<(), PackageError> {
    let mut observed = BTreeSet::new();
    for item in raw::read_dir(path)? {
        let item = item?;
        let name = item
            .file_name()
            .into_string()
            .map_err(|_name| PackageError::Invalid("package name is not UTF-8"))?;
        let kind = item.file_type()?;
        require(
            expected.iter().any(|(wanted, directory)| {
                name == *wanted
                    && if *directory {
                        kind.is_dir()
                    } else {
                        kind.is_file()
                    }
            }),
            "unexpected package member or link",
        )?;
        observed.insert(name);
    }
    require(
        observed
            == expected
                .iter()
                .map(|(name, _kind)| (*name).to_owned())
                .collect(),
        "package inventory is incomplete",
    )
}

#[derive(Debug)]
struct ExtractedPackage {
    directory: tempfile::TempDir,
    binary: PathBuf,
}

fn single(xml: &[u8], element: &str) -> Result<BTreeMap<String, String>, PackageError> {
    let mut values = attributes(xml, element)?;
    require(
        values.len() == 1,
        "package needs exactly one metadata element",
    )?;
    values
        .pop()
        .ok_or(PackageError::Invalid("package metadata is missing"))
}

fn verify_distribution(xml: &[u8], target: &str, version: &str) -> Result<(), PackageError> {
    permitted_tags(
        xml,
        &[
            "installer-gui-script",
            "title",
            "options",
            "domains",
            "choices-outline",
            "line",
            "choice",
            "pkg-ref",
            "bundle-version",
        ],
    )?;
    let options = single(xml, "options")?;
    for (key, value) in [
        ("hostArchitectures", architecture(target)?),
        ("require-scripts", "false"),
        ("rootVolumeOnly", "true"),
        ("customize", "never"),
    ] {
        exact_attribute(&options, key, value)?;
    }
    require(
        options.len() == 4,
        "package options contain unexpected attributes",
    )?;
    let domains = single(xml, "domains")?;
    for (key, value) in [
        ("enable_anywhere", "false"),
        ("enable_currentUserHome", "false"),
        ("enable_localSystem", "true"),
    ] {
        exact_attribute(&domains, key, value)?;
    }
    require(
        domains.len() == 3,
        "package domains contain unexpected attributes",
    )?;
    let root = single(xml, "installer-gui-script")?;
    exact_attribute(&root, "minSpecVersion", "2")?;
    require(root.len() == 1, "package root has unexpected attributes")?;
    for name in ["title", "choices-outline", "bundle-version"] {
        require(
            single(xml, name)?.is_empty(),
            "package contains unexpected presentation attributes",
        )?;
    }
    let line = single(xml, "line")?;
    exact_attribute(&line, "choice", "default")?;
    require(line.len() == 1, "package choice line differs")?;
    let choice = single(xml, "choice")?;
    exact_attribute(&choice, "id", "default")?;
    exact_attribute(&choice, "visible", "false")?;
    require(
        choice.len() == 2,
        "package choice contains executable attributes",
    )?;
    verify_references(xml, version)?;
    Ok(())
}

fn verify_references(xml: &[u8], version: &str) -> Result<(), PackageError> {
    let mut count = 0_u8;
    let mut versioned = false;
    for entry in xml_elements(xml)? {
        let (tag, body) = entry?;
        if !tag.starts_with("pkg-ref ") {
            continue;
        }
        count = count.saturating_add(1);
        let values = single(format!("<{tag}>").as_bytes(), "pkg-ref")?;
        exact_attribute(&values, "id", IDENTIFIER)?;
        if values.contains_key("version") {
            require(
                !versioned,
                "package contains duplicate versioned references",
            )?;
            versioned = true;
            exact_attribute(&values, "version", version)?;
            exact_attribute(&values, "onConclusion", "none")?;
            require(
                values.keys().all(|key| {
                    [
                        "id",
                        "version",
                        "onConclusion",
                        "installKBytes",
                        "updateKBytes",
                    ]
                    .contains(&key.as_str())
                }),
                "package reference contains executable attributes",
            )?;
            require(
                body.trim() == "#payload.pkg",
                "package refers to an external or unexpected payload",
            )?;
        } else {
            require(
                values.len() == 1 && body.trim().is_empty(),
                "unversioned package reference differs",
            )?;
        }
    }
    require(count == 3 && versioned, "package references are incomplete")
}

fn verify_component(xml: &[u8], version: &str) -> Result<(), PackageError> {
    permitted_tags(
        xml,
        &[
            "pkg-info",
            "payload",
            "bundle-version",
            "upgrade-bundle",
            "update-bundle",
            "atomic-update-bundle",
            "strict-identifier",
            "relocate",
        ],
    )?;
    let info = single(xml, "pkg-info")?;
    for (key, value) in [
        ("identifier", IDENTIFIER),
        ("version", version),
        ("install-location", "/"),
        ("auth", "root"),
        ("relocatable", "false"),
        ("postinstall-action", "none"),
    ] {
        exact_attribute(&info, key, value)?;
    }
    require(
        info.keys().all(|key| {
            [
                "overwrite-permissions",
                "relocatable",
                "identifier",
                "postinstall-action",
                "version",
                "format-version",
                "generator-version",
                "install-location",
                "auth",
            ]
            .contains(&key.as_str())
        }),
        "package component contains unexpected attributes",
    )?;
    Ok(())
}

fn inspect_payload(
    path: &Path,
    target: &str,
    version: &str,
    execute: &mut impl FnMut(&'static str, &str, &[OsString]) -> Result<Vec<u8>, PackageError>,
) -> Result<ExtractedPackage, PackageError> {
    regular(path, LIMIT)?;
    let before = hash(path)?;
    let directory = raw::temporary_directory(std::env::temp_dir().as_path())?;
    let expanded = directory.path().join("expanded");
    execute(
        "expand package",
        "/usr/sbin/pkgutil",
        &arguments(&["--expand-full", text(path)?, text(&expanded)?]),
    )?;
    inventory(&expanded, &[("Distribution", false), ("payload.pkg", true)])?;
    let component = expanded.join("payload.pkg");
    inventory(
        &component,
        &[("Bom", false), ("PackageInfo", false), ("Payload", true)],
    )?;
    verify_distribution(
        &read(&expanded.join("Distribution"), OUTPUT_LIMIT)?,
        target,
        version,
    )?;
    verify_component(
        &read(&component.join("PackageInfo"), OUTPUT_LIMIT)?,
        version,
    )?;
    let payload = component.join("Payload");
    let mut files = BTreeSet::new();
    walk(&payload, "", &mut files)?;
    require(
        files == FILES.into_iter().map(str::to_owned).collect(),
        "package payload is incomplete",
    )?;
    require(
        read(&payload.join("etc/paths.d/domyjob"), 128)? == PATH_LINE,
        "package PATH entry differs",
    )?;
    require(hash(path)? == before, "package changed during inspection")?;
    Ok(ExtractedPackage {
        binary: payload.join(FILES[0]),
        directory,
    })
}

fn installer_leaf(
    path: &Path,
    directory: &Path,
    execute: &mut impl FnMut(&'static str, &str, &[OsString]) -> Result<Vec<u8>, PackageError>,
) -> Result<PathBuf, PackageError> {
    let toc = directory.join("toc.xml");
    execute(
        "read installer certificate",
        "/usr/bin/xar",
        &arguments(&["-f", text(path)?, &format!("--dump-toc={}", text(&toc)?)]),
    )?;
    let xml = read(&toc, OUTPUT_LIMIT)?;
    let xml = std::str::from_utf8(&xml)
        .map_err(|_error| PackageError::Invalid("installer certificate XML is not UTF-8"))?;
    let encoded = xml
        .split_once("<X509Certificate>")
        .and_then(|(_prefix, value)| value.split_once("</X509Certificate>"))
        .map(|(value, _suffix)| value)
        .ok_or(PackageError::Invalid(
            "installer leaf certificate is missing",
        ))?;
    let compact: Vec<_> = encoded
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    let certificate = data_encoding::BASE64
        .decode(&compact)
        .map_err(|_error| PackageError::Invalid("installer certificate is not valid base64"))?;
    require(
        !certificate.is_empty() && certificate.len() <= 65_536,
        "installer certificate exceeds its bound",
    )?;
    let leaf = directory.join("leaf.cer");
    raw::write(&leaf, &certificate)?;
    Ok(leaf)
}

fn installer_status(output: &str) -> Result<String, PackageError> {
    require(
        output.lines().any(|line| {
            matches!(
                line.trim(),
                "Status: signed by a certificate trusted by macOS"
                    | "Status: signed by a certificate trusted by Mac OS X"
                    | "Status: signed by a developer certificate issued by Apple for distribution"
            )
        }),
        "installer signature is not trusted",
    )?;
    require(
        output.lines().any(|line| {
            line.trim()
                .strip_prefix("Signed with a trusted timestamp on:")
                .is_some_and(|value| {
                    !value.trim().is_empty() && !matches!(value.trim(), "none" | "not set")
                })
        }),
        "installer has no trusted timestamp",
    )?;
    let lines: Vec<_> = output.lines().map(str::trim).collect();
    require(
        lines
            .iter()
            .filter(|line| line.starts_with("1. Developer ID Installer:"))
            .count()
            == 1,
        "package needs one active Developer ID Installer leaf",
    )?;
    let start = lines
        .iter()
        .position(|line| line.starts_with("1. Developer ID Installer:"))
        .ok_or(PackageError::Invalid("installer certificate is missing"))?;
    let mut reading = false;
    let mut fingerprint = String::new();
    for line in lines.iter().skip(start.saturating_add(1)) {
        if line.eq_ignore_ascii_case("SHA256 Fingerprint:") {
            reading = true;
            continue;
        }
        if !reading {
            require(
                !line.starts_with("2."),
                "active installer SHA256 fingerprint is missing",
            )?;
            continue;
        }
        require(
            !line.is_empty()
                && line.split_whitespace().all(|word| {
                    word.len() == 2 && word.bytes().all(|byte| byte.is_ascii_hexdigit())
                }),
            "active installer SHA256 fingerprint is malformed",
        )?;
        for word in line.split_whitespace() {
            fingerprint.push_str(word);
        }
        require(
            fingerprint.len() <= 64,
            "active installer SHA256 fingerprint exceeds its bound",
        )?;
        if fingerprint.len() == 64 {
            return Ok(fingerprint.to_ascii_lowercase());
        }
    }
    Err(PackageError::Invalid(
        "active installer SHA256 fingerprint is incomplete",
    ))
}

pub(crate) fn verify_installer_with(
    path: &Path,
    identity: &str,
    execute: &mut impl FnMut(&'static str, &str, &[OsString]) -> Result<Vec<u8>, PackageError>,
) -> Result<(), PackageError> {
    let output = execute(
        "verify installer signature",
        "/usr/sbin/pkgutil",
        &arguments(&["--check-signature", text(path)?]),
    )?;
    let output = std::str::from_utf8(&output)
        .map_err(|_error| PackageError::Invalid("installer signature output is not UTF-8"))?;
    let signer_sha256 = installer_status(output)?;
    let directory = raw::temporary_directory(std::env::temp_dir().as_path())?;
    let leaf = installer_leaf(path, directory.path(), execute)?;
    require(
        hash(&leaf)? == signer_sha256,
        "installer certificate does not match the active package signer",
    )?;
    let fingerprint = execute(
        "check installer fingerprint",
        "/usr/bin/openssl",
        &arguments(&[
            "x509",
            "-inform",
            "DER",
            "-in",
            text(&leaf)?,
            "-noout",
            "-fingerprint",
            "-sha1",
        ]),
    )?;
    let fingerprint = std::str::from_utf8(&fingerprint)
        .map_err(|_error| PackageError::Invalid("installer fingerprint is not UTF-8"))?;
    let actual = fingerprint
        .trim()
        .split_once('=')
        .map(|(_label, value)| value.replace(':', ""))
        .ok_or(PackageError::Invalid("installer fingerprint is missing"))?;
    require(
        actual.eq_ignore_ascii_case(identity),
        "installer certificate differs from the receipt",
    )?;
    directory.close()?;
    Ok(())
}

fn verify_binary(
    binary: &Path,
    receipt: &PendingNotarization,
    execute: &mut impl FnMut(&'static str, &str, &[OsString]) -> Result<Vec<u8>, PackageError>,
) -> Result<(), PackageError> {
    require(
        hash(binary)? == receipt.binary_sha256(),
        "package binary differs from its receipt",
    )?;
    let expected_architecture = architecture(receipt.target())?;
    let output = execute(
        "inspect package binary architecture",
        "/usr/bin/lipo",
        &arguments(&["-archs", text(binary)?]),
    )?;
    require(
        std::str::from_utf8(&output).is_ok_and(|value| value.trim() == expected_architecture),
        "package binary architecture differs",
    )?;
    execute(
        "verify package binary",
        "/usr/bin/codesign",
        &arguments(&[
            "--verify",
            "--strict",
            "--verbose=2",
            "-R",
            &format!(
                "certificate leaf = H\"{}\"",
                receipt.signing_identity_sha1()
            ),
            text(binary)?,
        ]),
    )?;
    let metadata = execute(
        "inspect signed binary",
        "/usr/bin/codesign",
        &arguments(&["--display", "--verbose=4", text(binary)?]),
    )?;
    crate::release::verify_package_metadata(&metadata, receipt.cdhash())?;
    require(
        hash(binary)? == receipt.binary_sha256(),
        "package binary changed during verification",
    )
}

#[derive(Debug)]
pub(crate) struct VerifiedPackage {
    path: PathBuf,
    receipt: PendingNotarization,
}

pub(crate) fn verify_pending(
    path: &Path,
    receipt: &PendingNotarization,
) -> Result<VerifiedPackage, PackageError> {
    verify_pending_with(path, receipt, &mut native)
}

pub(crate) fn verify_pending_with(
    path: &Path,
    receipt: &PendingNotarization,
    execute: &mut impl FnMut(&'static str, &str, &[OsString]) -> Result<Vec<u8>, PackageError>,
) -> Result<VerifiedPackage, PackageError> {
    let package = receipt
        .package()
        .ok_or(PackageError::Invalid("receipt does not identify a package"))?;
    require(
        hash(path)? == package.sha256(),
        "pending package differs from its receipt",
    )?;
    verify_contents(path, receipt, execute)?;
    require(
        hash(path)? == package.sha256(),
        "pending package changed during verification",
    )?;
    Ok(VerifiedPackage {
        path: path.to_path_buf(),
        receipt: receipt.clone(),
    })
}

fn verify_contents(
    path: &Path,
    receipt: &PendingNotarization,
    execute: &mut impl FnMut(&'static str, &str, &[OsString]) -> Result<Vec<u8>, PackageError>,
) -> Result<(), PackageError> {
    let package = receipt
        .package()
        .ok_or(PackageError::Invalid("receipt does not identify a package"))?;
    require(
        package.identifier() == IDENTIFIER,
        "package receipt identifier differs",
    )?;
    let before = hash(path)?;
    verify_installer_with(path, package.installer_identity_sha1(), execute)?;
    let extracted = inspect_payload(path, receipt.target(), receipt.source().version(), execute)?;
    let result = verify_binary(&extracted.binary, receipt, execute);
    extracted.directory.close()?;
    result?;
    require(hash(path)? == before, "package changed during verification")
}

#[derive(Debug)]
pub(crate) struct StapledPackage {
    path: PathBuf,
    sha256: String,
}

impl StapledPackage {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
    pub(crate) fn sha256(&self) -> &str {
        &self.sha256
    }
}

pub(crate) fn staple(
    pending: VerifiedPackage,
    accepted: &crate::release::AcceptedToken,
    output: &Path,
) -> Result<StapledPackage, PackageError> {
    let VerifiedPackage {
        path: pending_path,
        receipt,
    } = pending;
    require(
        accepted.source() == receipt.source(),
        "Apple acceptance belongs to another source",
    )?;
    verify_pending(&pending_path, &receipt)?;
    directory(output)?;
    let destination = output.join(package_name(receipt.source().version(), receipt.target())?);
    absent(&destination)?;
    let staging = raw::temporary_directory(output)?;
    let path = staging.path().join("package.pkg");
    raw::copy(&pending_path, &path)?;
    require(
        hash(&path)?
            == receipt
                .package()
                .ok_or(PackageError::Invalid("missing package receipt"))?
                .sha256(),
        "package changed while staging the ticket",
    )?;
    native(
        "staple accepted package",
        "/usr/bin/xcrun",
        &arguments(&["stapler", "staple", text(&path)?]),
    )?;
    let verified = verify_stapled(&path, &receipt)?;
    verify_pending(&pending_path, &receipt)?;
    raw::hard_link(&path, &destination)?;
    staging.close()?;
    Ok(StapledPackage {
        path: destination,
        sha256: verified.sha256,
    })
}

pub(crate) fn verify_stapled(
    path: &Path,
    receipt: &PendingNotarization,
) -> Result<StapledPackage, PackageError> {
    let before = hash(path)?;
    verify_contents(path, receipt, &mut native)?;
    native(
        "validate stapled package",
        "/usr/bin/xcrun",
        &arguments(&["stapler", "validate", text(path)?]),
    )?;
    native(
        "assess stapled installer",
        "/usr/sbin/spctl",
        &arguments(&["--assess", "--type", "install", "--verbose=2", text(path)?]),
    )?;
    require(
        hash(path)? == before,
        "stapled package changed during verification",
    )?;
    Ok(StapledPackage {
        path: path.to_path_buf(),
        sha256: before,
    })
}

fn bounded(mut reader: impl std::io::Read) -> Result<Vec<u8>, PackageError> {
    let mut bytes = Vec::new();
    raw::read(
        &mut reader.by_ref().take(OUTPUT_LIMIT.saturating_add(1)),
        &mut bytes,
    )?;
    std::io::copy(&mut reader, &mut std::io::sink())?;
    require(
        u64::try_from(bytes.len()).is_ok_and(|length| length <= OUTPUT_LIMIT),
        "native packaging output exceeds its bound",
    )?;
    Ok(bytes)
}

fn native(stage: &'static str, program: &str, args: &[OsString]) -> Result<Vec<u8>, PackageError> {
    let mut command = crate::raw::command(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in crate::release::SECRET_ENVIRONMENT {
        command.env_remove(name);
    }
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or(PackageError::Invalid("native output is missing"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or(PackageError::Invalid("native diagnostic is missing"))?;
    std::thread::scope(|scope| {
        let out = scope.spawn(|| bounded(stdout));
        let err = scope.spawn(|| bounded(stderr));
        let status = child.wait()?;
        let output = out
            .join()
            .map_err(|_panic| PackageError::Invalid("native output reader stopped"))??;
        let diagnostic = err
            .join()
            .map_err(|_panic| PackageError::Invalid("native diagnostic reader stopped"))??;
        if !status.success() {
            return Err(PackageError::Native(stage));
        }
        Ok(if stage == "inspect signed binary" {
            diagnostic
        } else {
            output
        })
    })
}

mod raw {
    #![expect(
        clippy::disallowed_methods,
        reason = "macOS packaging owns bounded staged public payloads and invokes native packaging tools"
    )]
    use std::io;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    pub(super) fn metadata(path: &Path) -> io::Result<std::fs::Metadata> {
        std::fs::symlink_metadata(path)
    }
    pub(super) fn open(path: &Path) -> io::Result<std::fs::File> {
        std::fs::File::open(path)
    }
    pub(super) fn read(reader: &mut impl io::Read, bytes: &mut Vec<u8>) -> io::Result<usize> {
        reader.read_to_end(bytes)
    }
    pub(super) fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
        std::fs::write(path, bytes)
    }
    pub(super) fn copy(source: &Path, destination: &Path) -> io::Result<u64> {
        std::fs::copy(source, destination)
    }
    pub(super) fn create_directory(path: &Path) -> io::Result<()> {
        std::fs::DirBuilder::new().create(path)
    }
    pub(super) fn read_dir(path: &Path) -> io::Result<std::fs::ReadDir> {
        std::fs::read_dir(path)
    }
    pub(super) fn hard_link(source: &Path, destination: &Path) -> io::Result<()> {
        std::fs::hard_link(source, destination)
    }
    pub(super) fn temporary_directory(parent: &Path) -> io::Result<tempfile::TempDir> {
        tempfile::Builder::new()
            .prefix(".macos-package-")
            .tempdir_in(parent)
    }
    #[cfg(unix)]
    pub(super) fn permissions(path: &Path, executable: bool) -> io::Result<()> {
        std::fs::set_permissions(
            path,
            std::fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
        )
    }
    #[cfg(unix)]
    pub(super) fn mode(metadata: &std::fs::Metadata) -> u32 {
        metadata.permissions().mode()
    }
}

#[cfg(test)]
mod tests {
    use super::{PackageBuild, PackageError, architecture, attributes, build_with, package_name};

    use super::raw;

    #[test]
    fn targets_versions_and_duplicate_metadata_fail_closed() {
        for target in ["", "x86_64-unknown-linux-gnu", "../aarch64-apple-darwin"] {
            architecture(target).unwrap_err();
        }
        for version in ["../1.0.0", "v1.0.0", "01.0.0", ""] {
            package_name(version, "aarch64-apple-darwin").unwrap_err();
        }
        attributes(
            b"<options hostArchitectures=\"arm64\" hostArchitectures=\"x86_64\"/>",
            "options",
        )
        .unwrap_err();
        attributes(
            b"<!DOCTYPE options><options hostArchitectures=\"arm64\"/>",
            "options",
        )
        .unwrap_err();
        attributes(b"<options hostArchitectures=\"&#97;rm64\"/>", "options").unwrap_err();
    }

    #[test]
    fn unsafe_sources_never_reach_packaging() {
        let root = tempfile::tempdir().unwrap();
        let result = build_with(
            PackageBuild {
                root: root.path(),
                target: "aarch64-apple-darwin",
                version: "1.2.3",
                binary: &root.path().join("missing"),
                signing: None,
            },
            &mut |_stage, _program, _args| panic!("unexpected native command"),
        );
        assert!(matches!(result, Err(PackageError::Io(_))));
    }

    #[test]
    fn embedded_certificate_cannot_replace_the_active_installer_leaf() {
        let root = tempfile::tempdir().unwrap();
        let certificate = b"synthetic embedded public certificate";
        let identity = "a".repeat(40);
        let mut reached_sha1 = false;
        let error = super::verify_installer_with(&root.path().join("package.pkg"), &identity, &mut |stage, _program, args| {
            match stage {
                "verify installer signature" => Ok(format!("Status: signed by a developer certificate issued by Apple for distribution\nSigned with a trusted timestamp on: 2026-10-02 00:00:00 +0000\n1. Developer ID Installer: Different Publisher\nSHA256 Fingerprint:\n{}\n", ["00"; 32].join(" ")).into_bytes()),
                "read installer certificate" => {
                    let path = args.iter().find_map(|argument| argument.to_str().and_then(|value| value.strip_prefix("--dump-toc="))).unwrap();
                    raw::write(std::path::Path::new(path), format!("<xar><unrelated><X509Certificate>{}</X509Certificate></unrelated></xar>", data_encoding::BASE64.encode(certificate)).as_bytes()).unwrap();
                    Ok(Vec::new())
                }
                "check installer fingerprint" => { reached_sha1 = true; Ok(format!("SHA1 Fingerprint={identity}\n").into_bytes()) }
                _ => panic!("unexpected verification stage"),
            }
        }).unwrap_err();
        assert!(matches!(
            error,
            PackageError::Invalid("installer certificate does not match the active package signer")
        ));
        assert!(!reached_sha1);
    }

    #[cfg(target_os = "macos")]
    fn reject_changed_distribution(extracted: &super::ExtractedPackage, target: &str) {
        let distribution = String::from_utf8(
            super::read(
                &extracted.directory.path().join("expanded/Distribution"),
                super::OUTPUT_LIMIT,
            )
            .unwrap(),
        )
        .unwrap();
        for (old, changed) in [
            ("#payload.pkg", "https://example.invalid/other.pkg"),
            ("visible=\"false\"", "visible=\"system.run('other')\""),
            (
                "onConclusion=\"none\"",
                "onConclusionScript=\"system.run('other')\"",
            ),
            ("enable_anywhere=\"false\"", "enable_anywhere=\"true\""),
        ] {
            assert!(distribution.contains(old));
            super::verify_distribution(
                distribution.replace(old, changed).as_bytes(),
                target,
                "1.2.3",
            )
            .unwrap_err();
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_script_free_package_matches_the_closed_payload_contract() {
        let root = tempfile::tempdir().unwrap();
        for name in ["README.md", "LICENSE-MIT", "LICENSE-APACHE", "binary"] {
            raw::write(&root.path().join(name), name.as_bytes()).unwrap();
        }
        for target in ["aarch64-apple-darwin", "x86_64-apple-darwin"] {
            let binary = root.path().join("binary");
            let input = PackageBuild {
                root: root.path(),
                target,
                version: "1.2.3",
                binary: &binary,
                signing: None,
            };
            let package = build_with(input, &mut super::native).unwrap();
            assert!(package.try_exists().unwrap());
            let extracted =
                super::inspect_payload(&package, target, "1.2.3", &mut super::native).unwrap();
            assert_eq!(super::read(&extracted.binary, 128).unwrap(), b"binary");
            reject_changed_distribution(&extracted, target);
            raw::permissions(&extracted.binary, false).unwrap();
            super::walk(
                &extracted
                    .directory
                    .path()
                    .join("expanded/payload.pkg/Payload"),
                "",
                &mut std::collections::BTreeSet::new(),
            )
            .unwrap_err();
            raw::permissions(&extracted.binary, true).unwrap();
            super::inspect_payload(&package, target, "9.9.9", &mut super::native).unwrap_err();
            let other = if target == "aarch64-apple-darwin" {
                "x86_64-apple-darwin"
            } else {
                "aarch64-apple-darwin"
            };
            super::inspect_payload(&package, other, "1.2.3", &mut super::native).unwrap_err();
            extracted.directory.close().unwrap();
            build_with(input, &mut |_stage, _program, _args| {
                panic!("unexpected overwrite")
            })
            .unwrap_err();
        }
    }
}
