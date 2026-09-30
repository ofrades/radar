//! Build libghostty-vt (Ghostty's VT core) from a pinned revision and link it
//! into radar.
//!
//! This runs only when the `ghostty` feature is enabled; without it the build
//! is untouched. See `docs/libghostty-vt.md` for the pin, the Zig
//! requirement, and the smoke test.
//!
//! Why build from source rather than link a packaged library: the `ghostty`
//! package on this machine ships libghostty-vt 0.1.0, which has no terminal or
//! snapshot API at all. The snapshot API radar needs is unreleased, so the
//! revision is pinned here and the ABI moves only when the pin moves.
//!
//! Environment knobs:
//! - `GHOSTTY_SOURCE_DIR`: build from an existing Ghostty checkout instead of
//!   fetching the pin. The checkout must already be at the pinned revision.
//! - `ZIG`: path to the Zig binary. Defaults to `zig` on `PATH`.
//! - `LIBGHOSTTY_VT_OPTIMIZE`: Zig `OptimizeMode` (Debug, ReleaseSafe,
//!   ReleaseFast, ReleaseSmall). Defaults to `ReleaseSmall`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The Ghostty revision the fidelity spike validated
/// (`target/spike-libghostty/`, 2026-09-30). libghostty-vt is pre-1.0: both the
/// C ABI and the snapshot format still change, so the pin moves only on
/// purpose and only with a re-run of that spike.
const GHOSTTY_REPO: &str = "https://github.com/ghostty-org/ghostty.git";
const GHOSTTY_COMMIT: &str = "4da7523faba68ccb4042ea20585817098a51c015";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=GHOSTTY_SOURCE_DIR");
    println!("cargo:rerun-if-env-changed=ZIG");
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_OPTIMIZE");

    // libghostty-vt is the terminal engine: the daemon, the GTK client and the
    // web client all need it, so it is built unconditionally. Zig 0.16.0 is
    // required (`.mise.toml`); the artifact is cached per build tree.

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR must be set"));
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let source = ghostty_source(&out_dir);
    let zig = zig_binary(&manifest_dir);

    let prefix = out_dir.join("ghostty-install");
    let cache = out_dir.join("zig-cache");
    let optimize =
        std::env::var("LIBGHOSTTY_VT_OPTIMIZE").unwrap_or_else(|_| "ReleaseSmall".to_owned());

    let mut build = Command::new(&zig);
    build
        .arg("build")
        .arg("-Demit-lib-vt=true")
        .arg(format!("-Doptimize={optimize}"))
        // Cargo artifacts may run on older CPUs than the build host; without a
        // fixed baseline Zig can emit host-specific instructions that trap on
        // another machine.
        .arg("-Dcpu=baseline")
        .arg("-Dapp-runtime=none")
        .arg("-Demit-xcframework=false")
        .arg("--prefix")
        .arg(&prefix)
        .arg("--cache-dir")
        .arg(&cache)
        .current_dir(&source);
    run(&mut build, "zig build libghostty-vt");

    let lib_dir = prefix.join("lib");
    let archive = lib_dir.join("libghostty-vt.a");
    assert!(
        archive.exists(),
        "expected a static libghostty-vt at {}; the zig build did not produce it",
        archive.display()
    );
    let include_dir = prefix.join("include");
    assert!(
        include_dir.join("ghostty").join("vt.h").exists(),
        "expected headers at {}",
        include_dir.display()
    );

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=ghostty-vt");
    // libghostty-vt is Zig-built C/C++: the archive needs the C++ runtime and
    // the usual POSIX libraries. rustc's own std does not pull these in for a
    // static archive, so name them explicitly on Linux.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        for lib in ["stdc++", "m", "pthread", "dl"] {
            println!("cargo:rustc-link-lib={lib}");
        }
    }
    println!("cargo:include={}", include_dir.display());

    // A pin bump must not reuse a stale checkout or a stale zig cache.
    println!(
        "cargo:rerun-if-changed={}",
        source.join("build.zig").display()
    );

    build_wasm(&zig, &source, &out_dir);
}

