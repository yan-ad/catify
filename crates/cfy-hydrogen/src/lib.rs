use std::{
    collections::{BTreeSet, HashSet},
    env, fs,
    io::{self, Cursor, IsTerminal, Read, Write},
    path::{Component, Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};

use cfy_core::{Error, ErrorKind, Result};
use cfy_process::{OutputMode, ProcessOutput, ProcessSpec, Supervisor};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use swc_common::{
    FileName, GLOBALS, Globals, Mark, SourceMap, comments::SingleThreadedComments, sync::Lrc,
};
use swc_ecma_codegen::to_code_default;
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};
use swc_ecma_transforms_base::{fixer::fixer, hygiene::hygiene, resolver};
use swc_ecma_transforms_typescript::strip;
use thiserror::Error;

mod setup;
mod shortcut;
mod vite;

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

pub(crate) fn transpile_typescript(contents: &[u8], path: &Path) -> Result<Vec<u8>> {
    let source = String::from_utf8(contents.to_vec()).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            format!("Hydrogen template {} is not UTF-8", path.display()),
            error,
        )
    })?;
    let source_map: Lrc<SourceMap> = Default::default();
    let filename = FileName::Custom(path.display().to_string());
    let file = source_map.new_source_file(filename.into(), source);
    let comments = SingleThreadedComments::default();
    let lexer = Lexer::new(
        Syntax::Typescript(TsSyntax {
            tsx: path.extension().and_then(|extension| extension.to_str()) == Some("tsx"),
            ..Default::default()
        }),
        Default::default(),
        StringInput::from(&*file),
        Some(&comments),
    );
    let mut parser = Parser::new_from(lexer);
    let program = parser.parse_program().map_err(|error| {
        Error::config(format!(
            "could not parse official Hydrogen template {}: {error:?}",
            path.display()
        ))
    })?;
    let parser_errors = parser.take_errors();
    if !parser_errors.is_empty() {
        return Err(Error::config(format!(
            "could not parse official Hydrogen template {}",
            path.display()
        )));
    }
    let globals = Globals::default();
    let code = GLOBALS.set(&globals, || {
        let unresolved_mark = Mark::new();
        let top_level_mark = Mark::new();
        let program = program.apply(resolver(unresolved_mark, top_level_mark, true));
        let program = program.apply(strip(unresolved_mark, top_level_mark));
        let program = program.apply(hygiene());
        let program = program.apply(fixer(Some(&comments)));
        to_code_default(source_map, Some(&comments), &program)
    });
    Ok(code.into_bytes())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TemplateSource {
    root: PathBuf,
}

impl TemplateSource {
    pub(crate) fn read(&self, path: &Path) -> Result<Vec<u8>> {
        fs::read(self.root.join(path)).map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                format!("the cached Hydrogen template is missing {}", path.display()),
                error,
            )
        })
    }

    pub(crate) fn has_file(&self, path: &Path) -> bool {
        self.root.join(path).is_file()
    }
}

fn hydrogen_cache_root() -> PathBuf {
    env::var_os("CFY_CACHE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("XDG_CACHE_HOME").map(|path| PathBuf::from(path).join("catify/hydrogen"))
        })
        .or_else(|| {
            env::var_os("HOME").map(|path| PathBuf::from(path).join(".cache/catify/hydrogen"))
        })
        .unwrap_or_else(|| PathBuf::from(".catify-cache/hydrogen"))
}

fn pinned_template_ref(root: &Path) -> String {
    let Ok(contents) = fs::read_to_string(root.join("package.json")) else {
        return PINNED_HYDROGEN_COMMIT.to_owned();
    };
    let Ok(package): std::result::Result<Value, _> = serde_json::from_str(&contents) else {
        return PINNED_HYDROGEN_COMMIT.to_owned();
    };
    let version = package
        .get("dependencies")
        .and_then(|dependencies| dependencies.get("@shopify/hydrogen"))
        .or_else(|| {
            package
                .get("devDependencies")
                .and_then(|dependencies| dependencies.get("@shopify/hydrogen"))
        })
        .and_then(Value::as_str)
        .and_then(exact_hydrogen_version);
    version
        .map(|version| format!("skeleton@{version}"))
        .unwrap_or_else(|| PINNED_HYDROGEN_COMMIT.to_owned())
}

fn exact_hydrogen_version(requirement: &str) -> Option<&str> {
    let version = requirement
        .trim()
        .trim_start_matches(['=', '~', '^'])
        .split_whitespace()
        .next()?;
    let segments = version.split('.').collect::<Vec<_>>();
    (segments.len() == 3
        && segments.iter().all(|segment| {
            !segment.is_empty() && segment.chars().all(|value| value.is_ascii_digit())
        }))
    .then_some(version)
}

fn template_ref_cache_key(reference: &str) -> String {
    let key = if reference.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'@' | b'~')
    }) {
        reference.to_owned()
    } else {
        format!("{:x}", Sha256::digest(reference.as_bytes()))
    };
    // v2 adds setup assets to the official archive cache. Namespacing avoids
    // treating route-only snapshots created by earlier Catify versions as a
    // complete source for native setup commands.
    format!("v2-{key}")
}

fn template_cache_is_fresh(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .and_then(|modified| modified.elapsed().map_err(io::Error::other))
        .is_ok_and(|age| age <= TEMPLATE_DISCOVERY_TTL)
}

