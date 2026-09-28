#![expect(
    clippy::redundant_pub_crate,
    reason = "the binary composition root uses this private module"
)]

use domyjob_core::wire::BuildId;

macro_rules! sources {
    ($($path:literal),* $(,)?) => {
        const SOURCES: &[(&str, &[u8])] = &[$(($path, include_bytes!($path))),*];
    };
}

sources!(
    "../../domyjob-core/src/domain.rs",
    "../../domyjob-core/src/ingress.rs",
    "../../domyjob-core/src/lib.rs",
    "../../domyjob-core/src/state.rs",
    "../../domyjob-core/src/wire.rs",
    "../../domyjob/src/domain.rs",
    "../../domyjob/src/durable.rs",
    "../../domyjob/src/lock.rs",
    "../../domyjob/src/paths.rs",
    "../../domyjob/src/platform.rs",
    "../../domyjob/src/proc.rs",
    "../../domyjob/src/snapshot.rs",
    "../../domyjob/src/spawn.rs",
    "../../domyjob/src/state_file.rs",
    "../../domyjob/src/template.rs",
    "../../domyjob/src/tree.rs",
    "../../domyjob/src/watch_event.rs",
    "app.rs",
    "identity.rs",
    "main.rs",
    "store.rs",
    "transport.rs",
    "../../domyjob-core/Cargo.toml",
    "../../domyjob/Cargo.toml",
    "../Cargo.toml",
    "../../../Cargo.lock",
);

fn hash_sources<'a>(sources: impl IntoIterator<Item = &'a [u8]>) -> BuildId {
    let mut value = 0xcbf2_9ce4_8422_2325_u64;
    for source in sources {
        for byte in source {
            value ^= u64::from(*byte);
            value = value.wrapping_mul(0x0000_0100_0000_01b3);
        }
        value ^= 0xff;
        value = value.wrapping_mul(0x0000_0100_0000_01b3);
    }
    BuildId::from_fingerprint(value)
}

pub(crate) fn current() -> BuildId {
    hash_sources(SOURCES.iter().map(|(_path, bytes)| *bytes))
}

pub(crate) fn checkout() -> Result<Option<BuildId>, std::io::Error> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    match std::fs::metadata(&root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_other) => {
            return Err(std::io::Error::other(
                "the source checkout is not a directory",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let mut sources = Vec::with_capacity(SOURCES.len());
    let mut total = 0_u64;
    for (path, _embedded) in SOURCES {
        let path = root.join(path);
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(std::io::Error::other(
                "the source checkout contains a non-file",
            ));
        }
        total = total
            .checked_add(metadata.len())
            .ok_or_else(|| std::io::Error::other("the source checkout size overflowed"))?;
        if total > 67_108_864 {
            return Err(std::io::Error::other("the source checkout exceeds 64 MiB"));
        }
        let bytes = std::fs::read(path)?;
        sources.push(bytes);
    }
    Ok(Some(hash_sources(sources.iter().map(Vec::as_slice))))
}
