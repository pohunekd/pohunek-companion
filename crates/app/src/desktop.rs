//! The `desktop install` and `desktop uninstall` subcommands: an XDG desktop
//! entry and hicolor icons in the user's data directory, so application
//! launchers (`drun`, rofi, GNOME, KDE) list the app.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DATA_HOME_ENV: &str = "XDG_DATA_HOME";
const HOME_ENV: &str = "HOME";
const HOME_DATA_DIR: &str = ".local/share";

/// The entry as shipped in the release archive: `Exec` names the binary
/// without a directory, so it relies on `PATH`.
const ENTRY_TEMPLATE: &str = include_str!("../../../packaging/linux/pohunek-companion.desktop");
const ENTRY_FILE: &str = "pohunek-companion.desktop";
/// The `Exec` value the template carries; `install` replaces the program name.
const TEMPLATE_PROGRAM: &str = "pohunek-companion";

const ICON_FILE: &str = "pohunek-companion.png";
const SVG_FILE: &str = "pohunek-companion.svg";
const ICONS: [(&str, &[u8]); 7] = [
    (
        "16x16",
        include_bytes!("../../../assets/icons/hicolor/16x16/apps/pohunek-companion.png"),
    ),
    (
        "32x32",
        include_bytes!("../../../assets/icons/hicolor/32x32/apps/pohunek-companion.png"),
    ),
    (
        "48x48",
        include_bytes!("../../../assets/icons/hicolor/48x48/apps/pohunek-companion.png"),
    ),
    (
        "64x64",
        include_bytes!("../../../assets/icons/hicolor/64x64/apps/pohunek-companion.png"),
    ),
    (
        "128x128",
        include_bytes!("../../../assets/icons/hicolor/128x128/apps/pohunek-companion.png"),
    ),
    (
        "256x256",
        include_bytes!("../../../assets/icons/hicolor/256x256/apps/pohunek-companion.png"),
    ),
    (
        "512x512",
        include_bytes!("../../../assets/icons/hicolor/512x512/apps/pohunek-companion.png"),
    ),
];
const SVG: &[u8] =
    include_bytes!("../../../assets/icons/hicolor/scalable/apps/pohunek-companion.svg");

#[derive(Debug, thiserror::Error)]
pub enum DesktopError {
    #[error("cannot locate the data directory: neither {DATA_HOME_ENV} nor {HOME_ENV} is set")]
    NoDataDir,
    #[error("{DATA_HOME_ENV} must be an absolute path, got {}", .0.display())]
    RelativeDataHome(PathBuf),
    #[error("cannot determine the path of the running binary: {0}")]
    CurrentExe(std::io::Error),
    #[error("the binary path {} cannot be written to a desktop entry (not UTF-8, or it has a control character)", .0.display())]
    UnusableExecPath(PathBuf),
    #[error("{op} {path}: {source}")]
    Io {
        op: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
}

/// `$XDG_DATA_HOME`, else `~/.local/share`.
fn data_home(xdg: Option<OsString>, home: Option<OsString>) -> Result<PathBuf, DesktopError> {
    match xdg.filter(|value| !value.is_empty()) {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            if dir.is_absolute() {
                Ok(dir)
            } else {
                Err(DesktopError::RelativeDataHome(dir))
            }
        }
        None => match home.filter(|value| !value.is_empty()) {
            Some(home) => Ok(PathBuf::from(home).join(HOME_DATA_DIR)),
            None => Err(DesktopError::NoDataDir),
        },
    }
}

