fn main() {
    match std::env::var("TARGET") {
        Ok(target) => println!("cargo:rustc-env=DOMYJOB_TARGET={target}"),
        Err(error) => println!(
            "cargo:warning=TARGET is unavailable ({error}); self update will refuse to run"
        ),
    }
    println!("cargo:rerun-if-changed=build.rs");
}