/// Build `ghostty-vt.wasm` for the browser client (web/WASM). Same pinned
/// source and Zig; a different target. The artifact lands in `OUT_DIR` for
/// `include_bytes!`.
fn build_wasm(zig: &Path, source: &Path, out_dir: &Path) {
    let wasm = out_dir.join("ghostty-vt.wasm");
    if wasm.exists() {
        return;
    }
    let prefix = out_dir.join("wasm-install");
    let cache = out_dir.join("wasm-cache");
    let mut build = Command::new(zig);
    build
        .arg("build")
        .arg("-Demit-lib-vt=true")
        .arg("-Dtarget=wasm32-freestanding")
        .arg("-Doptimize=ReleaseSmall")
        .arg("--prefix")
        .arg(&prefix)
        .arg("--cache-dir")
        .arg(&cache)
        .current_dir(source);
    run(&mut build, "zig build libghostty-vt.wasm");

    let built = prefix.join("bin").join("ghostty-vt.wasm");
    assert!(
        built.exists(),
        "expected a wasm module at {}",
        built.display()
    );
    std::fs::copy(&built, &wasm)
        .unwrap_or_else(|error| panic!("cannot place {}: {error}", wasm.display()));
}

/// Locate the Ghostty source: an explicit checkout wins, otherwise fetch the
/// pinned commit into `OUT_DIR` and reuse it while the stamp matches.
fn ghostty_source(out_dir: &Path) -> PathBuf {
    if let Some(dir) = std::env::var_os("GHOSTTY_SOURCE_DIR") {
        let path = PathBuf::from(dir);
        assert!(
            path.join("build.zig").exists(),
            "GHOSTTY_SOURCE_DIR does not contain build.zig: {}",
            path.display()
        );
        return path;
    }

    let source = out_dir.join("ghostty-src");
    let stamp = source.join(".ghostty-commit");
    let already_pinned = std::fs::read_to_string(&stamp)
        .map(|existing| existing.trim() == GHOSTTY_COMMIT)
        .unwrap_or(false);
    if already_pinned {
        return source;
    }

    if source.exists() {
        std::fs::remove_dir_all(&source)
            .unwrap_or_else(|error| panic!("cannot clear {}: {error}", source.display()));
    }
    eprintln!("radar: fetching ghostty {GHOSTTY_COMMIT} (once per build tree)");

    let mut clone = Command::new("git");
    clone
        .arg("clone")
        .arg("--filter=blob:none")
        .arg("--no-checkout")
        .arg(GHOSTTY_REPO)
        .arg(&source);
    run(&mut clone, "git clone ghostty");

    let mut checkout = Command::new("git");
    checkout
        .arg("checkout")
        .arg(GHOSTTY_COMMIT)
        .current_dir(&source);
    run(&mut checkout, "git checkout ghostty pin");

    std::fs::write(&stamp, GHOSTTY_COMMIT).expect("cannot write the ghostty pin stamp");
    source
}

/// Find Zig: `ZIG` first, then `PATH`.
///
/// The path is resolved to the real executable, because `zig build` must run
/// with the Ghostty checkout as its working directory and version-manager
/// shims (mise, asdf) pick their tool version from the current directory. A
/// shim invoked from the checkout would see no pin; the resolved binary does
/// not care where it runs.
///
/// Ghostty pins the exact minor version (`.mise.toml`), so a wrong one fails
/// inside `zig build` with Ghostty's own message rather than a confusing link
/// error later.
fn zig_binary(manifest_dir: &Path) -> PathBuf {
    if let Some(zig) = std::env::var_os("ZIG") {
        return PathBuf::from(zig);
    }

    let shim = std::env::var("PATH")
        .ok()
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("zig"))
                .find(|candidate| candidate.is_file())
        })
        .unwrap_or_else(|| {
            panic!(
                "`zig` not found. radar's `ghostty` feature builds libghostty-vt \
                 with Zig 0.16.0: run `mise install` (the pin lives in \
                 .mise.toml) or set ZIG=/path/to/zig."
            )
        });

    resolve_zig_exe(&shim, manifest_dir).unwrap_or(shim)
}

/// Ask Zig for its own executable path (`zig env` prints `.zig_exe = "..."`).
/// Run from `manifest_dir` so a version-manager shim resolves the pinned tool.
fn resolve_zig_exe(shim: &Path, manifest_dir: &Path) -> Option<PathBuf> {
    let output = Command::new(shim)
        .arg("env")
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().find_map(|line| {
        // `.zig_exe = "/path/to/zig",` — take what is between the quotes.
        let value = line.trim().strip_prefix(".zig_exe")?;
        let value = value.split('"').nth(1)?;
        (!value.is_empty()).then(|| PathBuf::from(value))
    })
}

fn run(command: &mut Command, what: &str) {
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("failed to run {what}: {error}"));
    assert!(status.success(), "{what} failed with {status}");
}
