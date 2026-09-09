use std::{
    env, fs,
    path::{Path, PathBuf},
};

use cfy_core::{Error, ErrorKind, Result};
use serde_json::{Map, Value};

use super::{
    NativeCommand, current_directory, has_vite_config, resolve_template_source,
    transpile_typescript, write_generated_file,
};

const I18N_STRATEGIES: &[&str] = &["subfolders", "domains", "subdomains"];
const CSS_STRATEGIES: &[&str] = &["tailwind", "vanilla-extract", "css-modules", "postcss"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SetupMarketsOptions {
    path: PathBuf,
    strategy: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SetupCssOptions {
    path: PathBuf,
    strategy: Option<String>,
    force: bool,
    install_deps: bool,
}

fn env_bool(name: &str) -> Result<Option<bool>> {
    let Some(value) = env::var_os(name) else {
        return Ok(None);
    };
    match value.to_string_lossy().trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(Some(true)),
        "0" | "false" | "no" | "off" | "" => Ok(Some(false)),
        _ => Err(Error::invalid_input(format!(
            "{name} must be true or false"
        ))),
    }
}

fn parse_path_flag(args: &[String], command: &str) -> Result<(PathBuf, Vec<String>)> {
    let mut path = env::var_os("SHOPIFY_HYDROGEN_FLAG_PATH").map(PathBuf::from);
    let mut remaining = Vec::new();
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
            value => remaining.push(value.to_owned()),
        }
        index += 1;
    }
    let path = path.unwrap_or(current_directory()?);
    if path.as_os_str().is_empty() {
        return Err(Error::invalid_input(format!(
            "{command} path cannot be empty"
        )));
    }
    Ok((path, remaining))
}

pub(crate) fn parse_setup_markets(args: &[String]) -> Result<NativeCommand> {
    let (path, remaining) = parse_path_flag(args, "hydrogen setup markets")?;
    let mut strategy = env::var("SHOPIFY_HYDROGEN_ARG_STRATEGY").ok();
    for value in remaining {
        if matches!(value.as_str(), "--help" | "-h") {
            return Ok(NativeCommand::SetupMarketsHelp);
        }
        if value.starts_with('-') {
            return Err(Error::invalid_input(format!(
                "unexpected argument for hydrogen setup markets: {value}"
            )));
        }
        if strategy.replace(value.clone()).is_some() {
            return Err(Error::invalid_input(
                "hydrogen setup markets accepts at most one strategy",
            ));
        }
    }
    if let Some(strategy) = &strategy {
        validate_strategy(strategy, I18N_STRATEGIES, "markets")?;
    }
    Ok(NativeCommand::SetupMarkets(SetupMarketsOptions {
        path,
        strategy,
    }))
}

pub(crate) fn parse_setup_css(args: &[String]) -> Result<NativeCommand> {
    let (path, remaining) = parse_path_flag(args, "hydrogen setup css")?;
    let mut strategy = env::var("SHOPIFY_HYDROGEN_ARG_STRATEGY").ok();
    let mut force = env_bool("SHOPIFY_HYDROGEN_FLAG_FORCE")?.unwrap_or(false);
    let mut install_deps = env_bool("SHOPIFY_HYDROGEN_FLAG_INSTALL_DEPS")?.unwrap_or(true);
    for value in remaining {
        match value.as_str() {
            "--help" | "-h" => return Ok(NativeCommand::SetupCssHelp),
            "-f" | "--force" => force = true,
            "--install-deps" => install_deps = true,
            "--no-install-deps" => install_deps = false,
            value if value.starts_with('-') => {
                return Err(Error::invalid_input(format!(
                    "unexpected argument for hydrogen setup css: {value}"
                )));
            }
            value => {
                if strategy.replace(value.to_owned()).is_some() {
                    return Err(Error::invalid_input(
                        "hydrogen setup css accepts at most one strategy",
                    ));
                }
            }
        }
    }
    if let Some(strategy) = &strategy {
        validate_strategy(strategy, CSS_STRATEGIES, "CSS")?;
    }
    Ok(NativeCommand::SetupCss(SetupCssOptions {
        path,
        strategy,
        force,
        install_deps,
    }))
}

fn validate_strategy(value: &str, choices: &[&str], label: &str) -> Result<()> {
    if choices.contains(&value) {
        Ok(())
    } else {
        Err(Error::invalid_input(format!(
            "unknown {label} strategy {value:?}; expected one of {}",
            choices.join(", ")
        )))
    }
}

pub(crate) fn print_setup_markets_help() {
    println!(
        "Adds support for multiple markets to your project by using the URL structure.\n\nUsage: cfy hydrogen setup markets [OPTIONS] [STRATEGY]\n\nArguments:\n  [STRATEGY]  One of subfolders, domains, subdomains [env: SHOPIFY_HYDROGEN_ARG_STRATEGY=]\n\nOptions:\n      --path <PATH>  Path to the Hydrogen storefront [env: SHOPIFY_HYDROGEN_FLAG_PATH=]\n  -h, --help         Print help"
    );
}

