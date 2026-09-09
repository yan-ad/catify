use std::{
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};

use cfy_core::{Error, ErrorKind, Result};
use cfy_process::{OutputMode, ProcessOutput, ProcessSpec, Supervisor};
use serde_json::{Map, Value};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum HydrogenError {
    #[error(
        "Hydrogen tooling is not installed; install @shopify/cli-hydrogen or set CFY_HYDROGEN_BIN"
    )]
    NotInstalled,
    #[error("Hydrogen executable path is invalid: {0}")]
    InvalidExecutable(String),
    #[error("Hydrogen command failed: {0}")]
    Process(String),
}

impl From<HydrogenError> for Error {
    fn from(error: HydrogenError) -> Self {
        Error::with_source(ErrorKind::Process, "Hydrogen adapter failed", error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HydrogenTool {
    pub executable: PathBuf,
    pub version: Option<String>,
}

impl HydrogenTool {
    pub fn discover() -> std::result::Result<Self, HydrogenError> {
        if let Some(path) = env::var_os("CFY_HYDROGEN_BIN") {
            let path = PathBuf::from(path);
            if path.as_os_str().is_empty() {
                return Err(HydrogenError::InvalidExecutable("empty path".into()));
            }
            return Ok(Self {
                executable: path,
                version: None,
            });
        }
        for candidate in ["shopify", "npx"] {
            if which(candidate).is_some() {
                return Ok(Self {
                    executable: PathBuf::from(candidate),
                    version: None,
                });
            }
        }
        Err(HydrogenError::NotInstalled)
    }

    pub fn command_args(&self, args: &[String]) -> Vec<String> {
        if self.executable.file_name().and_then(|x| x.to_str()) == Some("npx") {
            let mut command = vec!["--no-install".into(), "shopify".into(), "hydrogen".into()];
            command.extend(args.iter().cloned());
            command
        } else if self.executable.file_name().and_then(|x| x.to_str()) == Some("shopify") {
            let mut command = vec!["hydrogen".into()];
            command.extend(args.iter().cloned());
            command
        } else {
            args.to_vec()
        }
    }

    pub async fn run(&self, args: &[String], supervisor: &Supervisor) -> Result<ProcessOutput> {
        let process = supervisor.spawn(
            ProcessSpec::new(self.executable.to_string_lossy())
                .args(self.command_args(args))
                .output(OutputMode::CaptureAndStream),
        )?;
        process.wait().await
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct UnlinkOptions {
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NativeCommand {
    Unlink(UnlinkOptions),
    UnlinkHelp,
}

fn parse_native_command(args: &[String]) -> Result<Option<NativeCommand>> {
    if args.first().map(String::as_str) != Some("unlink") {
        return Ok(None);
    }

    let mut path = env::var_os("SHOPIFY_HYDROGEN_FLAG_PATH").map(PathBuf::from);
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--path" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| Error::invalid_input("--path requires a value"))?;
                path = Some(PathBuf::from(value));
            }
            value if value.starts_with("--path=") => {
                path = Some(PathBuf::from(&value["--path=".len()..]));
            }
            "--help" | "-h" => return Ok(Some(NativeCommand::UnlinkHelp)),
            value => {
                return Err(Error::invalid_input(format!(
                    "unexpected argument for hydrogen unlink: {value}"
                )));
            }
        }
        index += 1;
    }

    Ok(Some(NativeCommand::Unlink(UnlinkOptions {
        path: path.unwrap_or(env::current_dir().map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                "could not resolve current directory",
                error,
            )
        })?),
    })))
}

fn print_unlink_help() {
    println!(
        "Unlink a local project from a Hydrogen storefront.\n\nUsage: cfy hydrogen unlink [OPTIONS]\n\nOptions:\n      --path <PATH>  Path to the storefront project [env: SHOPIFY_HYDROGEN_FLAG_PATH=]\n  -h, --help         Print help"
    );
}

fn unlink_storefront(root: &Path) -> Result<()> {
    let config_path = root.join(".shopify").join("project.json");
    if !config_path.is_file() {
        eprintln!("Warning: This project isn't linked to a Hydrogen storefront.");
        return Ok(());
    }

    let contents = fs::read_to_string(&config_path).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            format!("could not read {}", config_path.display()),
            error,
        )
    })?;
    let mut config: Map<String, Value> = serde_json::from_str(&contents).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            format!("could not parse {}", config_path.display()),
            error,
        )
    })?;

    let storefront_is_linked = config
        .get("storefront")
        .and_then(Value::as_object)
        .is_some_and(|storefront| !storefront.is_empty());
    if !storefront_is_linked {
        eprintln!("Warning: This project isn't linked to a Hydrogen storefront.");
        return Ok(());
    }
    let storefront = config.remove("storefront").expect("checked above");
    let title = storefront
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("the Hydrogen storefront")
        .to_owned();

    let serialized = serde_json::to_string(&config).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            "could not serialize Hydrogen project configuration",
            error,
        )
    })?;
    let permissions = fs::metadata(&config_path).map(|metadata| metadata.permissions());
    cfy_config::write_atomic(&config_path, serialized.as_bytes()).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            format!("could not replace {}", config_path.display()),
            error,
        )
    })?;
    if let Ok(permissions) = permissions {
        fs::set_permissions(&config_path, permissions).map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                format!("could not restore permissions on {}", config_path.display()),
                error,
            )
        })?;
    }
    ensure_shopify_gitignore(root);

    println!("You are no longer linked to {title}.");
    Ok(())
}