fn remote_templates_enabled() -> bool {
    !env::var_os("CFY_HYDROGEN_OFFLINE").is_some_and(|value| {
        matches!(
            value.to_string_lossy().trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn template_reference(root: &Path) -> String {
    env::var("CFY_HYDROGEN_TEMPLATE_REF")
        .ok()
        .filter(|reference| !reference.trim().is_empty())
        .unwrap_or_else(|| pinned_template_ref(root))
}

pub(crate) fn resolve_template_source(root: &Path) -> Result<TemplateSource> {
    let reference = template_reference(root);
    let cache_root = hydrogen_cache_root().join("templates");
    let cache_key = template_ref_cache_key(&reference);
    let cache_directory = cache_root.join(&cache_key);
    let marker = cache_directory.join(".complete");
    if marker.is_file()
        && cache_directory.join("app/routes").is_dir()
        && template_cache_is_fresh(&marker)
    {
        return Ok(TemplateSource {
            root: cache_directory,
        });
    }

    if !remote_templates_enabled() {
        return Err(Error::process(
            "Hydrogen templates are not cached and network access is disabled (CFY_HYDROGEN_OFFLINE); run once online to populate the cache",
        ));
    }

    fetch_official_template(&reference, &cache_directory)?;
    Ok(TemplateSource {
        root: cache_directory,
    })
}

fn fetch_official_template(reference: &str, destination: &Path) -> Result<()> {
    let bytes = load_template_archive(reference)?;
    let staging_name = format!(
        ".{}-{}",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("template"),
        std::process::id()
    );
    let temporary = destination.with_file_name(staging_name);
    if temporary.exists() {
        fs::remove_dir_all(&temporary).map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                "could not clear template staging directory",
                error,
            )
        })?;
    }
    extract_template_archive(&bytes, &temporary)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                "could not create Hydrogen template cache",
                error,
            )
        })?;
    }
    if destination.exists() {
        fs::remove_dir_all(destination).map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                "could not replace Hydrogen template cache",
                error,
            )
        })?;
    }
    fs::rename(&temporary, destination).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            "could not install Hydrogen template cache",
            error,
        )
    })?;
    fs::write(destination.join(".complete"), format!("{reference}\n")).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            "could not finalize Hydrogen template cache",
            error,
        )
    })?;
    Ok(())
}

fn load_template_archive(reference: &str) -> Result<Vec<u8>> {
    if let Some(path) = reference.strip_prefix("file://") {
        let bytes = fs::read(path).map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                format!("could not read Hydrogen template archive {path}"),
                error,
            )
        })?;
        return validate_template_archive_size(bytes);
    }
    if Path::new(reference).is_file() {
        let bytes = fs::read(reference).map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                format!("could not read Hydrogen template archive {reference}"),
                error,
            )
        })?;
        return validate_template_archive_size(bytes);
    }

    let encoded_reference = percent_encode_path_segment(reference);
    let url = format!("https://codeload.github.com/Shopify/hydrogen/zip/{encoded_reference}");
    let _ = rustls::crypto::ring::default_provider().install_default();
    let response = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .user_agent(format!("Catify/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| Error::process(format!("could not create template client: {error}")))?
        .get(&url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|error| {
            Error::process(format!("could not download Hydrogen template: {error}"))
        })?;
    if response
        .content_length()
        .is_some_and(|size| size > MAX_TEMPLATE_ARCHIVE_BYTES as u64)
    {
        return Err(Error::process(
            "Hydrogen template archive is unexpectedly large",
        ));
    }
    let bytes = response
        .bytes()
        .map_err(|error| Error::process(format!("could not read Hydrogen template: {error}")))?;
    validate_template_archive_size(bytes.to_vec())
}

fn validate_template_archive_size(bytes: Vec<u8>) -> Result<Vec<u8>> {
    if bytes.len() > MAX_TEMPLATE_ARCHIVE_BYTES {
        return Err(Error::process(
            "Hydrogen template archive is unexpectedly large",
        ));
    }
    Ok(bytes)
}

fn percent_encode_path_segment(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn extract_template_archive(bytes: &[u8], destination: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|error| {
        Error::process(format!("could not open Hydrogen template archive: {error}"))
    })?;
    let mut extracted = 0_usize;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| {
            Error::process(format!(
                "could not inspect Hydrogen template archive: {error}"
            ))
        })?;
        if !entry.is_file() {
            continue;
        }
        let Some(path) = entry.enclosed_name() else {
            return Err(Error::process(
                "Hydrogen template archive contains an unsafe path",
            ));
        };

        let target = if let Some(relative) = setup_asset_relative(&path) {
            destination.join("assets").join(relative)
        } else if path.ends_with(UPSTREAM_LOCALE_CHECK_PATH) {
            destination.join("locale-check.ts")
        } else {
            let components = path.components().collect::<Vec<_>>();
            let upstream = Path::new(UPSTREAM_TEMPLATE_PREFIX)
                .components()
                .collect::<Vec<_>>();
            let Some(start) = components
                .windows(upstream.len())
                .position(|window| window == upstream.as_slice())
            else {
                continue;
            };
            let relative = components[start + upstream.len()..].iter().fold(
                PathBuf::new(),
                |mut output, component| {
                    output.push(component.as_os_str());
                    output
                },
            );
            if relative.as_os_str().is_empty() {
                continue;
            }
            destination.join("app").join(relative)
        };

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                Error::with_source(
                    ErrorKind::Config,
                    "could not extract Hydrogen template",
                    error,
                )
            })?;
        }
        let mut contents = Vec::new();
        entry.read_to_end(&mut contents).map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                "could not read Hydrogen template entry",
                error,
            )
        })?;
        extracted = extracted.saturating_add(contents.len());
        if extracted > MAX_TEMPLATE_ARCHIVE_BYTES {
            return Err(Error::process(
                "Hydrogen template expands beyond the safety limit",
            ));
        }
        fs::write(target, contents).map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                "could not extract Hydrogen template",
                error,
            )
        })?;
    }
    if !destination.join("app/routes").is_dir() || !destination.join("locale-check.ts").is_file() {
        return Err(Error::process(
            "Hydrogen template archive is missing the skeleton routes or locale template",
        ));
    }
    Ok(())
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

const PINNED_HYDROGEN_COMMIT: &str = "2a2738ba20487ccc07006815fe40e93b24cb5f08";
const UPSTREAM_TEMPLATE_PREFIX: &str = "templates/skeleton/app/";
const UPSTREAM_LOCALE_CHECK_PATH: &str = "packages/cli/assets/routes/locale-check.ts";
const MAX_TEMPLATE_ARCHIVE_BYTES: usize = 32 * 1024 * 1024;
const TEMPLATE_DISCOVERY_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Official `packages/cli/assets` files that the native setup commands
/// synchronize from the immutable Hydrogen archive. The tuple maps the
/// upstream path suffix to the relative path inside the runtime cache.
const SETUP_ASSET_FILES: &[(&str, &str)] = &[
    (
        "packages/cli/assets/vite/vite.config.js",
        "vite/vite.config.js",
    ),
    ("packages/cli/assets/vite/package.json", "vite/package.json"),
    (
        "packages/cli/assets/i18n/subfolders.ts",
        "i18n/subfolders.ts",
    ),
    ("packages/cli/assets/i18n/domains.ts", "i18n/domains.ts"),
    (
        "packages/cli/assets/i18n/subdomains.ts",
        "i18n/subdomains.ts",
    ),
    (
        "packages/cli/assets/i18n/mock-i18n-types.ts",
        "i18n/mock-i18n-types.ts",
    ),
    (
        "packages/cli/assets/tailwind/tailwind.css",
        "tailwind/tailwind.css",
    ),
    (
        "packages/cli/assets/tailwind/package.json",
        "tailwind/package.json",
    ),
    (
        "packages/cli/assets/vanilla-extract/package.json",
        "vanilla-extract/package.json",
    ),
];

