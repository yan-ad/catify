use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[cfg(windows)]
use std::process::Command;

use cfy_core::{Error, ErrorKind, Result};

use super::{NativeCommand, current_directory};

const ALIAS_NAME: &str = "h2";

pub(crate) fn parse_shortcut(args: &[String]) -> Result<NativeCommand> {
    if args.len() > 2 {
        return Err(Error::invalid_input(
            "hydrogen shortcut does not accept positional arguments",
        ));
    }
    if let Some(value) = args.get(1) {
        match value.as_str() {
            "--help" | "-h" => return Ok(NativeCommand::ShortcutHelp),
            value if value.starts_with('-') => {
                return Err(Error::invalid_input(format!(
                    "unexpected argument for hydrogen shortcut: {value}"
                )));
            }
            value => {
                return Err(Error::invalid_input(format!(
                    "unexpected argument for hydrogen shortcut: {value}"
                )));
            }
        }
    }
    Ok(NativeCommand::Shortcut)
}

pub(crate) fn print_shortcut_help() {
    println!(
        "Creates a global `{ALIAS_NAME}` shortcut for the Hydrogen CLI.\n\nUsage: cfy hydrogen shortcut\n\nOptions:\n  -h, --help  Print help"
    );
}

fn is_windows() -> bool {
    cfg!(windows)
}

fn is_git_bash() -> bool {
    env::var_os("MINGW_PREFIX").is_some()
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn resolve_from_home(path: &str, home: &Path) -> PathBuf {
    path.strip_prefix("~/")
        .map(|rest| home.join(rest))
        .unwrap_or_else(|| PathBuf::from(path))
}

fn which(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn supports_shell(shell: &str) -> bool {
    which(shell).is_some()
}

fn shell_alias_file(shell: &str) -> &'static str {
    match shell {
        "bash" => "~/.bashrc",
        "zsh" => "~/.zshrc",
        _ => "~/.config/fish/functions/h2.fish",
    }
}

fn has_alias_definition(shell: &str, home: &Path) -> bool {
    let file = resolve_from_home(shell_alias_file(shell), home);
    if shell == "fish" {
        return file.is_file();
    }
    let Ok(contents) = fs::read_to_string(&file) else {
        return false;
    };
    contents.lines().any(|line| {
        line.trim_start()
            .starts_with(&format!("alias {ALIAS_NAME}"))
    })
}

fn shell_write_alias(shell: &str, home: &Path, alias_line: &str) -> bool {
    if !supports_shell(shell) {
        return false;
    }
    if has_alias_definition(shell, home) {
        return true;
    }

    let file = resolve_from_home(shell_alias_file(shell), home);
    if shell == "fish" {
        if let Some(parent) = file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        return fs::write(&file, alias_line).is_ok();
    }

    if let Some(parent) = file.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut contents = fs::read_to_string(&file).unwrap_or_default();
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str(alias_line);
    if !contents.ends_with('\n') {
        contents.push('\n');
    }
    fs::write(&file, contents).is_ok()
}

fn current_hydrogen_command() -> String {
    env::current_exe()
        .ok()
        .and_then(|path| fs::canonicalize(path).ok())
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "cfy".to_owned())
}

fn bash_zsh_alias(command: &str) -> String {
    format!(
        "\n # Shopify Hydrogen alias to local projects\n alias {ALIAS_NAME}='{command} hydrogen'\n"
    )
}

fn fish_function(command: &str) -> String {
    format!(
        "\n function {ALIAS_NAME} --wraps='{command} hydrogen' --description 'Shortcut for the Hydrogen CLI'\n   {command} hydrogen $argv\n end\n"
    )
}

fn create_shortcuts_for_unix() -> Vec<String> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let command = current_hydrogen_command();
    let mut shells = Vec::new();
    for (shell, alias) in [
        ("zsh", bash_zsh_alias(&command)),
        ("bash", bash_zsh_alias(&command)),
        ("fish", fish_function(&command)),
    ] {
        if shell_write_alias(shell, &home, &alias) {
            shells.push(shell.to_owned());
        }
    }
    shells
}

#[cfg(windows)]
fn create_shortcuts_for_windows() -> Vec<String> {
    let mut shells = Vec::new();
    for executable in ["powershell.exe", "pwsh.exe"] {
        let profile_command = format!(
            r#"if (!(Test-Path -Path $PROFILE)) {{ New-Item -ItemType File -Path $PROFILE -Force }}
$profileContent = Get-Content -Path $PROFILE
if (!$profileContent -or $profileContent -NotLike '*Invoke-Local-H2*') {{ Add-Content -Path $PROFILE -Value 'function Invoke-Local-H2 {{ $h2 = "{cmd}"; Invoke-Expression "$h2 hydrogen $Args" }}; Set-Alias -Name {name} -Value Invoke-Local-H2' }}
"#,
            cmd = current_hydrogen_command(),
            name = ALIAS_NAME,
        );
        let status = Command::new(executable)
            .args(["-NoProfile", "-Command", &profile_command])
            .status();
        if status.is_ok_and(|status| status.success()) {
            shells.push(if executable == "pwsh.exe" {
                "PowerShell 7+".to_owned()
            } else {
                "PowerShell".to_owned()
            });
        }
    }
    shells
}

#[cfg(not(windows))]
fn create_shortcuts_for_windows() -> Vec<String> {
    Vec::new()
}

pub(crate) fn run_create_shortcut() -> Result<()> {
    let _ = current_directory()?;
    let shortcuts = if is_windows() && !is_git_bash() {
        create_shortcuts_for_windows()
    } else {
        create_shortcuts_for_unix()
    };

    if shortcuts.is_empty() {
        eprintln!("No supported shell found.");
        return Err(Error::with_source(
            ErrorKind::Process,
            "Please create a shortcut manually.",
            std::io::Error::new(std::io::ErrorKind::NotFound, "no supported shell"),
        ));
    }

    println!(
        "Shortcut ready for the following shells: {}.\nRestart your terminal session and run `{ALIAS_NAME}` from your local project.",
        shortcuts.join(", ")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shortcut_flags() {
        assert_eq!(
            parse_shortcut(&["shortcut".into()]).unwrap(),
            NativeCommand::Shortcut
        );
        assert_eq!(
            parse_shortcut(&["shortcut".into(), "--help".into()]).unwrap(),
            NativeCommand::ShortcutHelp
        );
        assert!(parse_shortcut(&["shortcut".into(), "--bogus".into()]).is_err());
    }

    #[test]
    fn alias_lines_quote_the_command() {
        let bash = bash_zsh_alias("/usr/local/bin/cfy");
        assert!(bash.contains("alias h2='/usr/local/bin/cfy hydrogen'"));
        let fish = fish_function("/usr/local/bin/cfy");
        assert!(fish.contains("/usr/local/bin/cfy hydrogen $argv"));
    }

    #[test]
    fn detects_existing_alias_definition() {
        let home = env::temp_dir().join(format!(
            "cfy-shortcut-home-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join(".zshrc"), "# comment\nalias h2='cfy hydrogen'\n").unwrap();
        assert!(has_alias_definition("zsh", &home));
        assert!(!has_alias_definition("bash", &home));
        fs::remove_dir_all(home).unwrap();
    }
}
