//! Compile-time rejection of unsound usage. Note: aliasing *within* a query such as
//! `(&mut A, &mut A)` is rejected at runtime instead (see `aliasing_queries_rejected`).
//!
//! The expected `.stderr` files are the diagnostics of the pinned toolchain
//! (`rust-toolchain.toml`). Pre-release compilers word and order them differently (for
//! example rustc 1.101.0-nightly reports the `Sync` bound of `non_send_component.rs` before
//! the `Send` one), which is a diagnostics change, not a soundness change. So the test is
//! skipped, with a message on stderr, when the compiler trybuild will use is a `-nightly`,
//! `-beta` or `-dev` build (as with `cargo +nightly test` for ASan). Every stable compiler
//! still runs it. `CANOPY_UI_TESTS=force` runs it on any compiler. Skipped under Miri.
#![cfg(not(miri))]

use std::process::Command;

/// Why the UI tests should not run with this compiler, if they should not.
fn skip_reason() -> Option<String> {
    if std::env::var("CANOPY_UI_TESTS").as_deref() == Ok("force") {
        return None;
    }
    // trybuild runs `$CARGO`, which compiles with `$RUSTC` or the `rustc` on `PATH` (the
    // rustup proxy honours the `RUSTUP_TOOLCHAIN` that `cargo +toolchain` exports).
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    // If rustc cannot be run, do not skip: trybuild then fails loudly.
    let out = Command::new(rustc).arg("--version").output().ok()?;
    let version = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let pre_release = ["-nightly", "-beta", "-dev"]
        .iter()
        .any(|tag| version.contains(tag));
    pre_release.then(|| {
        format!(
            "compile_fail skipped: `{version}` is a pre-release compiler whose diagnostics \
             differ from the pinned stable `.stderr` files (set CANOPY_UI_TESTS=force to run)"
        )
    })
}

#[test]
fn compile_fail() {
    if let Some(reason) = skip_reason() {
        eprintln!("{reason}");
        return;
    }
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}