fn setup_asset_relative(path: &Path) -> Option<&'static str> {
    SETUP_ASSET_FILES
        .iter()
        .find(|(upstream, _)| path.ends_with(upstream))
        .map(|(_, relative)| *relative)
}

const ALL_ROUTE_CHOICES: &[&str] = &[
    "home",
    "page",
    "cart",
    "products",
    "collections",
    "policies",
    "blogs",
    "account",
    "search",
    "robots",
    "sitemap",
    "all",
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct UnlinkOptions {
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GenerateRouteOptions {
    route_name: String,
    path: PathBuf,
    adapter: Option<String>,
    typescript: Option<bool>,
    locale_param: Option<String>,
    force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NativeCommand {
    Unlink(UnlinkOptions),
    UnlinkHelp,
    GenerateRoute(GenerateRouteOptions),
    GenerateRouteHelp,
    GenerateRoutes(GenerateRouteOptions),
    GenerateRoutesHelp,
    SetupMarkets(setup::SetupMarketsOptions),
    SetupMarketsHelp,
    SetupCss(setup::SetupCssOptions),
    SetupCssHelp,
    SetupVite(vite::SetupViteOptions),
    SetupViteHelp,
    Shortcut,
    ShortcutHelp,
}

fn env_bool(name: &str) -> Result<Option<bool>> {
    let Some(value) = env::var_os(name) else {
        return Ok(None);
    };
    let value = value.to_string_lossy();
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(Some(true)),
        "0" | "false" | "no" | "off" | "" => Ok(Some(false)),
        _ => Err(Error::invalid_input(format!(
            "{name} must be true or false"
        ))),
    }
}

pub(crate) fn current_directory() -> Result<PathBuf> {
    env::current_dir().map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            "could not resolve current directory",
            error,
        )
    })
}

fn parse_native_command(args: &[String]) -> Result<Option<NativeCommand>> {
    match args.first().map(String::as_str) {
        Some("unlink") => parse_unlink_command(args).map(Some),
        Some("generate") if args.get(1).map(String::as_str) == Some("route") => {
            parse_generate_command(&args[2..], false).map(Some)
        }
        Some("generate") if args.get(1).map(String::as_str) == Some("routes") => {
            parse_generate_command(&args[2..], true).map(Some)
        }
        Some("setup") if args.get(1).map(String::as_str) == Some("markets") => {
            setup::parse_setup_markets(&args[2..]).map(Some)
        }
        Some("setup") if args.get(1).map(String::as_str) == Some("css") => {
            setup::parse_setup_css(&args[2..]).map(Some)
        }
        Some("setup") if args.get(1).map(String::as_str) == Some("vite") => {
            vite::parse_setup_vite(&args[2..]).map(Some)
        }
        Some("shortcut") => shortcut::parse_shortcut(args).map(Some),
        _ => Ok(None),
    }
}

fn parse_unlink_command(args: &[String]) -> Result<NativeCommand> {
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
            "--help" | "-h" => return Ok(NativeCommand::UnlinkHelp),
            value => {
                return Err(Error::invalid_input(format!(
                    "unexpected argument for hydrogen unlink: {value}"
                )));
            }
        }
        index += 1;
    }

    Ok(NativeCommand::Unlink(UnlinkOptions {
        path: path.unwrap_or(current_directory()?),
    }))
}

fn parse_generate_command(args: &[String], all: bool) -> Result<NativeCommand> {
    let mut route_name = if all {
        Some("all".to_owned())
    } else {
        env::var("SHOPIFY_HYDROGEN_ARG_ROUTE").ok()
    };
    let mut path = env::var_os("SHOPIFY_HYDROGEN_FLAG_PATH").map(PathBuf::from);
    let mut adapter = env::var("SHOPIFY_HYDROGEN_FLAG_ADAPTER").ok();
    // Shopify's current manifest accidentally shares this environment variable
    // between --adapter and --locale-param. Preserve that public contract.
    let mut locale_param = env::var("SHOPIFY_HYDROGEN_FLAG_ADAPTER").ok();
    let mut typescript = env_bool("SHOPIFY_HYDROGEN_FLAG_TYPESCRIPT")?;
    let mut force = env_bool("SHOPIFY_HYDROGEN_FLAG_FORCE")?.unwrap_or(false);
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => {
                return Ok(if all {
                    NativeCommand::GenerateRoutesHelp
                } else {
                    NativeCommand::GenerateRouteHelp
                });
            }
            "--path" | "--adapter" | "--locale-param" => {
                let flag = args[index].clone();
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| Error::invalid_input(format!("{flag} requires a value")))?;
                match flag.as_str() {
                    "--path" => path = Some(PathBuf::from(value)),
                    "--adapter" => adapter = Some(value.clone()),
                    "--locale-param" => locale_param = Some(value.clone()),
                    _ => unreachable!(),
                }
            }
            "--typescript" => typescript = Some(true),
            "-f" | "--force" => force = true,
            value if value.starts_with("--path=") => {
                path = Some(PathBuf::from(&value["--path=".len()..]));
            }
            value if value.starts_with("--adapter=") => {
                adapter = Some(value["--adapter=".len()..].to_owned());
            }
            value if value.starts_with("--locale-param=") => {
                locale_param = Some(value["--locale-param=".len()..].to_owned());
            }
            value if value.starts_with('-') => {
                return Err(Error::invalid_input(format!(
                    "unexpected argument for hydrogen generate {}: {value}",
                    if all { "routes" } else { "route" }
                )));
            }
            value if all => {
                return Err(Error::invalid_input(format!(
                    "unexpected argument for hydrogen generate routes: {value}"
                )));
            }
            value if route_name.is_none() => route_name = Some(value.to_owned()),
            value => {
                return Err(Error::invalid_input(format!(
                    "unexpected route argument: {value}"
                )));
            }
        }
        index += 1;
    }

    let route_name = route_name.ok_or_else(|| {
        Error::invalid_input(format!(
            "route name is required; choose one of {}",
            ALL_ROUTE_CHOICES.join(", ")
        ))
    })?;
    if !ALL_ROUTE_CHOICES.contains(&route_name.as_str()) {
        return Err(Error::invalid_input(format!(
            "No route found for {route_name}. Try one of {}.",
            ALL_ROUTE_CHOICES.join(", ")
        )));
    }

    let options = GenerateRouteOptions {
        route_name,
        path: path.unwrap_or(current_directory()?),
        adapter,
        typescript,
        locale_param,
        force,
    };
    Ok(if all {
        NativeCommand::GenerateRoutes(options)
    } else {
        NativeCommand::GenerateRoute(options)
    })
}

