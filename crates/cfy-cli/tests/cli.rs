use std::process::{Command, Output};

fn cfy(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args(args)
        .output()
        .expect("cfy should execute")
}

#[test]
fn theme_init_and_upgrade_match_shopify_public_command_shapes() {
    let theme_help = cfy(&["theme", "init", "--help"]);
    assert!(theme_help.status.success());
    let theme_help = String::from_utf8_lossy(&theme_help.stdout);
    assert!(theme_help.contains("Usage: cfy theme init [OPTIONS] [NAME]"));
    for flag in ["--path", "--clone-url", "--latest"] {
        assert!(theme_help.contains(flag), "missing theme init flag {flag}");
    }
    assert!(!theme_help.contains("--destination"));

    let upgrade_help = cfy(&["upgrade", "--help"]);
    assert!(upgrade_help.status.success());
    let upgrade_help = String::from_utf8_lossy(&upgrade_help.stdout);
    assert!(upgrade_help.contains("Usage: cfy upgrade"));
    assert!(!upgrade_help.contains("--dry-run"));
}

#[test]
fn app_info_matches_shopify_flags_and_reports_project_structure() {
    let root = std::env::temp_dir().join(format!(
        "cfy-app-info-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(root.join("extensions/sample")).unwrap();
    std::fs::create_dir_all(root.join("web")).unwrap();
    std::fs::write(
        root.join("shopify.app.toml"),
        r#"name = "Fixture app"
client_id = "fixture-client"
application_url = "https://example.test"
embedded = true

[access_scopes]
scopes = "read_products,write_products"
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("extensions/sample/shopify.extension.toml"),
        "name = \"Sample extension\"\nhandle = \"sample\"\ntype = \"theme_app_extension\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("web/shopify.web.toml"),
        "name = \"Web\"\nroles = [\"frontend\"]\n",
    )
    .unwrap();
    std::fs::write(root.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args(["--json", "app", "info", "--path"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["app"]["name"], "Fixture app");
    assert_eq!(report["extensions"].as_array().unwrap().len(), 1);
    assert_eq!(report["webs"].as_array().unwrap().len(), 1);
    assert_eq!(report["system"]["package_manager"], "pnpm");

    let web_env = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args(["app", "info", "--web-env", "--path"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(web_env.status.success());
    let web_env = String::from_utf8_lossy(&web_env.stdout);
    assert!(web_env.contains("SHOPIFY_API_KEY=fixture-client"));
    assert!(web_env.contains("SHOPIFY_APP_URL=https://example.test"));

    let help = cfy(&["app", "info", "--help"]);
    let help = String::from_utf8_lossy(&help.stdout);
    for flag in [
        "--auth-alias",
        "--client-id",
        "--config",
        "--path",
        "--reset",
        "--web-env",
    ] {
        assert!(help.contains(flag), "missing app info flag {flag}");
    }

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn theme_open_matches_shopify_flags_and_selection_guard() {
    let help = cfy(&["theme", "open", "--help"]);
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    for flag in [
        "--auth-alias",
        "--development",
        "--editor",
        "--environment",
        "--live",
        "--password",
        "--path",
        "--store",
        "--theme",
    ] {
        assert!(help.contains(flag), "missing theme open flag {flag}");
    }
    assert!(!help.contains("--version"));
    assert_eq!(
        cfy(&[
            "theme",
            "open",
            "--development",
            "--live",
            "--store",
            "demo",
            "--password",
            "secret",
        ])
        .status
        .code(),
        Some(2)
    );
}

#[test]
fn auth_logout_matches_shopify_local_session_command_contract() {
    let help = cfy(&["auth", "logout", "--help"]);
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("Usage: cfy auth logout"));
    assert!(!help.contains("--identity"));
    assert_eq!(
        cfy(&["auth", "logout", "--identity", "other"])
            .status
            .code(),
        Some(2)
    );
}

#[test]
fn commands_lists_the_complete_embedded_runtime_inventory() {
    let output = cfy(&["--json", "commands"]);
    assert!(output.status.success());
    let commands: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let commands = commands.as_array().unwrap();
    assert_eq!(commands.len(), 111);
    let build = commands
        .iter()
        .find(|command| command["name"] == "app build")
        .unwrap();
    assert_eq!(build["plugin_name"], "@shopify/cli");
    assert!(
        build["flags"]
            .as_array()
            .unwrap()
            .iter()
            .any(|flag| flag["name"] == "config")
    );
}

#[test]
fn commands_supports_shopify_compatible_columns_sort_and_tree() {
    let help = cfy(&["commands", "--help"]);
    let help = String::from_utf8_lossy(&help.stdout);
    for flag in [
        "--columns",
        "--extended",
        "--deprecated",
        "--hidden",
        "--no-truncate",
        "--sort",
        "--tree",
    ] {
        assert!(help.contains(flag), "missing commands flag {flag}");
    }

    let columns = cfy(&["commands", "--columns", "id,type", "--sort", "type"]);
    assert!(columns.status.success());
    let columns = String::from_utf8_lossy(&columns.stdout);
    assert!(columns.starts_with("Id\tType\n"));
    assert!(columns.contains("app build\tcore"));

    let tree = cfy(&["commands", "--tree"]);
    assert!(tree.status.success());
    let tree = String::from_utf8_lossy(&tree.stdout);
    assert!(tree.contains("  build\tBuild the app, including extensions."));
    assert!(tree.contains("    link\tFetch your app configuration"));
}

#[test]
fn plugins_use_exact_public_paths_and_shopify_compatible_flags() {
    for command in [
        "add",
        "install",
        "inspect",
        "link",
        "remove",
        "reset",
        "uninstall",
        "unlink",
        "update",
    ] {
        let output = cfy(&["plugins", command, "--help"]);
        assert!(output.status.success(), "plugins {command} help failed");
        let help = String::from_utf8_lossy(&output.stdout);
        assert!(help.contains(&format!("Usage: cfy plugins {command}")));
        assert!(help.contains("--verbose"));
        assert!(help.contains("-v"));
    }

    for command in ["add", "install"] {
        let help = cfy(&["plugins", command, "--help"]);
        let help = String::from_utf8_lossy(&help.stdout);
        assert!(help.contains("<PLUGIN>..."));
        assert!(help.contains("--force"));
        assert!(help.contains("-f"));
        assert!(help.contains("--silent"));
        assert!(help.contains("-s"));
        assert_eq!(
            cfy(&["plugins", command, "example", "--silent", "--verbose"])
                .status
                .code(),
            Some(2)
        );
    }

    let inspect = cfy(&["plugins", "inspect", "--help"]);
    assert!(String::from_utf8_lossy(&inspect.stdout).contains("[PLUGIN]..."));
    let link = cfy(&["plugins", "link", "--help"]);
    let link = String::from_utf8_lossy(&link.stdout);
    assert!(link.contains("[PATH]"));
    assert!(link.contains("--install"));
    assert!(link.contains("--no-install"));
    assert_eq!(
        cfy(&["plugins", "link", "--install", "--no-install"])
            .status
            .code(),
        Some(2)
    );
    let reset = cfy(&["plugins", "reset", "--help"]);
    let reset = String::from_utf8_lossy(&reset.stdout);
    assert!(reset.contains("--hard"));
    assert!(reset.contains("--reinstall"));

    for command in ["remove", "uninstall", "unlink"] {
        let help = cfy(&["plugins", command, "--help"]);
        assert!(String::from_utf8_lossy(&help.stdout).contains("[PLUGIN]..."));
    }
    assert_eq!(cfy(&["plugin", "inspect", "--help"]).status.code(), Some(2));
    assert_eq!(cfy(&["plugins-inspect", "--help"]).status.code(), Some(2));
}

#[cfg(unix)]
fn fake_package_manager(root: &std::path::Path, exit_code: i32) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let executable = root.join("fake-npm");
    std::fs::write(
        &executable,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$@" >> "{}"
prefix=''
previous=''
for argument in "$@"; do
  if [ "$previous" = '--prefix' ]; then prefix="$argument"; fi
  previous="$argument"
done
if [ "$1" = install ] && [ -n "$prefix" ] && [ "$#" -ge 5 ]; then
  source=''
  for argument in "$@"; do source="$argument"; done
  name="${{source%%@*}}"
  mkdir -p "$prefix/node_modules/$name"
  printf '{{"name":"%s","version":"1.0.0"}}\n' "$name" > "$prefix/node_modules/$name/package.json"
fi
exit {exit_code}
"#,
            root.join("calls.log").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    executable
}

#[cfg(unix)]
#[test]
fn plugins_inspect_link_and_reset_use_native_registry_and_package_manager() {
    let root = std::env::temp_dir().join(format!(
        "cfy-plugins-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let registry = root.join("registry");
    let plugin = root.join("linked-plugin");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(
        plugin.join("package.json"),
        r#"{"name":"linked-plugin","version":"2.0.0"}"#,
    )
    .unwrap();
    let package_manager = fake_package_manager(&root, 0);

    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_cfy"))
            .args(args)
            .env("CFY_PLUGIN_ROOT", &registry)
            .env("CFY_PACKAGE_MANAGER", &package_manager)
            .output()
            .unwrap()
    };

    let linked = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args(["--json", "plugins", "link"])
        .arg(&plugin)
        .arg("--no-install")
        .env("CFY_PLUGIN_ROOT", &registry)
        .env("CFY_PACKAGE_MANAGER", &package_manager)
        .output()
        .unwrap();
    assert!(linked.status.success());
    let linked: serde_json::Value = serde_json::from_slice(&linked.stdout).unwrap();
    assert_eq!(linked["plugin"]["name"], "linked-plugin");
    assert_eq!(linked["plugin"]["kind"], "linked");

    let inspected = run(&["--json", "plugins", "inspect", "linked-plugin"]);
    assert!(inspected.status.success());
    let inspected: serde_json::Value = serde_json::from_slice(&inspected.stdout).unwrap();
    assert_eq!(inspected[0]["version"], "2.0.0");

    let installed = run(&["--json", "plugins", "add", "installed-plugin"]);
    assert!(installed.status.success());
    let installed: serde_json::Value = serde_json::from_slice(&installed.stdout).unwrap();
    assert_eq!(installed[0]["action"], "install");
    assert_eq!(installed[0]["plugin"]["name"], "installed-plugin");

    let reset = run(&["--json", "plugins", "reset", "--hard", "--reinstall"]);
    assert!(reset.status.success());
    let reset: serde_json::Value = serde_json::from_slice(&reset.stdout).unwrap();
    assert_eq!(reset["removed_registry"], true);
    assert_eq!(reset["removed_artifacts"], true);
    assert_eq!(
        reset["reinstalled"][0]["plugin"]["name"],
        "installed-plugin"
    );
    assert!(root.join("calls.log").exists());

    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn plugins_propagate_package_manager_nonzero_exit() {
    let root = std::env::temp_dir().join(format!(
        "cfy-plugins-exit-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let package_manager = fake_package_manager(&root, 7);
    let output = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args(["--json", "plugins", "install", "broken-plugin"])
        .env("CFY_PLUGIN_ROOT", root.join("registry"))
        .env("CFY_PACKAGE_MANAGER", package_manager)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value[0]["process"]["exit_code"], 7);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn config_autocorrect_commands_persist_exact_native_state() {
    let root = std::env::temp_dir().join(format!(
        "catify-autocorrect-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let config = root.join("config.toml");

    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_cfy"))
            .args(args)
            .env("CFY_CONFIG_FILE", &config)
            .output()
            .expect("cfy should execute")
    };

    let status = run(&["--json", "config", "autocorrect", "status"]);
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("\"autocorrect\":false"));

    assert!(run(&["config", "autocorrect", "on"]).status.success());
    let enabled = run(&["--json", "config", "autocorrect", "status"]);
    assert!(String::from_utf8_lossy(&enabled.stdout).contains("\"autocorrect\":true"));

    assert!(run(&["config", "autocorrect", "off"]).status.success());
    let contents = std::fs::read_to_string(&config).unwrap();
    assert!(contents.contains("autocorrect = false"));
    assert!(contents.contains("autoupgrade = true"));

    assert!(run(&["config", "autocorrect", "on"]).status.success());
    let corrected = run(&["versoin"]);
    assert!(corrected.status.success());
    assert!(String::from_utf8_lossy(&corrected.stderr).contains("Autocorrected command"));

    assert!(run(&["config", "autocorrect", "off"]).status.success());
    assert!(!run(&["versoin"]).status.success());

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn theme_metafields_uses_shopify_compatible_nested_path() {
    assert!(
        cfy(&[
            "theme",
            "metafields",
            "pull",
            "--store",
            "shop.myshopify.com",
            "--environment",
            "development",
            "--help",
        ])
        .status
        .success()
    );
    assert!(
        !cfy(&["theme", "metafields-pull", "--help"])
            .status
            .success()
    );
}

#[test]
fn store_nested_commands_match_shopify_paths_and_flags() {
    for args in [
        vec!["store", "auth", "list", "--help"],
        vec![
            "store",
            "bulk",
            "execute",
            "--store",
            "shop.myshopify.com",
            "--query",
            "query { shop { id } }",
            "--version",
            "2026-07",
            "--help",
        ],
        vec![
            "store",
            "bulk",
            "status",
            "--store",
            "shop.myshopify.com",
            "--id",
            "123",
            "--help",
        ],
        vec![
            "store",
            "bulk",
            "cancel",
            "--store",
            "shop.myshopify.com",
            "--id",
            "123",
            "--help",
        ],
        vec![
            "store",
            "create",
            "preview",
            "--name",
            "Preview",
            "--country",
            "US",
            "--help",
        ],
    ] {
        assert!(cfy(&args).status.success(), "failed command: {args:?}");
    }
    for old in [
        "auth-list",
        "bulk-execute",
        "bulk-status",
        "bulk-cancel",
        "create-preview",
    ] {
        assert!(!cfy(&["store", old, "--help"]).status.success());
    }
}

#[test]
fn organization_list_uses_shopify_compatible_command_and_flags() {
    assert!(
        cfy(&[
            "organization",
            "list",
            "--auth-alias",
            "work",
            "--json",
            "--help",
        ])
        .status
        .success()
    );
}

#[test]
fn app_dev_runs_declared_web_command_natively_and_cleans_state() {
    let fixture = std::env::temp_dir().join(format!(
        "cfy-dev-fixture-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let web = fixture.join("web");
    std::fs::create_dir_all(&web).unwrap();
    std::fs::write(
        fixture.join("shopify.app.toml"),
        "client_id='client'\nname='app'\n[web_directories]\ndirectories=['web']\n",
    )
    .unwrap();
    std::fs::write(
        web.join("shopify.web.toml"),
        "name='web'\nroles=['frontend']\n[commands]\ndev='printf ready > dev-ready.txt'\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .current_dir(&fixture)
        .args(["app", "dev", "--use-localhost"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(web.join("dev-ready.txt")).unwrap(),
        "ready"
    );

    let state = fixture.join(".catify/dev");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("session.json"), "{}").unwrap();
    let clean = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .current_dir(&fixture)
        .args(["app", "dev", "clean"])
        .output()
        .unwrap();
    assert!(clean.status.success());
    assert!(!state.exists());

    std::fs::remove_dir_all(&fixture).unwrap();
}

#[test]
fn app_dev_requires_explicit_localhost_until_tunnel_is_wired() {
    let fixture = std::env::temp_dir().join(format!(
        "cfy-dev-url-fixture-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let web = fixture.join("web");
    std::fs::create_dir_all(&web).unwrap();
    std::fs::write(
        fixture.join("shopify.app.toml"),
        "client_id='client'\nname='app'\n[web_directories]\ndirectories=['web']\n",
    )
    .unwrap();
    std::fs::write(
        web.join("shopify.web.toml"),
        "name='web'\nroles=['frontend']\n[commands]\ndev='exit 0'\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .current_dir(&fixture)
        .args(["app", "dev", "--tunnel-url", "http://example.test"])
        .output()
        .unwrap();
    std::fs::remove_dir_all(&fixture).unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("must use HTTPS"));
}

#[cfg(unix)]
#[test]
fn app_dev_starts_and_cleans_up_cloudflared_tunnel() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = std::env::temp_dir().join(format!(
        "cfy-dev-tunnel-fixture-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let web = fixture.join("web");
    std::fs::create_dir_all(&web).unwrap();
    std::fs::write(
        fixture.join("shopify.app.toml"),
        "client_id='client'\nname='app'\n[web_directories]\ndirectories=['web']\n",
    )
    .unwrap();
    std::fs::write(
        web.join("shopify.web.toml"),
        "name='web'\nroles=['frontend']\n[commands]\ndev='exit 0'\n",
    )
    .unwrap();
    let cloudflared = fixture.join("cloudflared");
    std::fs::write(
        &cloudflared,
        "#!/bin/sh\necho 'ready https://fixture.trycloudflare.com' >&2\nsleep 30\n",
    )
    .unwrap();
    std::fs::set_permissions(&cloudflared, std::fs::Permissions::from_mode(0o755)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .current_dir(&fixture)
        .env("CFY_CLOUDFLARED_BIN", &cloudflared)
        .args(["app", "dev"])
        .output()
        .unwrap();
    std::fs::remove_dir_all(&fixture).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("https://fixture.trycloudflare.com/"));
}

#[test]
fn app_build_uses_shopify_compatible_command_and_flags() {
    assert!(
        cfy(&[
            "app",
            "build",
            "--config",
            "staging",
            "--auth-alias",
            "work",
            "--client-id",
            "client",
            "--path",
            ".",
            "--reset",
            "--skip-dependencies-installation",
            "--help",
        ])
        .status
        .success()
    );
}

#[test]
fn app_build_without_extensions_runs_natively() {
    let fixture = std::env::temp_dir().join(format!(
        "cfy-build-fixture-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(
        fixture.join("shopify.app.toml"),
        "client_id='client'\nname='app'\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .current_dir(&fixture)
        .args(["--json", "app", "build"])
        .output()
        .unwrap();
    std::fs::remove_dir_all(&fixture).unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("\"mode\":\"incremental\""));
}

fn cfy_outside_project(args: &[&str]) -> Output {
    let directory = std::env::temp_dir().join(format!(
        "cfy-no-project-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args(args)
        .current_dir(&directory)
        .output()
        .expect("cfy should execute");
    std::fs::remove_dir_all(directory).unwrap();
    output
}

#[test]
fn app_release_requires_explicit_non_interactive_policy() {
    let output = cfy(&["app", "release", "--version", "1", "--non-interactive"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--allow-updates"));
}

#[test]
fn app_versions_uses_shopify_compatible_nested_command_name() {
    assert!(cfy(&["app", "versions", "list", "--help"]).status.success());
    assert!(!cfy(&["app", "versions-list"]).status.success());
}

#[test]
fn app_env_uses_shopify_compatible_nested_command_names() {
    assert!(cfy(&["app", "env", "show", "--help"]).status.success());
    assert!(cfy(&["app", "env", "pull", "--help"]).status.success());
    assert!(!cfy(&["app", "env-pull"]).status.success());
}

#[test]
fn app_config_validate_reports_selected_config_diagnostics() {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    let root = std::env::temp_dir().join(format!(
        "cfy-config-validate-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("shopify.app.toml"), "client_id = \"valid\"\n").unwrap();
    fs::write(root.join("shopify.app.broken.toml"), "client_id = [\n").unwrap();

    let valid = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args([
            "app",
            "config",
            "validate",
            "--config",
            "shopify.app.toml",
            "--path",
        ])
        .arg(&root)
        .arg("--json")
        .output()
        .unwrap();
    assert!(valid.status.success());
    let valid: serde_json::Value = serde_json::from_slice(&valid.stdout).unwrap();
    assert_eq!(valid["valid"], true);

    let broken = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args(["app", "config", "validate", "--config", "broken", "--path"])
        .arg(&root)
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(broken.status.code(), Some(1));
    let broken: serde_json::Value = serde_json::from_slice(&broken.stdout).unwrap();
    assert_eq!(broken["valid"], false);
    assert_eq!(broken["errors"], 1);
    assert!(
        broken["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("malformed TOML")
    );
}

#[test]
fn app_config_use_persists_selection_and_reset_restores_default() {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    let root = std::env::temp_dir().join(format!(
        "cfy-config-use-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("shopify.app.toml"),
        "client_id = \"default-key\"\napplication_url = \"https://default.example\"\n",
    )
    .unwrap();
    fs::write(
        root.join("shopify.app.staging.toml"),
        "client_id = \"staging-key\"\napplication_url = \"https://staging.example\"\n",
    )
    .unwrap();
    let state = root.join("state.json");

    let selected = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args(["app", "config", "use", "staging", "--path"])
        .arg(&root)
        .arg("--json")
        .env("CFY_APP_STATE_FILE", &state)
        .output()
        .unwrap();
    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );

    let active = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args(["app", "env", "show", "--json"])
        .current_dir(&root)
        .env("CFY_APP_STATE_FILE", &state)
        .output()
        .unwrap();
    let active: serde_json::Value = serde_json::from_slice(&active.stdout).unwrap();
    assert_eq!(active["config"], "staging");
    assert_eq!(active["values"]["SHOPIFY_API_KEY"], "[REDACTED]");

    let reset = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args(["app", "config", "use", "--reset", "--path"])
        .arg(&root)
        .env("CFY_APP_STATE_FILE", &state)
        .output()
        .unwrap();
    assert!(reset.status.success());

    let default = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args(["app", "env", "show", "--json"])
        .current_dir(&root)
        .env("CFY_APP_STATE_FILE", &state)
        .output()
        .unwrap();
    let default: serde_json::Value = serde_json::from_slice(&default.stdout).unwrap();
    assert_eq!(default["config"], "default");
    assert_eq!(default["values"]["SHOPIFY_API_KEY"], "[REDACTED]");
}

#[cfg(unix)]
#[test]
fn app_config_link_forwards_the_exact_shopify_command_path_and_flags() {
    let executable = fake_shopify("#!/bin/sh\nprintf '%s\\n' \"$@\"\n");
    let output = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args([
            "app",
            "config",
            "link",
            "--config",
            "staging",
            "--client-id",
            "client-123",
            "--file-name",
            "shopify.app.staging.toml",
            "--force",
            "--path",
            "/tmp/example",
            "--reset",
            "--delegate",
        ])
        .env("CFY_SHOPIFY_BIN", executable)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        concat!(
            "app\nconfig\nlink\n--config\nstaging\n--client-id\nclient-123\n",
            "--file-name\nshopify.app.staging.toml\n--force\n--path\n/tmp/example\n--reset\n"
        )
    );
}

#[test]
fn app_config_uses_shopify_compatible_nested_command_names() {
    let nested = cfy(&["app", "config", "link", "--help"]);
    assert!(nested.status.success());
    let nested_help = String::from_utf8_lossy(&nested.stdout);
    assert!(nested_help.contains("Usage: cfy app config link"));

    let pull = cfy(&["app", "config", "pull", "--help"]);
    assert!(pull.status.success());
    let pull_help = String::from_utf8_lossy(&pull.stdout);
    assert!(pull_help.contains("Usage: cfy app config pull"));
    for flag in [
        "--config",
        "--auth-alias",
        "--client-id",
        "--path",
        "--reset",
    ] {
        assert!(pull_help.contains(flag), "missing {flag}");
    }
    assert!(nested_help.contains("--client-id"));
    assert!(nested_help.contains("--file-name"));

    let flat = cfy(&["app", "config-link"]);
    assert_eq!(flat.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&flat.stderr).contains("unrecognized subcommand"));
}

#[cfg(unix)]
#[test]
fn auth_login_can_explicitly_delegate_to_official_shopify_cli() {
    let executable = fake_shopify(
        "#!/bin/sh\n[ \"$1 $2\" = \"auth login\" ] || exit 9\nprintf 'delegated-login\\n'\nexit 0\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args(["auth", "login", "--delegate"])
        .env_remove("CFY_IDENTITY_CLIENT_ID")
        .env("CFY_SHOPIFY_BIN", executable)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("delegated-login"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Delegating authentication to the official Shopify CLI"));
    assert!(stderr.contains("session remains managed by Shopify CLI"));
}

#[cfg(unix)]
fn fake_shopify(script: &str) -> std::path::PathBuf {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        time::{SystemTime, UNIX_EPOCH},
    };
    let directory = std::env::temp_dir().join(format!(
        "cfy-theme-check-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&directory).unwrap();
    let executable = directory.join("shopify");
    fs::write(&executable, script).unwrap();
    let mut permissions = fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&executable, permissions).unwrap();
    executable
}

#[cfg(unix)]
#[test]
fn theme_check_preserves_streams_and_child_exit_code() {
    let executable = fake_shopify(
        "#!/bin/sh\nif [ \"$1\" = version ]; then echo 3.90.0; exit 0; fi\nprintf 'theme-json\\n'\nprintf 'theme-warning\\n' >&2\nexit 7\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args([
            "theme",
            "check",
            "--path",
            "tests/fixtures/theme-check/failing",
            "--output",
            "json",
        ])
        .env("CFY_THEME_CHECK_BIN", executable)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "theme-json\n");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "theme-warning\n");
}

#[cfg(unix)]
#[test]
fn theme_check_version_mismatch_is_actionable() {
    let executable = fake_shopify("#!/bin/sh\necho 1.0.0\n");
    let output = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args(["theme", "check"])
        .env("CFY_THEME_CHECK_BIN", executable)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unsupported Theme Check adapter version"));
    assert!(stderr.contains("CFY_THEME_CHECK_BIN"));
}

#[test]
fn theme_check_missing_dependency_is_actionable() {
    let output = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args(["theme", "check"])
        .env(
            "CFY_THEME_CHECK_BIN",
            "cfy-definitely-missing-theme-check-adapter",
        )
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("npm install -g @shopify/cli @shopify/theme"));
    assert!(stderr.contains("CFY_THEME_CHECK_BIN"));
}

#[test]
fn json_runtime_errors_leave_stdout_empty() {
    let output = cfy_outside_project(&["app", "info", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());

    let diagnostic: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("stderr should contain one JSON diagnostic");
    assert_eq!(diagnostic["error"]["code"], "invalid_input");
}

#[test]
fn verbose_diagnostics_are_explicit_and_stay_out_of_stdout() {
    let output = cfy(&["version", "--json", "--verbose"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        concat!(
            "{\"name\":\"cfy\",\"version\":\"",
            env!("CARGO_PKG_VERSION"),
            "\"}\n"
        )
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "debug: debug diagnostics enabled\n"
    );
}

#[test]
fn root_help_is_successful() {
    let output = cfy(&["--help"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage: cfy"));
}

#[test]
fn invalid_command_has_stable_usage_exit_code_and_suggestion() {
    let output = cfy(&["versoin"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("version"));
}

#[test]
fn runtime_command_error_uses_core_exit_code() {
    let output = cfy_outside_project(&["a", "show"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no Shopify app or theme project found"));
    assert!(!stderr.contains("reserved but not implemented yet"));
}

#[test]
fn unavailable_backends_have_command_specific_diagnostics() {
    let app = cfy(&["app", "logs"]);
    assert_eq!(app.status.code(), Some(1));
    let app_stderr = String::from_utf8_lossy(&app.stderr);
    assert!(app_stderr.contains("app logs is not available"));
    assert!(app_stderr.contains("issues/40"));
    assert!(!app_stderr.contains("reserved but not implemented yet"));

    let theme = cfy(&["theme", "preview"]);
    assert_eq!(theme.status.code(), Some(1));
    let theme_stderr = String::from_utf8_lossy(&theme.stderr);
    assert!(theme_stderr.contains("theme preview is not available"));
    assert!(theme_stderr.contains("issues/39"));
    assert!(!theme_stderr.contains("reserved but not implemented yet"));
}

#[test]
fn version_supports_machine_readable_output() {
    let output = cfy(&["version", "--json"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        concat!(
            "{\"name\":\"cfy\",\"version\":\"",
            env!("CARGO_PKG_VERSION"),
            "\"}\n"
        )
    );
}

#[test]
fn completion_generates_a_shell_script() {
    let output = cfy(&["completion", "bash"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("_cfy"));
}
