//! Runs `scripts/release-body` against a scratch copy of the repository's
//! changelog files for the three situations the release workflow meets.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FRAGMENT: &str = "---\nkind: fixed\n---\nSomething users hit is fixed.\n";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A scratch checkout with the repository's changelog files and, optionally,
/// one fragment.
fn checkout(name: &str, fragment: bool) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("changelog.d")).expect("create scratch checkout");
    for file in ["CHANGELOG.md", "changelog.toml"] {
        fs::copy(repo_root().join(file), dir.join(file)).expect("copy changelog file");
    }
    if fragment {
        fs::write(dir.join("changelog.d/9999-example.md"), FRAGMENT).expect("write fragment");
    }
    dir
}

/// The newest committed version, read from the scratch CHANGELOG.md.
fn committed_version(dir: &Path) -> String {
    let text = fs::read_to_string(dir.join("CHANGELOG.md")).expect("read CHANGELOG.md");
    let heading = text
        .lines()
        .filter_map(|line| line.strip_prefix("## "))
        .filter_map(|heading| heading.split(' ').next())
        .find(|version| version.split('.').count() == 3 && !version.contains('-'))
        .expect("an X.Y.Z version section");
    heading.to_string()
}

fn release_body(dir: &Path, version: &str, mode: &str) -> Output {
    Command::new(repo_root().join("scripts/release-body"))
        .args([version, mode])
        .current_dir(dir)
        .env("CHANGELOG_TOOL", env!("CARGO_BIN_EXE_changelog"))
        .output()
        .expect("run scripts/release-body")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn dry_run_with_a_section_and_a_fragment_previews_the_next_patch() {
    let dir = checkout("release-body-pr", true);
    let version = committed_version(&dir);
    let output = release_body(&dir, &version, "dry-run");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body = stdout(&output);
    assert!(
        body.starts_with("<!-- pohunek-changelog:start -->"),
        "{body}"
    );
    assert!(
        body.trim_end().ends_with("<!-- pohunek-changelog:end -->"),
        "{body}"
    );
    assert!(body.contains("Something users hit is fixed."), "{body}");
    assert!(
        dir.join("changelog.d/9999-example.md").exists(),
        "the checkout is not folded"
    );
}

#[test]
fn dry_run_without_fragments_prints_the_committed_section() {
    let dir = checkout("release-body-clean", false);
    let version = committed_version(&dir);
    let output = release_body(&dir, &version, "dry-run");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body = stdout(&output);
    assert!(body.contains("pohunek-changelog:start"), "{body}");
    assert!(!body.contains("Something users hit"), "{body}");
}

#[test]
fn tag_push_refuses_unfolded_fragments() {
    let dir = checkout("release-body-tag", true);
    let version = committed_version(&dir);

    let output = release_body(&dir, &version, "push");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not folded"), "{stderr}");
}