pub(crate) fn print_setup_css_help() {
    println!(
        "Adds support for certain CSS strategies to your project.\n\nUsage: cfy hydrogen setup css [OPTIONS] [STRATEGY]\n\nArguments:\n  [STRATEGY]  One of tailwind, vanilla-extract, css-modules, postcss [env: SHOPIFY_HYDROGEN_ARG_STRATEGY=]\n\nOptions:\n      --install-deps     Auto installs dependencies using the active package manager [env: SHOPIFY_HYDROGEN_FLAG_INSTALL_DEPS=]\n      --path <PATH>      Path to the Hydrogen storefront [env: SHOPIFY_HYDROGEN_FLAG_PATH=]\n  -f, --force            Overwrite destination files [env: SHOPIFY_HYDROGEN_FLAG_FORCE=]\n  -h, --help             Print help"
    );
}

fn prompt_strategy(kind: &str, choices: &[&str], default: Option<&str>) -> Result<String> {
    use std::io::{self, IsTerminal, Write};
    if !io::stdin().is_terminal() {
        return Err(Error::invalid_input(format!(
            "{kind} strategy is required in non-interactive mode; choose one of {}",
            choices.join(", ")
        )));
    }
    let default = default.unwrap_or(choices[0]);
    print!(
        "Select a {kind} strategy ({}; default {default}): ",
        choices.join(", ")
    );
    io::stdout().flush().map_err(|error| {
        Error::with_source(ErrorKind::Process, "could not render prompt", error)
    })?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|error| Error::with_source(ErrorKind::Process, "could not read prompt", error))?;
    let answer = answer.trim();
    let answer = if answer.is_empty() { default } else { answer };
    validate_strategy(answer, choices, kind)?;
    Ok(answer.to_owned())
}

fn app_directory(root: &Path) -> PathBuf {
    // The upstream Vite config stores this as a literal in remix({...}). Keeping
    // this deliberately conservative prevents a source-file parser from
    // guessing a custom path incorrectly.
    root.join("app")
}

fn first_file(base: &Path, stems: &[&str], extensions: &[&str]) -> Option<PathBuf> {
    stems
        .iter()
        .flat_map(|stem| {
            extensions
                .iter()
                .map(move |extension| base.join(format!("{stem}.{extension}")))
        })
        .find(|path| path.is_file())
}

fn context_file(root: &Path) -> Result<PathBuf> {
    let app = app_directory(root);
    first_file(&app.join("lib"), &["context"], &["ts", "tsx", "js", "jsx"]).ok_or_else(|| {
        Error::config(format!(
            "could not find a Hydrogen context file at {}",
            app.join("lib/context.ts").display()
        ))
    })
}

fn root_file(root: &Path) -> Result<PathBuf> {
    let app = app_directory(root);
    first_file(&app, &["root"], &["tsx", "ts", "jsx", "js"]).ok_or_else(|| {
        Error::config(format!(
            "could not find a route root file under {}",
            app.display()
        ))
    })
}

fn merge_package_json(root: &Path, asset: &[u8], remove: &[&str]) -> Result<()> {
    let package_path = root.join("package.json");
    let existing = fs::read_to_string(&package_path).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            format!("could not read {}", package_path.display()),
            error,
        )
    })?;
    let mut package: Map<String, Value> = serde_json::from_str(&existing).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            format!("could not parse {}", package_path.display()),
            error,
        )
    })?;
    let additions: Value = serde_json::from_slice(asset).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            "could not parse official Hydrogen package asset",
            error,
        )
    })?;
    for section in ["dependencies", "devDependencies", "scripts"] {
        let Some(additions) = additions.get(section).and_then(Value::as_object) else {
            continue;
        };
        let target = package
            .entry(section)
            .or_insert_with(|| Value::Object(Map::new()));
        let target = target
            .as_object_mut()
            .ok_or_else(|| Error::config(format!("package.json {section} must be an object")))?;
        for (name, value) in additions {
            target.entry(name.clone()).or_insert_with(|| value.clone());
        }
    }
    for section in ["dependencies", "devDependencies"] {
        if let Some(values) = package.get_mut(section).and_then(Value::as_object_mut) {
            for name in remove {
                values.remove(*name);
            }
        }
    }
    let contents = serde_json::to_vec_pretty(&Value::Object(package)).map_err(|error| {
        Error::with_source(ErrorKind::Config, "could not serialize package.json", error)
    })?;
    write_generated_file(&package_path, &contents)
}

