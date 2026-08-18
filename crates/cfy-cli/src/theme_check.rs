use cfy_core::{Error, ErrorKind, Result};
use cfy_process::{OutputMode, ProcessOutput, ProcessSpec, Supervisor};
use clap::Args;
use std::{
    env,
    ffi::OsStr,
    io::{self, Write},
    path::{Path, PathBuf},
};

pub const ADAPTER_ENV: &str = "CFY_THEME_CHECK_BIN";

#[derive(Debug, Args)]
pub struct ThemeCheckArgs {
    /// Theme directory to check.
    #[arg(long, value_name = "PATH")]
    pub path: Option<PathBuf>,
    /// Config file or upstream config name.
    #[arg(short = 'C', long, value_name = "CONFIG")]
    pub config: Option<String>,
    /// Only run this category (repeatable; supported by legacy engines).
    #[arg(short = 'c', long, action = clap::ArgAction::Append)]
    pub category: Vec<String>,
    /// Exclude this category (repeatable; supported by legacy engines).
    #[arg(short = 'x', long, visible_alias = "exclude-category", action = clap::ArgAction::Append)]
    pub exclude: Vec<String>,
    /// Automatically fix offenses.
    #[arg(short = 'a', long)]
    pub auto_correct: bool,
    /// Generate a .theme-check.yml file.
    #[arg(long, conflicts_with_all = ["list", "auto_correct"])]
    pub init: bool,
    /// List enabled checks.
    #[arg(long, conflicts_with = "init")]
    pub list: bool,
    /// Output format supported by the selected engine.
    #[arg(short = 'o', long, value_name = "FORMAT")]
    pub output: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdapterKind {
    ShopifyCli,
    ThemeCheck,
}

#[derive(Debug)]
struct Adapter {
    program: String,
    kind: AdapterKind,
    probe_version: bool,
}

impl Adapter {
    fn resolve() -> Result<Self> {
        let override_program = env::var(ADAPTER_ENV).ok();
        let program = override_program
            .clone()
            .unwrap_or_else(|| "shopify".to_owned());
        if program.trim().is_empty() {
            return Err(Error::config(format!("{ADAPTER_ENV} must not be empty")));
        }
        if is_cfy_executable(Path::new(&program)) {
            return Err(Error::config(format!(
                "{ADAPTER_ENV} points to cfy, which would recursively invoke itself; point it to the official `shopify` or `theme-check` executable"
            )));
        }
        let stem = Path::new(&program)
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or_default();
        let kind = if stem.eq_ignore_ascii_case("theme-check")
            || stem.eq_ignore_ascii_case("theme_check")
        {
            AdapterKind::ThemeCheck
        } else {
            AdapterKind::ShopifyCli
        };
        Ok(Self {
            program,
            kind,
            probe_version: override_program.is_some(),
        })
    }

    fn version_spec(&self) -> ProcessSpec {
        ProcessSpec::new(&self.program)
            .args(match self.kind {
                AdapterKind::ShopifyCli => vec!["version"],
                AdapterKind::ThemeCheck => vec!["--version"],
            })
            .output(OutputMode::Capture)
    }

    fn check_spec(&self, args: &ThemeCheckArgs) -> ProcessSpec {
        let mut child_args = Vec::new();
        if self.kind == AdapterKind::ShopifyCli {
            child_args.extend(["theme".to_owned(), "check".to_owned()]);
        }
        if let Some(path) = &args.path {
            if self.kind == AdapterKind::ShopifyCli {
                child_args.push("--path".to_owned());
            }
            child_args.push(path.to_string_lossy().into_owned());
        }
        if let Some(config) = &args.config {
            child_args.extend(["--config".to_owned(), config.clone()]);
        }
        for value in &args.category {
            child_args.extend(["--category".to_owned(), value.clone()]);
        }
        for value in &args.exclude {
            child_args.extend(["--exclude-category".to_owned(), value.clone()]);
        }
        if args.auto_correct {
            child_args.push("--auto-correct".to_owned());
        }
        if args.init {
            child_args.push("--init".to_owned());
        }
        if args.list {
            child_args.push("--list".to_owned());
        }
        if let Some(value) = &args.output {
            child_args.extend(["--output".to_owned(), value.clone()]);
        }
        ProcessSpec::new(&self.program)
            .args(child_args)
            .output(OutputMode::Capture)
    }
}

fn is_cfy_executable(path: &Path) -> bool {
    if path
        .file_stem()
        .and_then(OsStr::to_str)
        .is_some_and(|stem| stem.eq_ignore_ascii_case("cfy"))
    {
        return true;
    }
    let Ok(candidate) = path.canonicalize() else {
        return false;
    };
    env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok())
        .is_some_and(|current| current == candidate)
}

