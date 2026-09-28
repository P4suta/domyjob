#![expect(
    clippy::redundant_pub_crate,
    reason = "the binary composition root uses this private module"
)]

use domyjob_core::wire::BuildId;

const SOURCES: &[&[u8]] = &[
    include_bytes!("../../domyjob-core/src/domain.rs"),
    include_bytes!("../../domyjob-core/src/ingress.rs"),
    include_bytes!("../../domyjob-core/src/lib.rs"),
    include_bytes!("../../domyjob-core/src/state.rs"),
    include_bytes!("../../domyjob-core/src/wire.rs"),
    include_bytes!("../../domyjob/src/domain.rs"),
    include_bytes!("../../domyjob/src/durable.rs"),
    include_bytes!("../../domyjob/src/lock.rs"),
    include_bytes!("../../domyjob/src/paths.rs"),
    include_bytes!("../../domyjob/src/platform.rs"),
    include_bytes!("../../domyjob/src/proc.rs"),
    include_bytes!("../../domyjob/src/snapshot.rs"),
    include_bytes!("../../domyjob/src/spawn.rs"),
    include_bytes!("../../domyjob/src/state_file.rs"),
    include_bytes!("../../domyjob/src/template.rs"),
    include_bytes!("../../domyjob/src/tree.rs"),
    include_bytes!("../../domyjob/src/watch_event.rs"),
    include_bytes!("app.rs"),
    include_bytes!("identity.rs"),
    include_bytes!("main.rs"),
    include_bytes!("store.rs"),
    include_bytes!("transport.rs"),
    include_bytes!("../../domyjob-core/Cargo.toml"),
    include_bytes!("../../domyjob/Cargo.toml"),
    include_bytes!("../Cargo.toml"),
    include_bytes!("../../../Cargo.lock"),
];

pub(crate) fn current() -> BuildId {
    let mut value = 0xcbf2_9ce4_8422_2325_u64;
    for source in SOURCES {
        for byte in *source {
            value ^= u64::from(*byte);
            value = value.wrapping_mul(0x0000_0100_0000_01b3);
        }
        value ^= 0xff;
        value = value.wrapping_mul(0x0000_0100_0000_01b3);
    }
    BuildId::from_fingerprint(value)
}
