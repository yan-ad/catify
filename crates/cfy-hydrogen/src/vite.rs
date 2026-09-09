use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use cfy_core::{Error, ErrorKind, Result};
use regex::Regex;

use crate::{
    NativeCommand, current_directory, has_vite_config, resolve_static_app_directory,
    resolve_template_source, static_string_property, write_generated_file,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SetupViteOptions {
    path: PathBuf,
}

fn parse_path(args: &[String]) -> Result<(PathBuf, Vec<&str>)> {
    let mut path = env::var_os("SHOPIFY_HYDROGEN_FLAG_PATH").map(PathBuf::from);
    let mut rest = Vec::new();
    let mut index = 0;
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
            value => rest.push(value),
        }
        index += 1;
    }
    Ok((path.unwrap_or(current_directory()?), rest))
}

pub(crate) fn parse_setup_vite(args: &[String]) -> Result<NativeCommand> {
    let (path, rest) = parse_path(args)?;
    if rest.iter().any(|value| matches!(*value, "--help" | "-h")) {
        if rest.len() == 1 {
            return Ok(NativeCommand::SetupViteHelp);
        }
        return Err(Error::invalid_input(
            "hydrogen setup vite --help does not accept other arguments",
        ));
    }
    if let Some(value) = rest.first() {
        return Err(Error::invalid_input(format!(
            "unexpected argument for hydrogen setup vite: {value}"
        )));
    }
    Ok(NativeCommand::SetupVite(SetupViteOptions { path }))
}

pub(crate) fn print_setup_vite_help() {
    println!(
        "EXPERIMENTAL: Upgrades the project to use Vite.\n\nUsage: cfy hydrogen setup vite [OPTIONS]\n\nOptions:\n      --path <PATH>  Path to the Hydrogen storefront [env: SHOPIFY_HYDROGEN_FLAG_PATH=]\n  -h, --help         Print help"
    );
}

fn classic_remix_config(root: &Path) -> Result<PathBuf> {
    ["js", "cjs", "mjs", "ts"]
        .iter()
        .map(|extension| root.join(format!("remix.config.{extension}")))
        .find(|path| path.is_file())
        .ok_or_else(|| {
            Error::config(
                "could not find remix.config.js; hydrogen setup vite only supports Classic Remix projects",
            )
        })
}

fn server_entry(config: &str) -> String {
    static SERVER: OnceLock<Regex> = OnceLock::new();
    SERVER
        .get_or_init(|| {
            Regex::new(r#"(?m)\bserver\s*:\s*['\"]([^'\"]+)['\"]"#)
                .expect("valid server entry regex")
        })
        .captures(config)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_owned())
        .unwrap_or_else(|| "server.js".to_owned())
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(_) if !path.exists() => Ok(()),
        Err(error) => Err(Error::with_source(
            ErrorKind::Config,
            format!("could not remove {}", path.display()),
            error,
        )),
    }
}

fn rename_if_exists(from: &Path, to: &Path) -> Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) if !from.exists() => Ok(()),
        Err(error) => Err(Error::with_source(
            ErrorKind::Config,
            format!("could not rename {}", from.display()),
            error,
        )),
    }
}

fn read_utf8(path: &Path) -> Result<String> {
    fs::read_to_string(path).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            format!("could not read {}", path.display()),
            error,
        )
    })
}

fn css_url_imports(contents: &str) -> String {
    static CSS: OnceLock<Regex> = OnceLock::new();
    let css = CSS.get_or_init(|| {
        Regex::new(r#"(?m)^(\s*import\s+[^\n]*?['\"][^'\"]+\.css)(['\"];?)$"#)
            .expect("valid CSS import regex")
    });
    css.replace_all(contents, |captures: &regex::Captures<'_>| {
        let whole = captures.get(0).map_or("", |value| value.as_str());
        if whole.contains(".module.css") || whole.contains(".css?url") {
            whole.to_owned()
        } else {
            format!("{}?url{}", &captures[1], &captures[2])
        }
    })
    .into_owned()
}

fn rewrite_root(contents: &str) -> String {
    static LIVE_RELOAD_IMPORT: OnceLock<Regex> = OnceLock::new();
    static CSS_BUNDLE_IMPORT: OnceLock<Regex> = OnceLock::new();
    let output = LIVE_RELOAD_IMPORT
        .get_or_init(|| {
            Regex::new(r#"(?m)^\s*import\s*\{[^}]*\bLiveReload\b[^}]*\}\s*from\s*['\"]@remix-run/react['\"];?\s*\n?"#)
                .expect("valid LiveReload import regex")
        })
        .replace_all(contents, "");
    let output = CSS_BUNDLE_IMPORT
        .get_or_init(|| {
            Regex::new(r#"(?m)^\s*import\s*\{\s*cssBundleHref\s*\}\s*from\s*['\"]@remix-run/css-bundle['\"];?\s*\n?"#)
                .expect("valid css bundle import regex")
        })
        .replace_all(&output, "");
    css_url_imports(
        &output
            .replace("<LiveReload />", "")
            .replace("<LiveReload/>", "")
            .replace("<LiveReload></LiveReload>", "")
            .replace("...cssBundleHref,", "")
            .replace("...cssBundleHref", ""),
    )
}

fn root_file(root: &Path, app: &Path) -> Result<PathBuf> {
    ["tsx", "ts", "jsx", "js"]
        .iter()
        .map(|extension| app.join(format!("root.{extension}")))
        .find(|path| path.is_file())
        .ok_or_else(|| {
            Error::config(format!(
                "could not find a route root file under {}",
                root.join("app").display()
            ))
        })
}

fn merge_vite_package(root: &Path, addition: &[u8]) -> Result<()> {
    let path = root.join("package.json");
    let current: serde_json::Value = serde_json::from_str(&read_utf8(&path)?).map_err(|error| {
        Error::with_source(ErrorKind::Config, "could not parse package.json", error)
    })?;
    let extra: serde_json::Value = serde_json::from_slice(addition).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            "could not parse official Hydrogen Vite package asset",
            error,
        )
    })?;
    let mut current = current
        .as_object()
        .cloned()
        .ok_or_else(|| Error::config("package.json must be an object"))?;
    for section in ["dependencies", "devDependencies", "scripts"] {
        let Some(additions) = extra.get(section).and_then(serde_json::Value::as_object) else {
            continue;
        };
        let target = current
            .entry(section)
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        let target = target
            .as_object_mut()
            .ok_or_else(|| Error::config(format!("package.json {section} must be an object")))?;
        for (name, value) in additions {
            target.entry(name.clone()).or_insert_with(|| value.clone());
        }
    }
    for section in ["dependencies", "devDependencies"] {
        if let Some(values) = current
            .get_mut(section)
            .and_then(serde_json::Value::as_object_mut)
        {
            values.remove("@remix-run/css-bundle");
        }
    }
    let contents =
        serde_json::to_vec_pretty(&serde_json::Value::Object(current)).map_err(|error| {
            Error::with_source(ErrorKind::Config, "could not serialize package.json", error)
        })?;
    write_generated_file(&path, &contents)
}