/// One argument of an `Exec` line, quoted per the Desktop Entry spec: an
/// argument with a reserved character is double-quoted with `"`, `` ` ``,
/// `$` and `\` backslash-escaped, and `%` is doubled so it is not a field code.
fn quote_exec_arg(arg: &str) -> String {
    const RESERVED: &str = " \t\n\"'\\><~|&;$*?#()`";
    if !arg.is_empty() && !arg.contains(|c| RESERVED.contains(c) || c == '%') {
        return arg.to_owned();
    }
    let mut out = String::from("\"");
    for c in arg.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '%' => out.push_str("%%"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A string value of the key file format: backslash is its escape character.
fn escape_value(value: &str) -> String {
    value.replace('\\', "\\\\")
}

/// The `Exec=` line for `program` followed by plain `args`.
fn exec_line(program: &str, args: &str) -> String {
    let mut line = format!("Exec={}", escape_value(&quote_exec_arg(program)));
    if !args.is_empty() {
        line.push(' ');
        line.push_str(args);
    }
    line
}

/// The template with every `Exec` line running `program`.
fn render_entry(template: &str, program: &str) -> String {
    let mut out = String::new();
    for line in template.lines() {
        match line
            .strip_prefix("Exec=")
            .and_then(|rest| rest.strip_prefix(TEMPLATE_PROGRAM))
        {
            Some(args) => out.push_str(&exec_line(program, args.trim_start())),
            None => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

/// Every file the install writes, relative to the data directory, with its content.
fn files(entry: String) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = vec![(
        Path::new("applications").join(ENTRY_FILE),
        entry.into_bytes(),
    )];
    for (size, bytes) in ICONS {
        files.push((icon_path(size, ICON_FILE), bytes.to_vec()));
    }
    files.push((icon_path("scalable", SVG_FILE), SVG.to_vec()));
    files
}

fn icon_path(size: &str, file: &str) -> PathBuf {
    Path::new("icons/hicolor")
        .join(size)
        .join("apps")
        .join(file)
}

fn io_err(op: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> DesktopError {
    let path = path.to_owned();
    move |source| DesktopError::Io { op, path, source }
}

/// Writes the entry and the icons under the data directory; returns it.
pub fn install() -> Result<PathBuf, DesktopError> {
    let root = data_home(std::env::var_os(DATA_HOME_ENV), std::env::var_os(HOME_ENV))?;
    let exe = std::env::current_exe().map_err(DesktopError::CurrentExe)?;
    let program = exe
        .to_str()
        .filter(|path| !path.chars().any(char::is_control))
        .ok_or_else(|| DesktopError::UnusableExecPath(exe.clone()))?;
    let entry = render_entry(ENTRY_TEMPLATE, program);
    for (relative, bytes) in files(entry) {
        let target = root.join(&relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(io_err("create", parent))?;
        }
        write_atomic(&target, &bytes)?;
    }
    refresh_caches(&root);
    Ok(root)
}

/// Writes through a temporary sibling so a launcher never reads a partial file.
fn write_atomic(target: &Path, bytes: &[u8]) -> Result<(), DesktopError> {
    let mut temp = target.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    std::fs::write(&temp, bytes).map_err(io_err("write", &temp))?;
    std::fs::rename(&temp, target).map_err(|source| {
        let _ = std::fs::remove_file(&temp);
        DesktopError::Io {
            op: "rename into place",
            path: target.to_owned(),
            source,
        }
    })
}

/// Removes exactly the files `install` writes, then any directory that is
/// left empty and the icon and desktop caches that only they kept alive.
pub fn uninstall() -> Result<PathBuf, DesktopError> {
    let root = data_home(std::env::var_os(DATA_HOME_ENV), std::env::var_os(HOME_ENV))?;
    for (relative, _) in files(String::new()) {
        let target = root.join(&relative);
        match std::fs::remove_file(&target) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(io_err("remove", &target)(err)),
        }
    }
    let applications = root.join("applications");
    let icons = root.join("icons");
    let hicolor = icons.join("hicolor");
    prune_empty_children(&hicolor);
    remove_cache_only_dir(&hicolor, "icon-theme.cache");
    remove_cache_only_dir(&applications, "mimeinfo.cache");
    for dir in [&hicolor, &icons, &applications] {
        let _ = std::fs::remove_dir(dir);
    }
    refresh_caches(&root);
    Ok(root)
}

/// Deletes `cache` when it is the only entry left, so a removed install
/// leaves no directory behind.
fn remove_cache_only_dir(dir: &Path, cache: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let names: Vec<_> = entries.flatten().map(|entry| entry.file_name()).collect();
    if names.len() == 1 && names[0] == cache {
        let _ = std::fs::remove_file(dir.join(cache));
    }
}

/// Removes the subdirectories of `dir` that hold nothing, recursively. A
/// directory with any file stays.
fn prune_empty_children(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            prune_empty_children(&entry.path());
            let _ = std::fs::remove_dir(entry.path());
        }
    }
}

/// Runs `update-desktop-database` on the applications directory and
/// `gtk-update-icon-cache` on the hicolor directory, when the tools are
/// installed. The icon cache is only built where an `index.theme` exists: a
/// user hicolor directory has none, the tool refuses it, and GTK reads
/// uncached icons. Launchers read the entry and icons without either tool, so
/// a missing tool or a failing run is reported on stderr and never fails the
/// command.
fn refresh_caches(root: &Path) {
    let applications = root.join("applications");
    let hicolor = root.join("icons/hicolor");
    if applications.is_dir() {
        run_tool("update-desktop-database", &[applications.as_os_str()]);
    }
    if hicolor.join("index.theme").is_file() {
        run_tool(
            "gtk-update-icon-cache",
            &["--force".as_ref(), hicolor.as_os_str()],
        );
    }
}

fn run_tool(tool: &str, args: &[&std::ffi::OsStr]) {
    let status = Command::new(tool)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("{tool} exited with {status}; the entry still works without it"),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => eprintln!("cannot run {tool}: {err}"),
    }
}

/// The paths `install` writes and `uninstall` removes, one per line.
pub fn summary(root: &Path) -> String {
    let mut out = String::new();
    for (relative, _) in files(String::new()) {
        let _ = writeln!(out, "{}", root.join(relative).display());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec_lines(entry: &str) -> Vec<&str> {
        entry.lines().filter(|l| l.starts_with("Exec=")).collect()
    }

    #[test]
    fn plain_path_is_not_quoted() {
        let entry = render_entry(ENTRY_TEMPLATE, "/home/o/bin/pohunek-companion");
        assert_eq!(
            exec_lines(&entry),
            [
                "Exec=/home/o/bin/pohunek-companion",
                "Exec=/home/o/bin/pohunek-companion new-session",
                "Exec=/home/o/bin/pohunek-companion assistant"
            ]
        );
    }

    #[test]
    fn path_with_spaces_and_specials_is_quoted_and_escaped() {
        let entry = render_entry(
            ENTRY_TEMPLATE,
            "/opt/My Apps/50%/a$b\"c\\d/pohunek-companion",
        );
        assert_eq!(
            exec_lines(&entry)[1],
            r#"Exec="/opt/My Apps/50%%/a\\$b\\"c\\\\d/pohunek-companion" new-session"#
        );
    }

    #[test]
    fn entry_keeps_the_required_keys() {
        let entry = render_entry(ENTRY_TEMPLATE, "/bin/pohunek-companion");
        for key in [
            "Name=Pohunek Companion",
            "Icon=pohunek-companion",
            "Terminal=false",
            "Categories=Development;",
            "StartupWMClass=pohunek-companion",
            "Actions=new-session;assistant;",
            "[Desktop Action new-session]",
            "[Desktop Action assistant]",
        ] {
            assert!(entry.lines().any(|l| l == key), "missing {key}");
        }
    }

    #[test]
    fn data_home_prefers_xdg_and_rejects_relative() {
        let abs = data_home(Some("/x/share".into()), Some("/home/o".into()));
        assert_eq!(abs.unwrap(), PathBuf::from("/x/share"));
        let home = data_home(None, Some("/home/o".into()));
        assert_eq!(home.unwrap(), PathBuf::from("/home/o/.local/share"));
        assert!(matches!(
            data_home(Some("rel".into()), None),
            Err(DesktopError::RelativeDataHome(_))
        ));
        assert!(matches!(
            data_home(None, None),
            Err(DesktopError::NoDataDir)
        ));
    }
}