fn print_unlink_help() {
    println!(
        "Unlink a local project from a Hydrogen storefront.\n\nUsage: cfy hydrogen unlink [OPTIONS]\n\nOptions:\n      --path <PATH>  Path to the storefront project [env: SHOPIFY_HYDROGEN_FLAG_PATH=]\n  -h, --help         Print help"
    );
}

fn print_generate_route_help() {
    println!(
        "Generates a standard Shopify route.\n\nUsage: cfy hydrogen generate route [OPTIONS] <ROUTENAME>\n\nArguments:\n  <ROUTENAME>  The route to generate. One of home, page, cart, products, collections, policies, blogs, account, search, robots, sitemap, all\n\nOptions:\n      --adapter <ADAPTER>          React Router adapter used in the route [env: SHOPIFY_HYDROGEN_FLAG_ADAPTER=]\n      --typescript                 Generate TypeScript files [env: SHOPIFY_HYDROGEN_FLAG_TYPESCRIPT=]\n      --locale-param <PARAM>       Param name used for the i18n locale\n  -f, --force                      Overwrite destination files [env: SHOPIFY_HYDROGEN_FLAG_FORCE=]\n      --path <PATH>                Path to the Hydrogen storefront [env: SHOPIFY_HYDROGEN_FLAG_PATH=]\n  -h, --help                       Print help"
    );
}

fn print_generate_routes_help() {
    println!(
        "Generates all supported standard Shopify routes.\n\nUsage: cfy hydrogen generate routes [OPTIONS]\n\nOptions:\n      --adapter <ADAPTER>          React Router adapter used in the routes [env: SHOPIFY_HYDROGEN_FLAG_ADAPTER=]\n      --typescript                 Generate TypeScript files [env: SHOPIFY_HYDROGEN_FLAG_TYPESCRIPT=]\n      --locale-param <PARAM>       Param name used for the i18n locale\n  -f, --force                      Overwrite destination files [env: SHOPIFY_HYDROGEN_FLAG_FORCE=]\n      --path <PATH>                Path to the Hydrogen storefront [env: SHOPIFY_HYDROGEN_FLAG_PATH=]\n  -h, --help                       Print help"
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouteOperation {
    Created,
    Skipped,
    Replaced,
}

impl RouteOperation {
    const fn label(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Skipped => "skipped",
            Self::Replaced => "replaced",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RouteResult {
    destination: PathBuf,
    operation: RouteOperation,
}

#[derive(Debug, Clone, Copy)]
struct RouteGeneration<'a> {
    root: &'a Path,
    app_directory: &'a Path,
    typescript: bool,
    locale: Option<&'a str>,
    v1: bool,
}

fn route_key_prefixes(route_name: &str) -> Option<&'static [&'static str]> {
    Some(match route_name {
        "home" => &["_index", "$"],
        "page" => &["pages*"],
        "cart" => &["cart", "cart.$lines", "discount.$code"],
        "products" => &["products*"],
        "collections" => &["collections*"],
        "policies" => &["policies*"],
        "blogs" => &["blogs*"],
        "account" => &["account*"],
        "search" => &["search", "api.predictive-search"],
        "robots" => &["[robots.txt]"],
        "sitemap" => &["[sitemap.xml]", "sitemap.$type.$page[.xml]"],
        _ => return None,
    })
}

fn all_route_prefixes() -> Vec<&'static str> {
    ALL_ROUTE_CHOICES
        .iter()
        .filter_map(|choice| route_key_prefixes(choice))
        .flatten()
        .map(|value| value.trim_end_matches('*'))
        .collect()
}

fn list_route_templates(source: &TemplateSource) -> Result<Vec<String>> {
    let mut routes = Vec::new();
    let directory = source.root.join("app/routes");
    let entries = fs::read_dir(&directory).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            format!(
                "could not list Hydrogen template routes in {}",
                directory.display()
            ),
            error,
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                "could not read Hydrogen template route entry",
                error,
            )
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = Path::new(&name)
            .extension()
            .and_then(|extension| extension.to_str())
            .and_then(|extension| {
                matches!(extension, "ts" | "tsx" | "js" | "jsx")
                    .then(|| name[..name.len() - extension.len() - 1].to_owned())
            })
        else {
            continue;
        };
        routes.push(stem);
    }
    routes.sort();
    routes.dedup();
    Ok(routes)
}

fn route_names(source: &TemplateSource, route_name: &str) -> Result<Vec<String>> {
    let all = list_route_templates(source)?;
    let prefixes = if route_name == "all" {
        all_route_prefixes()
    } else {
        route_key_prefixes(route_name)
            .map(|values| {
                values
                    .iter()
                    .map(|value| value.trim_end_matches('*'))
                    .collect::<Vec<_>>()
            })
            .ok_or_else(|| {
                Error::invalid_input(format!(
                    "No route found for {route_name}. Try one of {}.",
                    ALL_ROUTE_CHOICES.join(", ")
                ))
            })?
    };
    Ok(all
        .into_iter()
        .filter(|candidate| prefixes.iter().any(|prefix| candidate.starts_with(prefix)))
        .collect())
}

pub(crate) fn has_vite_config(root: &Path) -> bool {
    ["tsx", "ts", "jsx", "js", "mjs", "cjs"]
        .iter()
        .any(|extension| root.join(format!("vite.config.{extension}")).is_file())
}