fn ensure_shopify_gitignore(root: &Path) {
    let path = root.join(".gitignore");
    let Ok(mut contents) = fs::read_to_string(&path).or_else(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Ok(String::new())
        } else {
            Err(error)
        }
    }) else {
        return;
    };
    if contents.lines().any(|line| line.trim() == ".shopify") {
        return;
    }
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str(".shopify\n");
    let _ = fs::write(path, contents);
}

pub async fn run(args: &[String]) -> Result<i32> {
    match parse_native_command(args)? {
        Some(NativeCommand::Unlink(options)) => {
            unlink_storefront(&options.path)?;
            return Ok(0);
        }
        Some(NativeCommand::UnlinkHelp) => {
            print_unlink_help();
            return Ok(0);
        }
        None => {}
    }

    let tool = HydrogenTool::discover()?;
    let supervisor = Supervisor::new(Duration::from_secs(2));
    let output = tool.run(args, &supervisor).await?;
    Ok(output.exit_code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn fixture(name: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "cfy-hydrogen-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn maps_shopify_and_npx_invocations() {
        let shopify = HydrogenTool {
            executable: PathBuf::from("shopify"),
            version: None,
        };
        assert_eq!(
            shopify.command_args(&["build".into()]),
            vec!["hydrogen", "build"]
        );
        let npx = HydrogenTool {
            executable: PathBuf::from("npx"),
            version: None,
        };
        assert_eq!(
            npx.command_args(&["dev".into()]),
            vec!["--no-install", "shopify", "hydrogen", "dev"]
        );
    }

    #[test]
    fn parses_unlink_path_forms() {
        assert_eq!(
            parse_native_command(&["unlink".into(), "--path".into(), "storefront".into()]).unwrap(),
            Some(NativeCommand::Unlink(UnlinkOptions {
                path: PathBuf::from("storefront")
            }))
        );
        assert!(matches!(
            parse_native_command(&["unlink".into(), "--path=storefront".into()]).unwrap(),
            Some(NativeCommand::Unlink(UnlinkOptions { path })) if path == Path::new("storefront")
        ));
        assert!(matches!(
            parse_native_command(&["unlink".into(), "--help".into()]).unwrap(),
            Some(NativeCommand::UnlinkHelp)
        ));
        assert!(parse_native_command(&["dev".into()]).unwrap().is_none());
    }

    #[test]
    fn unlinks_storefront_without_losing_account_config() {
        let root = fixture("unlink");
        fs::create_dir_all(root.join(".shopify")).unwrap();
        fs::write(
            root.join(".shopify/project.json"),
            r#"{"shop":"example.myshopify.com","shopName":"Example","email":"owner@example.com","storefront":{"id":"gid://shopify/HydrogenStorefront/1","title":"Hydrogen Test"}}"#,
        )
        .unwrap();

        unlink_storefront(&root).unwrap();

        let config: Value =
            serde_json::from_str(&fs::read_to_string(root.join(".shopify/project.json")).unwrap())
                .unwrap();
        assert_eq!(config["shop"], "example.myshopify.com");
        assert!(config.get("storefront").is_none());
        assert!(
            fs::read_to_string(root.join(".gitignore"))
                .unwrap()
                .contains(".shopify")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unlink_is_idempotent_when_project_is_not_linked() {
        let root = fixture("unlinked");
        fs::create_dir_all(&root).unwrap();
        unlink_storefront(&root).unwrap();
        assert!(!root.join(".shopify/project.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn null_storefront_is_left_unchanged() {
        let root = fixture("null-storefront");
        fs::create_dir_all(root.join(".shopify")).unwrap();
        let original = r#"{"shop":"example.myshopify.com","storefront":null}"#;
        fs::write(root.join(".shopify/project.json"), original).unwrap();
        unlink_storefront(&root).unwrap();
        assert_eq!(
            fs::read_to_string(root.join(".shopify/project.json")).unwrap(),
            original
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unlink_preserves_project_config_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = fixture("permissions");
        fs::create_dir_all(root.join(".shopify")).unwrap();
        let path = root.join(".shopify/project.json");
        fs::write(&path, r#"{"storefront":{"title":"Private"}}"#).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        unlink_storefront(&root).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(root).unwrap();
    }
}