fn insert_after_imports(contents: &str, line: &str) -> String {
    let mut offset = 0;
    for segment in contents.split_inclusive('\n') {
        if segment.trim_start().starts_with("import ") {
            offset += segment.len();
        } else {
            break;
        }
    }
    format!("{}{}\n{}", &contents[..offset], line, &contents[offset..])
}

fn rewrite_context_for_i18n(contents: &str) -> Result<String> {
    if contents.contains("getLocaleFromRequest(") {
        return Err(Error::config(
            "an i18n strategy is already set up in app/lib/context",
        ));
    }
    let context = if contents.contains("from \"./i18n\"") || contents.contains("from './i18n'") {
        contents.to_owned()
    } else {
        insert_after_imports(contents, "import {getLocaleFromRequest} from './i18n';")
    };
    let Some(start) = context.find("createHydrogenContext(") else {
        return Err(Error::config(
            "could not find createHydrogenContext(...) in app/lib/context",
        ));
    };
    let after_call = &context[start..];
    let Some(open_relative) = after_call.find('{') else {
        return Err(Error::config(
            "createHydrogenContext must receive an inline object",
        ));
    };
    let open = start + open_relative;
    let mut depth = 0usize;
    let mut close = None;
    for (relative, character) in context[open..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + relative);
                    break;
                }
            }
            _ => {}
        }
    }
    let close =
        close.ok_or_else(|| Error::config("could not parse createHydrogenContext options"))?;
    let object = &context[open + 1..close];
    let replacement = if let Some(position) = object.find("i18n:") {
        let value_start = position + "i18n:".len();
        let value_end = object[value_start..]
            .find(',')
            .map(|value| value_start + value)
            .unwrap_or(object.len());
        format!(
            "{} i18n: getLocaleFromRequest(request){}",
            &object[..position],
            &object[value_end..]
        )
    } else if object.trim().is_empty() {
        "i18n: getLocaleFromRequest(request)".to_owned()
    } else {
        format!("{object},\n    i18n: getLocaleFromRequest(request)")
    };
    Ok(format!(
        "{}{{{}}}{}",
        &context[..open],
        replacement,
        &context[close + 1..]
    ))
}

pub(crate) fn run_setup_markets(options: &SetupMarketsOptions) -> Result<()> {
    let strategy = match &options.strategy {
        Some(strategy) => strategy.clone(),
        None => prompt_strategy("markets", I18N_STRATEGIES, None)?,
    };
    let source = resolve_template_source(&options.path)?;
    let asset = source.read(Path::new(&format!("assets/i18n/{strategy}.ts")))?;
    let context = context_file(&options.path)?;
    let is_js = matches!(
        context.extension().and_then(|value| value.to_str()),
        Some("js" | "jsx")
    );
    let i18n = if is_js {
        transpile_typescript(&asset, Path::new("i18n.ts"))?
    } else {
        asset
    };
    let i18n = String::from_utf8(i18n)
        .map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                "official Hydrogen i18n asset is not UTF-8",
                error,
            )
        })?
        .replace("'./mock-i18n-types.js'", "'@shopify/hydrogen'")
        .replace("\"./mock-i18n-types.js\"", "\"@shopify/hydrogen\"");
    let destination = context.with_file_name(if is_js { "i18n.js" } else { "i18n.ts" });
    if destination.exists() {
        return Err(Error::config(format!(
            "{} already exists; rename or remove it before setting up markets",
            destination.display()
        )));
    }
    let existing = fs::read_to_string(&context).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            format!("could not read {}", context.display()),
            error,
        )
    })?;
    let rewritten = rewrite_context_for_i18n(&existing)?;
    write_generated_file(&destination, i18n.as_bytes())?;
    write_generated_file(&context, rewritten.as_bytes())?;
    println!("Markets support setup complete with strategy {strategy}.");
    Ok(())
}

fn inject_vite_plugin(contents: &str, import: &str, invocation: &str) -> Result<String> {
    let contents = if contents.contains(import) {
        contents.to_owned()
    } else {
        insert_after_imports(contents, import)
    };
    if contents.contains(invocation) {
        return Ok(contents);
    }
    let Some(plugins) = contents.find("plugins:") else {
        return Err(Error::config("could not find plugins in vite.config"));
    };
    let Some(open_relative) = contents[plugins..].find('[') else {
        return Err(Error::config("vite.config plugins must be an array"));
    };
    let insert = plugins + open_relative + 1;
    Ok(format!(
        "{}\n    {invocation},{}",
        &contents[..insert],
        &contents[insert..]
    ))
}