fn project_directories(root: &Path) -> Result<(PathBuf, PathBuf)> {
    let root = fs::canonicalize(root).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            format!("could not resolve Hydrogen project {}", root.display()),
            error,
        )
    })?;
    if !has_vite_config(&root) {
        return Err(Error::config(
            "Classic Remix Compiler projects are no longer supported, please upgrade to Vite by running 'npx shopify hydrogen setup vite'",
        ));
    }

    // The official generator asks Vite for this setting. The common static forms
    // cover Hydrogen and React Router projects without executing user JavaScript.
    let app_directory = resolve_static_app_directory(&root).unwrap_or_else(|| root.join("app"));
    if !app_directory.starts_with(&root) {
        return Err(Error::config(
            "the configured Hydrogen appDirectory must stay inside the project",
        ));
    }
    Ok((root, app_directory))
}

pub(crate) fn resolve_static_app_directory(root: &Path) -> Option<PathBuf> {
    for filename in [
        "react-router.config.ts",
        "react-router.config.js",
        "react-router.config.mjs",
        "remix.config.ts",
        "remix.config.js",
        "remix.config.mjs",
        "vite.config.ts",
        "vite.config.js",
        "vite.config.mjs",
    ] {
        let Ok(contents) = fs::read_to_string(root.join(filename)) else {
            continue;
        };
        if let Some(value) = static_string_property(&contents, "appDirectory") {
            let path = Path::new(&value);
            if !path.is_absolute()
                && !path.components().any(|component| {
                    matches!(
                        component,
                        Component::ParentDir | Component::RootDir | Component::Prefix(_)
                    )
                })
            {
                return Some(root.join(path));
            }
        }
    }
    None
}

pub(crate) fn static_string_property(contents: &str, property: &str) -> Option<String> {
    let position = contents.find(property)?;
    let after = &contents[position + property.len()..];
    let colon = after.find(':')?;
    let value = after[colon + 1..].trim_start();
    let quote = value.chars().next()?;
    if quote != '\'' && quote != '"' && quote != '`' {
        return None;
    }
    let value = &value[quote.len_utf8()..];
    let end = value.find(quote)?;
    Some(value[..end].to_owned())
}

fn v1_route_convention_installed(root: &Path) -> bool {
    root.join("node_modules/@remix-run/v1-route-convention/package.json")
        .is_file()
}

fn infer_locale(routes_directory: &Path, v1: bool) -> Option<String> {
    let entries = fs::read_dir(routes_directory).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(locale) = locale_from_filename(&name, v1) {
            return Some(locale);
        }
    }
    None
}

fn locale_from_filename(name: &str, v1: bool) -> Option<String> {
    let rest = name.strip_prefix("($")?;
    let end = rest.find(')')?;
    let locale = &rest[..end];
    if locale.is_empty()
        || !locale
            .chars()
            .all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        return None;
    }
    let suffix = &rest[end + 1..];
    let matches = if v1 {
        suffix.is_empty()
    } else {
        [
            "._index.tsx",
            "._index.jsx",
            ".$.tsx",
            ".$.jsx",
            ".cart.tsx",
            ".cart.jsx",
        ]
        .contains(&suffix)
    };
    matches.then(|| locale.to_owned())
}

fn convert_route_to_v1(route: &str) -> String {
    let mut output = String::new();
    let mut bracket_depth = 0_u32;
    for character in route.chars() {
        match character {
            '[' => {
                bracket_depth += 1;
                output.push(character);
            }
            ']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                output.push(character);
            }
            '.' if bracket_depth == 0 => output.push('/'),
            _ => output.push(character),
        }
    }
    if output.ends_with("/_index") {
        output.truncate(output.len() - "_index".len());
        output.push_str("index");
    } else if output == "_index" {
        output = "index".to_owned();
    }
    output
}

fn route_destination_name(route: &str, locale: Option<&str>, v1: bool) -> String {
    let route = if v1 {
        convert_route_to_v1(route)
    } else {
        route.to_owned()
    };
    if route.contains("robots.txt") {
        return route;
    }
    match locale {
        Some(locale) if v1 => format!("(${locale_prefix})/{route}", locale_prefix = locale),
        Some(locale) => format!("(${locale_prefix}).{route}", locale_prefix = locale),
        None => route,
    }
}

fn confirm_replace(relative_path: &Path) -> Result<bool> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Ok(false);
    }
    eprint!(
        "The file {} already exists. Do you want to replace it? [y/N] ",
        relative_path.display()
    );
    io::stderr().flush().map_err(|error| {
        Error::with_source(ErrorKind::Process, "could not display confirmation", error)
    })?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).map_err(|error| {
        Error::with_source(ErrorKind::Process, "could not read confirmation", error)
    })?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn source_extension(typescript: bool, jsx: bool) -> &'static str {
    match (typescript, jsx) {
        (true, true) => "tsx",
        (true, false) => "ts",
        (false, true) => "jsx",
        (false, false) => "js",
    }
}

fn transform_adapter(contents: &[u8], adapter: Option<&str>) -> Vec<u8> {
    let Some(adapter) = adapter else {
        return contents.to_vec();
    };
    let text = String::from_utf8_lossy(contents);
    text.replace("from 'react-router'", &format!("from '{adapter}'"))
        .replace("from \"react-router\"", &format!("from \"{adapter}\""))
        .into_bytes()
}

pub(crate) fn write_generated_file(path: &Path, contents: &[u8]) -> Result<()> {
    reject_symlink_ancestors(path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                format!("could not create {}", parent.display()),
                error,
            )
        })?;
    }
    cfy_config::write_atomic(path, contents).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            format!("could not write generated file {}", path.display()),
            error,
        )
    })
}

pub(crate) fn reject_symlink_ancestors(path: &Path) -> Result<()> {
    let mut current = path.parent();
    while let Some(directory) = current {
        match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                // macOS exposes /var as a system alias to /private/var, and
                // its standard temporary directory lives beneath it. It is
                // not a project-controlled escape; all other symlinks remain
                // prohibited, including any inside a project tree.
                if cfg!(target_os = "macos") && directory == Path::new("/var") {
                    current = directory.parent();
                    continue;
                }
                return Err(Error::config(format!(
                    "refusing to generate through symlink {}",
                    directory.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Error::with_source(
                    ErrorKind::Config,
                    format!("could not inspect {}", directory.display()),
                    error,
                ));
            }
        }
        current = directory.parent();
    }
    Ok(())
}

