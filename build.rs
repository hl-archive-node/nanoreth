use std::{env, error::Error};
use vergen::{Build, Cargo, Emitter};
use vergen_git2::Git2;

fn main() -> Result<(), Box<dyn Error>> {
    let mut emitter = Emitter::default();

    let build_builder = Build::builder().build_timestamp(true).build();

    emitter.add_instructions(&build_builder)?;

    let cargo_builder = Cargo::builder().features(true).target_triple(true).build();

    emitter.add_instructions(&cargo_builder)?;

    let git_builder = Git2::builder().describe(false, true, None).dirty(true).sha(false).build();

    emitter.add_instructions(&git_builder)?;

    // Git metadata is best-effort. `emit_and_set` fails outright when the repository cannot be
    // read -- a source tarball, or a container where libgit2 rejects the checkout's ownership --
    // and a missing commit string is not a reason to fail the build. The reads below fall back to
    // placeholders when that happens.
    if let Err(err) = emitter.emit_and_set() {
        println!("cargo:warning=version metadata unavailable ({err}); using placeholders");
    }

    // Git metadata is not always available: a source tarball, a vendored build, or a container
    // build whose repository libgit2 declines to open all leave these unset. Fall back to a
    // placeholder so the build still succeeds, rather than failing over a version string.
    let sha = env::var("VERGEN_GIT_SHA").unwrap_or_else(|_| "unknown".to_string());
    let sha_short = &sha[0..sha.len().min(7)];

    let is_dirty = env::var("VERGEN_GIT_DIRTY").is_ok_and(|dirty| dirty == "true");
    // > git describe --always --tags
    // if not on a tag: v0.2.0-beta.3-82-g1939939b
    // if on a tag: v0.2.0-beta.3
    let not_on_tag = env::var("VERGEN_GIT_DESCRIBE")
        .is_ok_and(|describe| describe.ends_with(&format!("-g{sha_short}")));
    let version_suffix = if is_dirty || not_on_tag { "-dev" } else { "" };
    println!("cargo:rustc-env=RETH_HL_VERSION_SUFFIX={version_suffix}");

    // Set short SHA
    println!("cargo:rustc-env=VERGEN_GIT_SHA_SHORT={}", &sha[..sha.len().min(8)]);

    // Set the build profile
    let out_dir = env::var("OUT_DIR").unwrap();
    let profile = out_dir.rsplit(std::path::MAIN_SEPARATOR).nth(3).unwrap();
    println!("cargo:rustc-env=RETH_HL_BUILD_PROFILE={profile}");

    // Set formatted version strings
    let pkg_version = env!("CARGO_PKG_VERSION");

    // The short version information for reth.
    // - The latest version from Cargo.toml
    // - The short SHA of the latest commit.
    // Example: 0.1.0 (defa64b2)
    println!("cargo:rustc-env=RETH_HL_SHORT_VERSION={pkg_version}{version_suffix} ({sha_short})");

    // LONG_VERSION
    // The long version information for reth.
    //
    // - The latest version from Cargo.toml + version suffix (if any)
    // - The full SHA of the latest commit
    // - The build datetime
    // - The build features
    // - The build profile
    //
    // Example:
    //
    // ```text
    // Version: 0.1.0
    // Commit SHA: defa64b2
    // Build Timestamp: 2023-05-19T01:47:19.815651705Z
    // Build Features: jemalloc
    // Build Profile: maxperf
    // ```
    println!("cargo:rustc-env=RETH_HL_LONG_VERSION_0=Version: {pkg_version}{version_suffix}");
    println!("cargo:rustc-env=RETH_HL_LONG_VERSION_1=Commit SHA: {sha}");
    println!(
        "cargo:rustc-env=RETH_HL_LONG_VERSION_2=Build Timestamp: {}",
        env::var("VERGEN_BUILD_TIMESTAMP").unwrap_or_else(|_| "unknown".to_string())
    );
    println!(
        "cargo:rustc-env=RETH_HL_LONG_VERSION_3=Build Features: {}",
        env::var("VERGEN_CARGO_FEATURES").unwrap_or_else(|_| "unknown".to_string())
    );
    println!("cargo:rustc-env=RETH_HL_LONG_VERSION_4=Build Profile: {profile}");

    // The version information for reth formatted for P2P (devp2p).
    // - The latest version from Cargo.toml
    // - The target triple
    //
    // Example: reth/v0.1.0-alpha.1-428a6dc2f/aarch64-apple-darwin
    println!(
        "cargo:rustc-env=RETH_HL_P2P_CLIENT_VERSION={}",
        format_args!(
            "reth/v{pkg_version}-{sha_short}/{}",
            env::var("VERGEN_CARGO_TARGET_TRIPLE").unwrap_or_else(|_| "unknown".to_string())
        )
    );

    Ok(())
}
