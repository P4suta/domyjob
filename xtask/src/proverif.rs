use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum ProofError {
    #[error("{action} {path}: {source}")]
    Run {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("ProVerif failed on {path}: {detail}")]
    Failed { path: PathBuf, detail: String },
    #[error("ProVerif returned unexpected results for {path}: {results:?}")]
    Results { path: PathBuf, results: Vec<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Claim {
    Secrecy,
    ClientOrigin,
}

fn claim(result: &str) -> Option<Claim> {
    let proposition = result.strip_suffix(" is true.")?;
    if proposition == "not attacker(payload[])"
        || proposition
            .strip_prefix("not attacker_p")
            .is_some_and(|tail| {
                let Some((phase, rest)) = tail.split_once('(') else {
                    return false;
                };
                !phase.is_empty()
                    && phase.bytes().all(|byte| byte.is_ascii_digit())
                    && rest == "payload[])"
            })
    {
        return Some(Claim::Secrecy);
    }
    if proposition.starts_with("event(ServerAccepted(")
        && proposition.contains(")) ==> event(ClientSent(")
        && proposition.ends_with("))")
    {
        return Some(Claim::ClientOrigin);
    }
    None
}

fn results(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| line.strip_prefix("RESULT ").map(str::to_owned))
        .collect()
}

fn check(path: &Path, output: &str, expected: &[Claim]) -> Result<(), ProofError> {
    let found = results(output);
    let claims: Option<Vec<_>> = found.iter().map(|line| claim(line)).collect();
    if claims.as_deref() == Some(expected) {
        Ok(())
    } else {
        Err(ProofError::Results {
            path: path.to_path_buf(),
            results: found,
        })
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "the repository task starts the ProVerif CLI and passes only model paths"
)]
pub fn verify(root: &Path) -> Result<(), ProofError> {
    let models: &[(&str, &[Claim])] = &[
        ("connect-x25519-broken.pv", &[Claim::Secrecy]),
        (
            "connect-ml-kem-broken.pv",
            &[Claim::Secrecy, Claim::ClientOrigin],
        ),
    ];
    for (file, expected) in models {
        let path = root.join("docs/security/proverif").join(file);
        let output = Command::new("proverif")
            .arg(&path)
            .output()
            .map_err(|source| ProofError::Run {
                action: "running ProVerif on",
                path: path.clone(),
                source,
            })?;
        if !output.status.success() {
            return Err(ProofError::Failed {
                path,
                detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        let report = String::from_utf8_lossy(&output.stdout);
        check(&path, &report, expected)?;
        println!("{}: {} proof(s) true", file, expected.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_unknown_missing_or_extra_query_fails_the_gate() {
        let path = Path::new("model.pv");
        let secrecy = "RESULT not attacker_p1(payload[]) is true.\n";
        let origin = "RESULT event(ServerAccepted(cpub,spub_1)) ==> event(ClientSent(cpub,spub_1)) is true.\n";
        check(
            path,
            &format!("{secrecy}{origin}"),
            &[Claim::Secrecy, Claim::ClientOrigin],
        )
        .unwrap();
        let duplicate = format!("{secrecy}{secrecy}");
        for output in [
            "",
            "RESULT not attacker_p1(payload[]) is false.\n",
            "RESULT not attacker_p1(payload[]) cannot be proved.\n",
            &duplicate,
            origin,
        ] {
            check(path, output, &[Claim::Secrecy]).unwrap_err();
        }
    }
}
