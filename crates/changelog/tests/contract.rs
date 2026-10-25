//! Runs the ecosystem changelog contract fixtures byte for byte and checks
//! every invalid input listed in the contract.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use pohunek_companion_changelog::{CONFIG_FILE, Config, Layout, check, fold, notes};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn fixture(path: &str) -> PathBuf {
    Path::new(FIXTURES).join(path)
}

/// Copies a fixture tree into a fresh scratch directory and returns it.
fn scratch(name: &str, source: &Path) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    copy_tree(source, &dir);
    dir
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create scratch directory");
    for entry in fs::read_dir(from).expect("read fixture directory") {
        let entry = entry.expect("fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn single_config() -> Config {
    Config {
        repository: "example/app".to_owned(),
        layout: Layout::Single {
            changelog: "CHANGELOG.md".into(),
        },
    }
}

fn multi_config() -> Config {
    let surfaces = BTreeMap::from([
        ("plugin".to_owned(), PathBuf::from("plugin/CHANGELOG.md")),
        ("web".to_owned(), PathBuf::from("web/CHANGELOG.md")),
    ]);
    Config {
        repository: "example/work".to_owned(),
        layout: Layout::Surfaces(surfaces),
    }
}

fn file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("read changelog.d")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

#[test]
fn single_product_fold_and_notes_match_fixtures() {
    let root = scratch("single", &fixture("single/input"));
    let config = single_config();

    fold(&root, &config, "0.37.0", "2026-10-24", None).expect("fold 0.37.0");
    assert_eq!(
        read(&root.join("CHANGELOG.md")),
        read(&fixture("single/expected-0.37.0/CHANGELOG.md"))
    );
    assert!(file_names(&root.join("changelog.d")).is_empty());
    assert_eq!(
        notes(&root, &config, "0.37.0", None).expect("notes 0.37.0"),
        read(&fixture("single/expected-0.37.0/notes.md"))
    );

    fold(&root, &config, "0.37.1", "2026-10-25", None).expect("fold 0.37.1");
    assert_eq!(
        read(&root.join("CHANGELOG.md")),
        read(&fixture("single/expected-0.37.1/CHANGELOG.md"))
    );
    assert_eq!(
        notes(&root, &config, "0.37.1", None).expect("notes 0.37.1"),
        read(&fixture("single/expected-0.37.1/notes.md"))
    );
}

#[test]
fn surface_fold_removes_only_the_released_surface() {
    let root = scratch("multi", &fixture("multi/input"));
    let config = multi_config();

    fold(&root, &config, "0.8.0", "2026-10-24", Some("plugin")).expect("fold plugin 0.8.0");

    assert_eq!(
        read(&root.join("plugin/CHANGELOG.md")),
        read(&fixture("multi/expected-plugin-0.8.0/plugin/CHANGELOG.md"))
    );
    assert_eq!(
        read(&root.join("web/CHANGELOG.md")),
        read(&fixture("multi/input/web/CHANGELOG.md"))
    );
    let expected_dir = fixture("multi/expected-plugin-0.8.0/changelog.d");
    assert_eq!(
        file_names(&root.join("changelog.d")),
        file_names(&expected_dir)
    );
    for name in file_names(&expected_dir) {
        assert_eq!(
            read(&root.join("changelog.d").join(&name)),
            read(&expected_dir.join(&name)),
            "fragment {name}"
        );
    }
}

#[test]
fn notes_refuse_unfolded_fragments() {
    let root = scratch("unfolded", &fixture("single/input"));
    let config = single_config();

    let error = notes(&root, &config, "0.37.0", None).expect_err("fragments remain");
    assert!(error.to_string().contains("not folded"), "{error}");
}

#[test]
fn fold_refuses_an_existing_version() {
    let root = scratch("existing", &fixture("single/input"));
    let config = single_config();
    fold(&root, &config, "0.37.0", "2026-10-24", None).expect("first fold");

    let error = fold(&root, &config, "0.37.0", "2026-10-24", None).expect_err("duplicate");
    assert!(
        error.to_string().contains("already has a section"),
        "{error}"
    );
}

const VALID: &str = "---\nkind: added\n---\nA change.\n";

/// Each case is a fragment file name and content that check must reject in a
/// single-product repository, one per invalid input listed in the contract.
const INVALID_SINGLE: &[(&str, &str)] = &[
    ("1-a.md", "kind: added\n---\nBody.\n"),
    ("1-a.md", "---\nkind: added\nBody.\n"),
    ("1-a.md", "---\nkind: added\ncolour: red\n---\nBody.\n"),
    ("1-a.md", "---\nkind: added\nkind: fixed\n---\nBody.\n"),
    ("1-a.md", "---\nbreaking: true\n---\nBody.\n"),
    ("1-a.md", "---\nkind: improved\n---\nBody.\n"),
    ("1-a.md", "---\nkind: added\nbreaking: yes\n---\nBody.\n"),
    ("1-a.md", "---\nkind: added\n---\n\n\n"),
    ("1-a.md", "---\nkind: added\n---\nFirst.\n\nSecond.\n"),
    ("1-a.md", "---\nkind: added\n---\n# Title\n"),
    ("x-foo.md", VALID),
    ("012-foo.md", VALID),
    ("12-Foo.md", VALID),
    ("12_foo.md", VALID),
    ("1-a.md", "---\nkind: added\nsurfaces: plugin\n---\nBody.\n"),
    ("1-a.md", "---\r\nkind: added\r\n---\r\nBody.\r\n"),
    ("1-a.md", "---\nkind:  added\n---\nBody.\n"),
];

/// Multi-surface repositories: surfaces are required, must be declared, and
/// must not repeat.
const INVALID_MULTI: &[(&str, &str)] = &[
    ("1-a.md", VALID),
    ("1-a.md", "---\nkind: added\nsurfaces: cli\n---\nBody.\n"),
    (
        "1-a.md",
        "---\nkind: added\nsurfaces: plugin, plugin\n---\nBody.\n",
    ),
];

/// Writes one fragment into a fresh repository and reports whether check accepts it.
fn check_accepts(prefix: &str, case: usize, name: &str, content: &str, config: &Config) -> bool {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("check-{prefix}-{case}"));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("changelog.d")).expect("create changelog.d");
    fs::write(root.join("changelog.d").join(name), content).expect("write fragment");
    check(&root, config).is_ok()
}