fn find_dependencies(source: &TemplateSource, route_path: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut dependencies = BTreeSet::new();
    let mut pending = vec![route_path.to_path_buf()];
    let mut checked = HashSet::new();

    while let Some(path) = pending.pop() {
        if !checked.insert(path.clone()) {
            continue;
        }
        dependencies.insert(path.clone());
        if !is_code_file(&path) {
            continue;
        }
        let contents = String::from_utf8_lossy(&source.read(&path)?).into_owned();
        for module in imported_modules(&contents) {
            if module.contains("/+types/") || !(module.starts_with('.') || module.starts_with('~'))
            {
                continue;
            }
            let module = module
                .split_once('?')
                .map_or(module.as_str(), |(base, _)| base);
            let base = if let Some(relative) = module.strip_prefix("~/") {
                Path::new("app").join(relative)
            } else {
                path.parent().unwrap_or_else(|| Path::new("")).join(module)
            };
            let Some(resolved) = resolve_import_path(source, &base) else {
                continue;
            };
            if resolved.starts_with(Path::new("app/routes")) {
                continue;
            }
            if is_code_file(&resolved) {
                pending.push(resolved.clone());
            }
            dependencies.insert(resolved);
        }
    }
    Ok(dependencies)
}

fn is_code_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|value| value.to_str()),
        Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs")
    )
}

fn imported_modules(contents: &str) -> Vec<String> {
    static IMPORT_RE: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = IMPORT_RE.get_or_init(|| {
        regex::Regex::new("(?ims)^(import|export)\\s+.*?\\s+from\\s+['\"](.*?)['\"];?$")
            .expect("valid import extraction regex")
    });
    pattern
        .captures_iter(contents)
        .filter_map(|captures| captures.get(2))
        .map(|value| value.as_str().to_owned())
        .collect()
}

