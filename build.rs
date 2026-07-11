// Dynamically set the crate version from the latest Git tag, falling back to
// the Cargo package version when Git or tags are unavailable (e.g. crates.io
// or tarball builds, shallow CI clones).

use std::process::Command;

fn main() {
    // Re-run this script when the checked-out commit or refs change so the
    // embedded version never goes stale. These paths only exist inside a Git
    // checkout; when they are absent Cargo simply ignores them.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/tags");
    println!("cargo:rerun-if-changed=.git/packed-refs");

    let version = git_tag_version().unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    println!("cargo:rustc-env=APP_VERSION={version}");
}

/// Returns a version string derived from the closest Git tag:
///
/// * `TAG` when the current commit is exactly that tag (e.g. `0.1.5`).
/// * `TAG+SHORTHASH` when the current commit is ahead of the closest tag, using
///   the abbreviated commit hash (e.g. `0.1.5+bc7e4d2`). The short hash is used
///   instead of the commit distance because distance is not stable across
///   machines with different clones.
///
/// Any leading `v` on the tag is stripped. Returns `None` when Git is missing,
/// this is not a repository, or no tags exist.
fn git_tag_version() -> Option<String> {
    // `--long` always emits the full `TAG-DISTANCE-gSHA` form, including
    // `TAG-0-gSHA` when the commit is exactly on the tag.
    let output = Command::new("git")
        .args(["describe", "--tags", "--long"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let described = String::from_utf8(output.stdout).ok()?;
    let described = described.trim();
    if described.is_empty() {
        return None;
    }

    // Strip the trailing `-DISTANCE-gSHA`, splitting from the right so tags
    // that themselves contain hyphens are preserved.
    let (tag, distance, sha) = {
        let mut parts = described.rsplitn(3, '-');
        let sha = parts.next()?;
        let distance: u64 = parts.next()?.parse().ok()?;
        let tag = parts.next()?;
        (tag, distance, sha)
    };

    let tag = tag.strip_prefix('v').unwrap_or(tag);
    // `git describe` prefixes the abbreviated hash with `g` (e.g. `gbc7e4d2`);
    // drop it to expose the bare short hash.
    let short_hash = sha.strip_prefix('g').unwrap_or(sha);

    if distance == 0 {
        Some(tag.to_string())
    } else {
        Some(format!("{tag}+{short_hash}"))
    }
}