pub(crate) fn run_setup_vite(options: &SetupViteOptions) -> Result<()> {
    if has_vite_config(&options.path) {
        return Err(Error::config(
            "This project already has a Vite config file.",
        ));
    }
    let remix_config = classic_remix_config(&options.path)?;
    let config = read_utf8(&remix_config)?;
    let server = server_entry(&config);
    let is_typescript = server.ends_with(".ts");
    let extension = if is_typescript { "ts" } else { "js" };
    let app =
        resolve_static_app_directory(&options.path).unwrap_or_else(|| options.path.join("app"));
    let source = resolve_template_source(&options.path)?;
    let mut vite = String::from_utf8(source.read(Path::new("assets/vite/vite.config.js"))?)
        .map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                "official Hydrogen Vite asset is not UTF-8",
                error,
            )
        })?;
    if let Some(directory) =
        static_string_property(&config, "appDirectory").filter(|value| value != "app")
    {
        vite = vite.replacen(
            "future:",
            &format!("appDirectory: '{directory}',\n    future:"),
            1,
        );
    }
    merge_vite_package(
        &options.path,
        &source.read(Path::new("assets/vite/package.json"))?,
    )?;
    remove_if_exists(&remix_config)?;
    rename_if_exists(
        &options.path.join("remix.env.d.ts"),
        &options.path.join("env.d.ts"),
    )?;
    let env_dts = options.path.join("env.d.ts");
    if env_dts.is_file() {
        write_generated_file(
            &env_dts,
            read_utf8(&env_dts)?
                .replace("types=\"@remix-run/dev\"", "types=\"vite/client\"")
                .as_bytes(),
        )?;
    }
    rename_if_exists(
        &options.path.join(".eslintrc.js"),
        &options.path.join(".eslintrc.cjs"),
    )?;
    let server_path = options.path.join(&server);
    if server_path.is_file() {
        let prefix = if is_typescript { "// @ts-ignore\n" } else { "" };
        write_generated_file(
            &server_path,
            format!(
                "{prefix}{}",
                read_utf8(&server_path)?
                    .replace("@remix-run/dev/server-build", "virtual:remix/server-build")
            )
            .as_bytes(),
        )?;
    }
    let root = root_file(&options.path, &app)?;
    write_generated_file(&root, rewrite_root(&read_utf8(&root)?).as_bytes())?;
    let routes = app.join("routes");
    if routes.is_dir() {
        for entry in fs::read_dir(&routes).map_err(|error| {
            Error::with_source(ErrorKind::Config, "could not read Hydrogen routes", error)
        })? {
            let path = entry
                .map_err(|error| {
                    Error::with_source(ErrorKind::Config, "could not inspect Hydrogen route", error)
                })?
                .path();
            if path.is_file()
                && matches!(
                    path.extension().and_then(|value| value.to_str()),
                    Some("js" | "jsx" | "ts" | "tsx")
                )
            {
                write_generated_file(&path, css_url_imports(&read_utf8(&path)?).as_bytes())?;
            }
        }
    }
    write_generated_file(
        &options.path.join(format!("vite.config.{extension}")),
        vite.as_bytes(),
    )?;
    println!("Your Vite project is ready!\nPlease use Git to review the changes.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vite_flags() {
        assert!(matches!(
            parse_setup_vite(&["--path".into(), "storefront".into()]).unwrap(),
            NativeCommand::SetupVite(SetupViteOptions { path }) if path == Path::new("storefront")
        ));
    }

    #[test]
    fn rewrites_classic_remix_files() {
        let root = rewrite_root(
            "import {LiveReload} from '@remix-run/react';\nimport {cssBundleHref} from '@remix-run/css-bundle';\nimport site from './site.css';\nexport const links = () => [...cssBundleHref];\nexport default () => <LiveReload />;\n",
        );
        assert!(!root.contains("LiveReload"));
        assert!(!root.contains("cssBundleHref"));
        assert!(root.contains("./site.css?url"));
        assert_eq!(server_entry("server: 'server.ts'"), "server.ts");
    }
}
