use std::process::Command;

fn main() {
    // Embed short git hash at build time
    let hash = git_output(&["rev-parse", "--short", "HEAD"]);
    println!("cargo:rustc-env=GIT_HASH={hash}");

    // Derive version from latest git tag (e.g. "v0.6.4" → "0.6.4").
    // Falls back to CARGO_PKG_VERSION from Cargo.toml when no tag exists.
    let version = git_output(&["describe", "--tags", "--abbrev=0"]);
    if !version.is_empty() {
        let version = version.strip_prefix('v').unwrap_or(&version);
        println!("cargo:rustc-env=DESKD_VERSION={version}");
    }

    // Re-run if HEAD changes (new commit / tag)
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/tags");
}

fn git_output(args: &[&str]) -> String {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout).ok()
            } else {
                None
            }
        })
        .unwrap_or_default()
        .trim()
        .to_string()
}
