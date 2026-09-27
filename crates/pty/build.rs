//! Stamps `PTY_VERSION` = `<CARGO_PKG_VERSION>+<short-sha>` into the binary,
//! e.g. `0.13.0-rust+1a2b3c4`.
//!
//! The short SHA comes from `PTY_BUILD_SHA` when set (nix builds have no
//! `.git`), else `git rev-parse --short HEAD` run in this crate's repository,
//! else `unknown`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=PTY_BUILD_SHA");

    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let sha = match std::env::var("PTY_BUILD_SHA") {
        Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => git_short_sha(&manifest_dir).unwrap_or_else(|| "unknown".to_string()),
    };

    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    println!("cargo:rustc-env=PTY_VERSION={version}+{sha}");

    if std::env::var("TARGET").is_ok_and(|target| target.contains("apple-darwin")) {
        build_darwin_socket_owner(&manifest_dir);
    }
}

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn git_short_sha(repo: &Path) -> Option<String> {
    let sha = git(repo, &["rev-parse", "--short", "HEAD"])?;

    // Rebuild when HEAD moves: watch HEAD itself and, for a symbolic HEAD, the
    // branch ref it points at. `--git-path` resolves both for worktrees.
    // Only existing files are declared; a missing path would make cargo
    // rerun the script on every build.
    if let Some(head) = git(repo, &["rev-parse", "--git-path", "HEAD"]) {
        let head = repo.join(head);
        if head.exists() {
            println!("cargo:rerun-if-changed={}", head.display());
        }
        if let Ok(content) = std::fs::read_to_string(&head)
            && let Some(target) = content.trim().strip_prefix("ref: ")
            && let Some(ref_path) = git(repo, &["rev-parse", "--git-path", target])
        {
            let ref_path = repo.join(ref_path);
            if ref_path.exists() {
                println!("cargo:rerun-if-changed={}", ref_path.display());
            }
        }
    }
    Some(sha)
}

fn build_darwin_socket_owner(manifest_dir: &Path) {
    let source = manifest_dir.join("native/darwin_socket_owner.c");
    println!("cargo:rerun-if-changed={}", source.display());
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let object = out.join("darwin_socket_owner.o");
    let archive = out.join("libpty_darwin_socket_owner.a");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let status = Command::new(cc)
        .args(["-std=c11", "-Wall", "-Wextra", "-c"])
        .arg(&source)
        .arg("-o")
        .arg(&object)
        .status()
        .expect("compile Darwin socket ownership boundary");
    assert!(
        status.success(),
        "Darwin socket ownership boundary did not compile"
    );
    let ar = std::env::var("AR").unwrap_or_else(|_| "ar".to_string());
    let status = Command::new(ar)
        .arg("crus")
        .arg(&archive)
        .arg(&object)
        .status()
        .expect("archive Darwin socket ownership boundary");
    assert!(
        status.success(),
        "Darwin socket ownership boundary did not archive"
    );
    // Not `rustc-link-lib`: Cargo gives that only to a package's library
    // target when there is one, and this package has one (the embedding
    // handle). The shim's caller is the daemon, which is in the binary, so
    // the binary stopped linking it and failed with an undefined
    // `_pty_inspect_socket_owner_darwin`. A link argument reaches every target
    // that is linked: the binary and its unit tests. The linker takes from
    // the archive only what a target references.
    println!("cargo:rustc-link-arg={}", archive.display());
}
