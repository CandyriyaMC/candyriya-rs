//! Build-time metadata for the binary.
//!
//! Captures the short git SHA of the built revision (8 characters, like
//! the notes elsewhere in the release notes) into `CANDYRIYA_BUILD_SHA`,
//! falling back to `unknown` outside a git checkout or when git is not
//! available. The value is only used for `--version` output.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=CANDYRIYA_BUILD_SHA");

    let sha = std::env::var("CANDYRIYA_BUILD_SHA")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            let output = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            let raw = String::from_utf8(output.stdout).ok()?;
            let raw = raw.trim();
            (!raw.is_empty()).then(|| raw.chars().take(8).collect())
        })
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=CANDYRIYA_BUILD_SHA={sha}");
}
