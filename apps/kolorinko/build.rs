//! Compiles the `steer-bpf` eBPF program (see `src/steer.rs`) into
//! `OUT_DIR/kolorinko-steer` for it to `include_bytes!`. The BPF toolchain
//! (`bpf-linker` + a nightly with `rust-src`) is a soft requirement: anywhere
//! it's missing — or a non-Linux host, or `AYA_BUILD_SKIP=1` — an empty
//! placeholder is written instead, and the runtime loader reports steering
//! unavailable, leaving dispatch on the kernel's 4-tuple hash.

use std::{env, path::PathBuf};

fn main() {
    let object =
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR set")).join("kolorinko-steer");
    #[cfg(target_os = "linux")]
    match aya_build::build_ebpf(
        [aya_build::Package {
            name: "kolorinko-steer-bpf",
            root_dir: "steer-bpf",
            no_default_features: false,
            features: &[],
        }],
        aya_build::Toolchain::default(),
    ) {
        Ok(()) if object.exists() => return,
        // `AYA_BUILD_SKIP` (or a deleted object): keep the placeholder below.
        Err(e) => println!(
            "cargo:warning=steer-bpf not built ({e:#}); QUIC dispatch stays on the kernel hash"
        ),
        _ => {}
    }
    std::fs::write(&object, []).expect("write the empty steering placeholder");
}