fn resolve_import_path(source: &TemplateSource, path: &Path) -> Option<PathBuf> {
    let normalized = normalize_relative_path(path)?;
    if source.has_file(&normalized) {
        return Some(normalized);
    }
    for extension in ["tsx", "ts", "jsx", "js", "mjs", "cjs"] {
        let candidate = normalized.with_extension(extension);
        if source.has_file(&candidate) {
            return Some(candidate);
        }
    }
    for extension in ["tsx", "ts", "jsx", "js", "mjs", "cjs"] {
        let candidate = normalized.join(format!("index.{extension}"));
        if source.has_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn normalize_relative_path(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(normalized)
}

fn dependency_destination(path: &Path, typescript: bool) -> PathBuf {
    if typescript {
        return path.to_path_buf();
    }
    match path.extension().and_then(|value| value.to_str()) {
        Some("tsx") => path.with_extension("jsx"),
        Some("ts") => path.with_extension("js"),
        _ => path.to_path_buf(),
    }
}

fn route_source_path(source: &TemplateSource, route: &str) -> Result<PathBuf> {
    for extension in ["tsx", "ts", "jsx", "js"] {
        let candidate = Path::new("app/routes").join(format!("{route}.{extension}"));
        if source.has_file(&candidate) {
            return Ok(candidate);
        }
    }
    Err(Error::config(format!(
        "the official Hydrogen template has no route named {route}"
    )))
}

fn generate_route(
    source: &TemplateSource,
    route: &str,
    options: &GenerateRouteOptions,
    config: RouteGeneration<'_>,
) -> Result<RouteResult> {
    let extension = source_extension(config.typescript, true);
    let source_path = route_source_path(source, route)?;
    let destination_name = route_destination_name(route, config.locale, config.v1);
    let destination_path = config
        .app_directory
        .join("routes")
        .join(format!("{destination_name}.{extension}"));
    let destination = destination_path
        .strip_prefix(config.root)
        .unwrap_or(&destination_path)
        .to_path_buf();

    let operation = if destination_path.exists() {
        if options.force || confirm_replace(&destination)? {
            RouteOperation::Replaced
        } else {
            return Ok(RouteResult {
                destination,
                operation: RouteOperation::Skipped,
            });
        }
    } else {
        RouteOperation::Created
    };

    for dependency in find_dependencies(source, &source_path)? {
        let target = if dependency == source_path {
            destination_path.clone()
        } else {
            let relative = dependency.strip_prefix("app").unwrap_or(&dependency);
            config
                .app_directory
                .join(dependency_destination(relative, config.typescript))
        };
        let contents = source.read(&dependency)?;
        let contents = if !config.typescript
            && matches!(
                dependency.extension().and_then(|value| value.to_str()),
                Some("ts" | "tsx")
            ) {
            transpile_typescript(&contents, &dependency)?
        } else {
            contents
        };
        let contents = transform_adapter(&contents, options.adapter.as_deref());
        write_generated_file(&target, &contents)?;
    }

    Ok(RouteResult {
        destination,
        operation,
    })
}

fn copy_locale_route(
    source: &TemplateSource,
    routes_directory: &Path,
    typescript: bool,
    adapter: Option<&str>,
    locale: &str,
) -> Result<()> {
    let extension = source_extension(typescript, true);
    let locale_path = routes_directory.join(format!("(${locale}).{extension}"));
    if locale_path.exists() {
        return Ok(());
    }
    let contents = source.read(Path::new("locale-check.ts"))?;
    let contents = if typescript {
        contents
    } else {
        transpile_typescript(&contents, Path::new("locale-check.ts"))?
    };
    let contents = transform_adapter(&contents, adapter);
    write_generated_file(&locale_path, &contents)
}

fn generate_routes(options: &GenerateRouteOptions) -> Result<()> {
    let (root, app_directory) = project_directories(&options.path)?;
    let typescript = options
        .typescript
        .unwrap_or_else(|| root.join("tsconfig.json").is_file());
    let v1 = v1_route_convention_installed(&root);
    let routes_directory = app_directory.join("routes");
    let locale = options.locale_param.clone().or_else(|| {
        (options.route_name != "all")
            .then(|| infer_locale(&routes_directory, v1))
            .flatten()
    });
    let source = resolve_template_source(&root)?;
    let routes = route_names(&source, &options.route_name)?;
    let config = RouteGeneration {
        root: &root,
        app_directory: &app_directory,
        typescript,
        locale: locale.as_deref(),
        v1,
    };

    let mut results = Vec::new();
    for route in routes {
        results.push(generate_route(&source, &route, options, config)?);
    }

    if let Some(locale) = locale.as_deref() {
        copy_locale_route(
            &source,
            &routes_directory,
            typescript,
            options.adapter.as_deref(),
            locale,
        )?;
    }

    let generated = results
        .iter()
        .filter(|result| result.operation != RouteOperation::Skipped)
        .count();
    let noun = if results.len() == 1 {
        "route"
    } else {
        "routes"
    };
    println!("{generated} of {} {noun} generated", results.len());
    let width = results
        .iter()
        .map(|result| result.destination.display().to_string().len())
        .max()
        .unwrap_or(0)
        + 3;
    for result in results {
        println!(
            "{:<width$}[{}]",
            result.destination.display(),
            result.operation.label(),
            width = width
        );
    }
    Ok(())
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
        Some(NativeCommand::GenerateRoute(options))
        | Some(NativeCommand::GenerateRoutes(options)) => {
            tokio::task::spawn_blocking(move || generate_routes(&options))
                .await
                .map_err(|error| {
                    Error::process(format!("route generator task failed: {error}"))
                })??;
            return Ok(0);
        }
        Some(NativeCommand::GenerateRouteHelp) => {
            print_generate_route_help();
            return Ok(0);
        }
        Some(NativeCommand::GenerateRoutesHelp) => {
            print_generate_routes_help();
            return Ok(0);
        }
        Some(NativeCommand::SetupMarkets(options)) => {
            tokio::task::spawn_blocking(move || setup::run_setup_markets(&options))
                .await
                .map_err(|error| Error::process(format!("setup markets task failed: {error}")))??;
            return Ok(0);
        }
        Some(NativeCommand::SetupMarketsHelp) => {
            setup::print_setup_markets_help();
            return Ok(0);
        }
        Some(NativeCommand::SetupCss(options)) => {
            tokio::task::spawn_blocking(move || setup::run_setup_css(&options))
                .await
                .map_err(|error| Error::process(format!("setup css task failed: {error}")))??;
            return Ok(0);
        }
        Some(NativeCommand::SetupCssHelp) => {
            setup::print_setup_css_help();
            return Ok(0);
        }
        Some(NativeCommand::SetupVite(options)) => {
            tokio::task::spawn_blocking(move || vite::run_setup_vite(&options))
                .await
                .map_err(|error| Error::process(format!("setup vite task failed: {error}")))??;
            return Ok(0);
        }
        Some(NativeCommand::SetupViteHelp) => {
            vite::print_setup_vite_help();
            return Ok(0);
        }
        Some(NativeCommand::Shortcut) => {
            tokio::task::spawn_blocking(shortcut::run_create_shortcut)
                .await
                .map_err(|error| Error::process(format!("shortcut task failed: {error}")))??;
            return Ok(0);
        }
        Some(NativeCommand::ShortcutHelp) => {
            shortcut::print_shortcut_help();
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

    fn write_source_file(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn template_source(name: &str) -> (PathBuf, TemplateSource) {
        let root = fixture(name);
        let source = TemplateSource { root: root.clone() };
        (root, source)
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
    fn parses_native_route_generation() {
        let command = parse_native_command(&[
            "generate".into(),
            "route".into(),
            "products".into(),
            "--typescript".into(),
            "--path=storefront".into(),
            "--force".into(),
        ])
        .unwrap();
        assert_eq!(
            command,
            Some(NativeCommand::GenerateRoute(GenerateRouteOptions {
                route_name: "products".into(),
                path: PathBuf::from("storefront"),
                adapter: None,
                typescript: Some(true),
                locale_param: None,
                force: true,
            }))
        );
        assert!(matches!(
            parse_native_command(&["generate".into(), "routes".into(), "--help".into()]).unwrap(),
            Some(NativeCommand::GenerateRoutesHelp)
        ));
    }

    #[test]
    fn generates_typescript_route_and_dependencies() {
        let root = fixture("generate-route-ts");
        let (template_root, source) = template_source("generate-route-ts-template");
        fs::create_dir_all(root.join("app/routes")).unwrap();
        fs::write(root.join("vite.config.ts"), "export default {};").unwrap();
        fs::write(root.join("tsconfig.json"), "{}").unwrap();
        write_source_file(
            &template_root,
            "app/routes/products.$handle.tsx",
            "import {ProductForm} from '~/components/ProductForm';\nexport default function Product() { return <ProductForm />; }\n",
        );
        write_source_file(
            &template_root,
            "app/components/ProductForm.tsx",
            "import {ProductPrice} from './ProductPrice';\nexport function ProductForm() { return <ProductPrice />; }\n",
        );
        write_source_file(
            &template_root,
            "app/components/ProductPrice.tsx",
            "export function ProductPrice() { return null; }\n",
        );
        write_source_file(
            &template_root,
            "locale-check.ts",
            "export async function loader() {}\n",
        );

        let options = GenerateRouteOptions {
            route_name: "products".into(),
            path: root.clone(),
            adapter: None,
            typescript: Some(true),
            locale_param: None,
            force: false,
        };

        let (project, app) = project_directories(&root).unwrap();
        let config = RouteGeneration {
            root: &project,
            app_directory: &app,
            typescript: true,
            locale: None,
            v1: false,
        };
        for route in route_names(&source, "products").unwrap() {
            generate_route(&source, &route, &options, config).unwrap();
        }

        assert!(root.join("app/routes/products.$handle.tsx").is_file());
        assert!(root.join("app/components/ProductForm.tsx").is_file());
        assert!(root.join("app/components/ProductPrice.tsx").is_file());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(template_root).unwrap();
    }

    #[test]
    fn extracts_multiline_local_imports() {
        let source = r#"import {
  type LoaderReturnData,
} from '~/lib/search';
export function Search() {
  return null;
}
"#;
        let modules = imported_modules(source);
        assert_eq!(modules, vec!["~/lib/search".to_owned()]);
    }

    #[test]
    fn transpiles_official_typescript_template_to_javascript() {
        let source = br#"import type {Route} from './+types/test';
export const meta: Route.MetaFunction = () => [{title: 'Test'}];
export default function Test({value}: {value: string}) {
  return <p>{value}</p>;
}
"#;
        let output =
            String::from_utf8(transpile_typescript(source, Path::new("routes/test.tsx")).unwrap())
                .unwrap();
        assert!(!output.contains("import type"));
        assert!(!output.contains("Route.MetaFunction"));
        assert!(!output.contains(": string"));
        assert!(output.contains("<p>{value}</p>"));
    }

    #[test]
    fn selects_project_compatible_skeleton_tag() {
        let root = fixture("template-version");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("package.json"),
            r#"{"dependencies":{"@shopify/hydrogen":"2026.4.5"}}"#,
        )
        .unwrap();
        assert_eq!(pinned_template_ref(&root), "skeleton@2026.4.5");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generates_javascript_route_and_locale_from_typescript_template() {
        let root = fixture("generate-route-js");
        let (template_root, source) = template_source("generate-route-js-template");
        fs::create_dir_all(root.join("app/routes")).unwrap();
        fs::write(root.join("vite.config.js"), "export default {};").unwrap();
        write_source_file(
            &template_root,
            "app/routes/pages.$handle.tsx",
            "import {useLoaderData} from 'react-router';\nexport function loader({params}: {params: {handle?: string}}) { return params.handle; }\nexport default function Page() { const handle = useLoaderData<typeof loader>(); return <h1>{handle}</h1>; }\n",
        );
        write_source_file(
            &template_root,
            "locale-check.ts",
            "import type {LoaderFunctionArgs} from 'react-router';\nexport async function loader({params, context}: LoaderFunctionArgs) { return null; }\n",
        );

        let options = GenerateRouteOptions {
            route_name: "page".into(),
            path: root.clone(),
            adapter: Some("@shopify/remix-oxygen".into()),
            typescript: Some(false),
            locale_param: Some("locale".into()),
            force: false,
        };

        let (project, app) = project_directories(&root).unwrap();
        let config = RouteGeneration {
            root: &project,
            app_directory: &app,
            typescript: false,
            locale: Some("locale"),
            v1: false,
        };
        generate_route(&source, "pages.$handle", &options, config).unwrap();
        copy_locale_route(
            &source,
            &app.join("routes"),
            false,
            options.adapter.as_deref(),
            "locale",
        )
        .unwrap();

        let route =
            fs::read_to_string(root.join("app/routes/($locale).pages.$handle.jsx")).unwrap();
        assert!(route.contains("@shopify/remix-oxygen"));
        assert!(!route.contains("params: {"));
        assert!(!route.contains("LoaderFunctionArgs"));
        let locale = fs::read_to_string(root.join("app/routes/($locale).jsx")).unwrap();
        assert!(!locale.contains("LoaderFunctionArgs"));
        assert!(locale.contains("export async function loader"));
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(template_root).unwrap();
    }

    #[test]
    fn extracts_only_the_official_skeleton_app_and_locale_template() {
        let root = fixture("template-archive");
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut archive = zip::ZipWriter::new(&mut bytes);
            let options = zip::write::SimpleFileOptions::default();
            archive
                .start_file(
                    "hydrogen-commit/templates/skeleton/app/routes/_index.tsx",
                    options,
                )
                .unwrap();
            archive
                .write_all(b"export default function Home() {}\n")
                .unwrap();
            archive
                .start_file(
                    "hydrogen-commit/templates/skeleton/app/components/Foo.tsx",
                    options,
                )
                .unwrap();
            archive.write_all(b"export function Foo() {}\n").unwrap();
            archive
                .start_file(
                    "hydrogen-commit/packages/cli/assets/routes/locale-check.ts",
                    options,
                )
                .unwrap();
            archive
                .write_all(b"export async function loader() {}\n")
                .unwrap();
            for (upstream, _) in SETUP_ASSET_FILES {
                archive
                    .start_file(format!("hydrogen-commit/{upstream}"), options)
                    .unwrap();
                archive.write_all(b"export default {};\n").unwrap();
            }
            archive
                .start_file("hydrogen-commit/README.md", options)
                .unwrap();
            archive.write_all(b"ignored").unwrap();
            archive.finish().unwrap();
        }

        extract_template_archive(bytes.get_ref(), &root).unwrap();
        assert!(root.join("app/routes/_index.tsx").is_file());
        assert!(root.join("app/components/Foo.tsx").is_file());
        assert!(root.join("locale-check.ts").is_file());
        assert!(root.join("assets/vite/vite.config.js").is_file());
        assert!(!root.join("README.md").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn installs_official_template_from_local_archive() {
        let root = fixture("template-install");
        let archive_path = root.join("hydrogen.zip");
        let cache_dir = root.join("cache").join("test-ref");
        fs::create_dir_all(archive_path.parent().unwrap()).unwrap();
        {
            let mut bytes = Cursor::new(Vec::new());
            {
                let mut archive = zip::ZipWriter::new(&mut bytes);
                let options = zip::write::SimpleFileOptions::default();
                archive
                    .start_file(
                        "hydrogen-commit/templates/skeleton/app/routes/_index.tsx",
                        options,
                    )
                    .unwrap();
                archive
                    .write_all(b"export default function Home() {}\n")
                    .unwrap();
                archive
                    .start_file(
                        "hydrogen-commit/packages/cli/assets/routes/locale-check.ts",
                        options,
                    )
                    .unwrap();
                archive
                    .write_all(b"export async function loader() {}\n")
                    .unwrap();
                for (upstream, _) in SETUP_ASSET_FILES {
                    archive
                        .start_file(format!("hydrogen-commit/{upstream}"), options)
                        .unwrap();
                    archive.write_all(b"export default {};\n").unwrap();
                }
                archive.finish().unwrap();
            }
            fs::write(&archive_path, bytes.get_ref()).unwrap();
        }

        fetch_official_template(archive_path.to_str().unwrap(), &cache_dir).unwrap();
        assert!(cache_dir.join("app/routes/_index.tsx").is_file());
        assert!(cache_dir.join("locale-check.ts").is_file());
        assert!(cache_dir.join("assets/tailwind/tailwind.css").is_file());
        assert!(cache_dir.join(".complete").is_file());
        fs::remove_dir_all(root).unwrap();
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