#[test]
fn check_rejects_every_invalid_single_product_input() {
    let config = single_config();
    for (case, (name, content)) in INVALID_SINGLE.iter().enumerate() {
        assert!(
            !check_accepts("single", case, name, content, &config),
            "accepted {name}: {content:?}"
        );
    }
    assert!(check_accepts(
        "single",
        usize::MAX,
        "1-a.md",
        VALID,
        &config
    ));
}

#[test]
fn check_rejects_every_invalid_multi_surface_input() {
    let config = multi_config();
    for (case, (name, content)) in INVALID_MULTI.iter().enumerate() {
        assert!(
            !check_accepts("multi", case, name, content, &config),
            "accepted {name}: {content:?}"
        );
    }
    assert!(check_accepts(
        "multi",
        usize::MAX,
        "1-a.md",
        "---\nkind: added\nsurfaces: plugin, web\n---\nBody.\n",
        &config
    ));
}

#[test]
fn repository_changelog_config_is_valid_and_fragments_check() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let config = Config::load(&root.join(CONFIG_FILE)).expect("repository changelog.toml");
    assert_eq!(config, single_config_for_repository());
    check(&root, &config).expect("repository fragments are valid");
}

fn single_config_for_repository() -> Config {
    Config {
        repository: "pohunekd/pohunek-companion".to_owned(),
        layout: Layout::Single {
            changelog: "CHANGELOG.md".into(),
        },
    }
}