async fn execute(
    supervisor: &Supervisor,
    spec: ProcessSpec,
    program: &str,
) -> Result<ProcessOutput> {
    supervisor
        .spawn(spec)
        .map_err(|source| Error::process(format!(
            "could not start Theme Check adapter `{program}`: {source}. Install Shopify CLI (`npm install -g @shopify/cli @shopify/theme`) or set {ADAPTER_ENV} to an official executable"
        )))?
        .wait_with_signal_forwarding()
        .await
        .map_err(|source| Error::process(format!("Theme Check adapter `{program}` failed: {source}")))
}

fn validate_version(adapter: &Adapter, output: &ProcessOutput) -> Result<()> {
    let bytes = if output.stdout.is_empty() {
        &output.stderr
    } else {
        &output.stdout
    };
    let text = String::from_utf8_lossy(bytes);
    let major = text
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .find(|part| part.contains('.'))
        .and_then(|version| version.split('.').next())
        .and_then(|major| major.parse::<u64>().ok());
    let supported = output.status.success()
        && match adapter.kind {
            AdapterKind::ShopifyCli => major.is_some_and(|major| major >= 3),
            AdapterKind::ThemeCheck => major.is_some_and(|major| major >= 1),
        };
    if supported {
        Ok(())
    } else {
        Err(Error::process(format!(
            "unsupported Theme Check adapter version from `{}`: `{}`; cfy supports Shopify CLI 3.x+ or Theme Check 1.x+ (override with {ADAPTER_ENV})",
            adapter.program,
            text.trim()
        )))
    }
}

pub async fn run(args: &ThemeCheckArgs) -> Result<u8> {
    let adapter = Adapter::resolve()?;
    let supervisor = Supervisor::default();
    if adapter.probe_version {
        validate_version(
            &adapter,
            &execute(&supervisor, adapter.version_spec(), &adapter.program).await?,
        )?;
    }
    let output = execute(&supervisor, adapter.check_spec(args), &adapter.program).await?;
    io::stdout().write_all(&output.stdout).map_err(|source| {
        Error::with_source(
            ErrorKind::Process,
            "could not write Theme Check stdout",
            source,
        )
    })?;
    io::stderr().write_all(&output.stderr).map_err(|source| {
        Error::with_source(
            ErrorKind::Process,
            "could not write Theme Check stderr",
            source,
        )
    })?;
    Ok(output
        .status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn success_status() -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(0)
    }

    #[cfg(windows)]
    fn success_status() -> std::process::ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(0)
    }

    fn args() -> ThemeCheckArgs {
        ThemeCheckArgs {
            path: Some(PathBuf::from("fixtures/theme")),
            config: Some(".theme-check.yml".into()),
            category: vec!["liquid".into()],
            exclude: vec!["performance".into()],
            auto_correct: true,
            init: false,
            list: false,
            output: Some("json".into()),
        }
    }

    #[test]
    fn shopify_spec_preserves_flags() {
        let spec = Adapter {
            program: "shopify".into(),
            kind: AdapterKind::ShopifyCli,
            probe_version: false,
        }
        .check_spec(&args());
        assert_eq!(
            spec.args,
            [
                "theme",
                "check",
                "--path",
                "fixtures/theme",
                "--config",
                ".theme-check.yml",
                "--category",
                "liquid",
                "--exclude-category",
                "performance",
                "--auto-correct",
                "--output",
                "json"
            ]
        );
    }

    #[test]
    fn direct_engine_uses_positional_path() {
        let spec = Adapter {
            program: "theme-check".into(),
            kind: AdapterKind::ThemeCheck,
            probe_version: true,
        }
        .check_spec(&args());
        assert_eq!(
            spec.args.first().map(String::as_str),
            Some("fixtures/theme")
        );
        assert!(!spec.args.iter().any(|arg| arg == "--path"));
    }

    #[test]
    fn recursion_guard_is_cross_platform() {
        assert!(is_cfy_executable(Path::new("cfy")));
        assert!(is_cfy_executable(Path::new("cfy.exe")));
        assert!(!is_cfy_executable(Path::new("shopify")));
    }

    #[test]
    fn accepts_supported_shopify_cli_major_versions() {
        for version in ["3.90.0\n", "4.6.1\n"] {
            let adapter = Adapter {
                program: "shopify".into(),
                kind: AdapterKind::ShopifyCli,
                probe_version: true,
            };
            let output = ProcessOutput {
                status: success_status(),
                stdout: version.as_bytes().to_vec(),
                stderr: Vec::new(),
                cancelled: false,
            };
            assert!(validate_version(&adapter, &output).is_ok());
        }
    }
}