fn add_tailwind_root_link(contents: &str) -> Result<String> {
    let contents = if contents.contains("tailwind.css?url") {
        contents.to_owned()
    } else {
        insert_after_imports(
            contents,
            "import tailwindCss from './styles/tailwind.css?url';",
        )
    };
    if contents.contains("href: tailwindCss") {
        return Ok(contents);
    }
    let Some(links) = contents.find("links") else {
        return Err(Error::config(
            "could not find a links export in app/root; add the Tailwind stylesheet manually",
        ));
    };
    let Some(array_relative) = contents[links..].find('[') else {
        return Err(Error::config("app/root links export must return an array"));
    };
    let insert = links + array_relative + 1;
    Ok(format!(
        "{}\n    {{rel: 'stylesheet', href: tailwindCss}},{}",
        &contents[..insert],
        &contents[insert..]
    ))
}

fn selected_css_strategy(strategy: Option<String>) -> Result<String> {
    match strategy {
        Some(value) => Ok(value),
        None => prompt_strategy("CSS", CSS_STRATEGIES, Some("tailwind")),
    }
}

pub(crate) fn run_setup_css(options: &SetupCssOptions) -> Result<()> {
    if !has_vite_config(&options.path) {
        return Err(Error::config(
            "No Vite config found. This command is only supported in Vite projects.",
        ));
    }
    let strategy = selected_css_strategy(options.strategy.clone())?;
    if matches!(strategy.as_str(), "css-modules" | "postcss") {
        println!("Vite works out of the box with {strategy}.");
        return Ok(());
    }
    let source = resolve_template_source(&options.path)?;
    let vite = first_file(
        &options.path,
        &["vite.config"],
        &["ts", "tsx", "js", "jsx", "mjs", "cjs"],
    )
    .ok_or_else(|| Error::config("could not find vite.config"))?;
    let vite_contents = fs::read_to_string(&vite).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            format!("could not read {}", vite.display()),
            error,
        )
    })?;
    match strategy.as_str() {
        "tailwind" => {
            let stylesheet = app_directory(&options.path).join("styles/tailwind.css");
            if stylesheet.exists() && !options.force {
                return Err(Error::config(format!(
                    "{} already exists; use --force to overwrite it",
                    stylesheet.display()
                )));
            }
            merge_package_json(
                &options.path,
                &source.read(Path::new("assets/tailwind/package.json"))?,
                &[],
            )?;
            let vite_contents = inject_vite_plugin(
                &vite_contents,
                "import tailwindcss from '@tailwindcss/vite';",
                "tailwindcss()",
            )?;
            let root = root_file(&options.path)?;
            let root_contents = fs::read_to_string(&root).map_err(|error| {
                Error::with_source(
                    ErrorKind::Config,
                    format!("could not read {}", root.display()),
                    error,
                )
            })?;
            write_generated_file(
                &stylesheet,
                &source.read(Path::new("assets/tailwind/tailwind.css"))?,
            )?;
            write_generated_file(&vite, vite_contents.as_bytes())?;
            write_generated_file(&root, add_tailwind_root_link(&root_contents)?.as_bytes())?;
        }
        "vanilla-extract" => {
            merge_package_json(
                &options.path,
                &source.read(Path::new("assets/vanilla-extract/package.json"))?,
                &[],
            )?;
            let vite_contents = inject_vite_plugin(
                &vite_contents,
                "import {vanillaExtractPlugin} from '@vanilla-extract/vite-plugin';",
                "vanillaExtractPlugin()",
            )?;
            write_generated_file(&vite, vite_contents.as_bytes())?;
        }
        _ => unreachable!(),
    }
    if options.install_deps {
        eprintln!(
            "Dependencies were added to package.json. Run your project package manager to install them."
        );
    }
    println!("{strategy} setup complete.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_setup_flags() {
        assert!(
            matches!(parse_setup_markets(&["subfolders".into()]).unwrap(), NativeCommand::SetupMarkets(SetupMarketsOptions { strategy: Some(strategy), .. }) if strategy == "subfolders")
        );
        assert!(matches!(
            parse_setup_css(&["tailwind".into(), "--no-install-deps".into()]).unwrap(),
            NativeCommand::SetupCss(SetupCssOptions {
                install_deps: false,
                ..
            })
        ));
    }

    #[test]
    fn rewrites_context_with_i18n() {
        let input = "import {createHydrogenContext} from '@shopify/hydrogen';\nexport function createAppLoadContext(request) { return createHydrogenContext({env: {}}); }\n";
        let output = rewrite_context_for_i18n(input).unwrap();
        assert!(output.contains("getLocaleFromRequest"));
        assert!(output.contains("i18n: getLocaleFromRequest(request)"));
    }

    #[test]
    fn injects_css_into_vite_and_root() {
        let vite = inject_vite_plugin(
            "import {defineConfig} from 'vite';\nexport default defineConfig({plugins: []});",
            "import tailwindcss from '@tailwindcss/vite';",
            "tailwindcss()",
        )
        .unwrap();
        assert!(vite.contains("tailwindcss()"));
        let root = add_tailwind_root_link("export function links() { return []; }").unwrap();
        assert!(root.contains("href: tailwindCss"));
    }
}
