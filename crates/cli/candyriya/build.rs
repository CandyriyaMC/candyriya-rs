//! Build-time metadata for the binary.
//!
//! Captures the short git SHA of the built revision (8 characters, like
//! the notes elsewhere in the release notes) into `CANDYRIYA_BUILD_SHA`,
//! falling back to `unknown` outside a git checkout or when git is not
//! available. The value is only used for `--version` output.

use std::path::{Component, Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=CANDYRIYA_BUILD_SHA");
    // without these cargo never re-runs us when git state moves and the
    // embedded sha goes stale (issue #307)
    emit_git_rerun_changed();

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

fn emit_git_rerun_changed() {
    // cargo runs this with the package root as cwd; .git can be a file
    // (linked worktree), so ask git instead of assuming a layout
    let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") else {
        return;
    };
    let manifest_dir = PathBuf::from(manifest_dir);

    let Some(git_dir) = git_rev_parse(&manifest_dir, "--git-dir") else {
        // no git / not a checkout: same output as before this existed
        return;
    };
    // branch/tag refs live in the common dir; a linked worktree gets a
    // private dir for HEAD only. fall back to git_dir if this fails
    let common_dir =
        git_rev_parse(&manifest_dir, "--git-common-dir").unwrap_or_else(|| git_dir.clone());

    let head_path = git_dir.join("HEAD");
    let Ok(head) = std::fs::read_to_string(&head_path) else {
        return;
    };

    println!("cargo:rerun-if-changed={}", head_path.display());

    // HEAD is either `ref: <name>` (branch/tag) or a raw sha (detached)
    if let Some(ref_name) = head
        .strip_prefix("ref:")
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        // covers new commits on the branch and rewritten refs; a ref that
        // vanished from disk (packed by gc) makes cargo re-run us every
        // build, which is the safe direction anyway
        println!(
            "cargo:rerun-if-changed={}",
            common_dir.join(ref_name).display()
        );
    }
    // detached HEAD: the HEAD line above is the only thing to watch
}

fn git_rev_parse(manifest_dir: &Path, flag: &str) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", flag])
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    Some(absolutize(manifest_dir, Path::new(raw)))
}

// git sometimes prints paths relative to the cwd; make them absolute
// without touching the fs (fs::canonicalize would add a \\?\ prefix on windows)
fn absolutize(base: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    lexically_normalize(&joined)
}

fn lexically_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                // never pop past the root
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}
