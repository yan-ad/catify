mod config;
mod functions;

pub use config::AppConfigCommand;
use config::{app_config_command, app_state_path, load_local_app_configs};
use functions::app_function_command;
pub use functions::{AppFunctionCommand, AppFunctionContext};

use super::super::{
    AbortOnDrop, SHOPIFY_API_VERSION, authenticated_session, open_browser, output::Output,
    select_organization, select_text_choice,
};
use cfy_app::{
    AppDevClient, AppManagementClient, BusinessPlatformClient, LinkOptions, RemoteOrganization,
    exchange_app_management_token,
    extension_generate::{GenerateExtensionOptions, generate_extension},
    extension_import::{
        ExistingDirectoryPolicy, ExtensionRegistrationProvider, ImportExtensionsOptions,
        ImportSelection, RemoteExtensionRegistration, filter_imported_registrations,
        import_directory_conflicts, import_extension_registrations, migration_family,
    },
    logs::AppLogsClient,
    webhook::{
        WebhookClient, WebhookDeliveryMethod, deliver_local_webhook, resolve_delivery_method,
    },
    write_linked_config,
};
use cfy_app_init::{
    AppInitRequest, AppTemplate, PackageManager as AppPackageManager, ReactRouterFlavor,
    initialize as initialize_app, parse_github_template_url, slugify as slugify_app_name,
};
use cfy_auth::{
    NativeCredentialStore, Secret,
    identity::{HttpIdentityTransport, IdentityClient, IdentityConfig},
};
use cfy_build::{BuildInput, BuildMode, BuildOptions, BuildPipeline};
use cfy_bulk::{
    AppCredentials as BulkAppCredentials, BulkClient, BulkOperationId, BulkOperationStatus,
    GraphiqlServer, StoreDomain as BulkStoreDomain, exchange_client_credentials,
    resolve_api_version,
};
use cfy_config::{
    active_config::ActiveConfigState,
    app_env::{from_project as app_environment, merge_dotenv, redacted as redact_app_environment},
    project::{Environment, ProjectKind, ProjectOverrides, discover, resolve_environment},
    write_atomic,
};
use cfy_core::{Cancellation, Error, ErrorKind, Result};
use cfy_deploy::{
    AppManagementBackend as DeployBackend, DeployOptions, DeploySelection, VersionMetadata,
    deploy as deploy_app,
};
use cfy_dev::{ComponentSpec, DevOptions, DevSession};
use cfy_extension_adapter::{Adapter, AdapterCommand, Parallelism};
use cfy_process::{OutputMode, ProcessSpec, Supervisor};
use cfy_store::{
    AdminStoreBackend, OrganizationStoreClient, StoreTarget,
    custom_data::{existing_definitions, import_definitions},
};
use cfy_tunnel::{CloudflaredAdapter, TunnelConfig, TunnelProvider, TunnelSession};
use clap::{ArgAction, Args, Subcommand, ValueEnum};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

fn select_extension_imports(
    registrations: &[RemoteExtensionRegistration],
) -> Result<ImportSelection> {
    let mut families = BTreeMap::<String, Vec<&RemoteExtensionRegistration>>::new();
    for registration in registrations {
        families
            .entry(migration_family(&registration.extension_type).to_owned())
            .or_default()
            .push(registration);
    }
    let family_names = families.keys().cloned().collect::<Vec<_>>();
    let family_index = if family_names.len() == 1 {
        0
    } else {
        select_text_choice(
            "Which extension family would you like to import?",
            &family_names,
        )?
    };
    let family = &family_names[family_index];
    let candidates = &families[family];
    if candidates.len() == 1 {
        return Ok(ImportSelection::Uuids(BTreeSet::from([candidates[0]
            .uuid
            .clone()])));
    }
    let mut choices = vec![format!("All {family} extensions")];
    choices.extend(
        candidates
            .iter()
            .map(|registration| format!("{} ({})", registration.title, registration.uuid)),
    );
    let selected = select_text_choice("Which extension would you like to import?", &choices)?;
    if selected == 0 {
        return Ok(ImportSelection::Uuids(
            candidates
                .iter()
                .map(|registration| registration.uuid.clone())
                .collect(),
        ));
    }
    Ok(ImportSelection::Uuids(BTreeSet::from([candidates
        [selected - 1]
        .uuid
        .clone()])))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum AppLogStatusArg {
    Success,
    Failure,
}

struct AppLogsRunArgs {
    config: Option<String>,
    auth_alias: Option<String>,
    client_id: Option<String>,
    path: Option<PathBuf>,
    reset: bool,
    stores: Vec<String>,
    sources: Vec<String>,
    status: Option<AppLogStatusArg>,
}

#[derive(Debug, Subcommand)]
pub enum AppLogsCommand {
    /// Print source names accepted by `app logs --source`.
    #[command(disable_version_flag = true)]
    Sources {
        #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID", conflicts_with = "config")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum WebhookDeliveryMethodArg {
    Http,
    GooglePubSub,
    EventBridge,
}

impl From<WebhookDeliveryMethodArg> for WebhookDeliveryMethod {
    fn from(value: WebhookDeliveryMethodArg) -> Self {
        match value {
            WebhookDeliveryMethodArg::Http => Self::Http,
            WebhookDeliveryMethodArg::GooglePubSub => Self::GooglePubSub,
            WebhookDeliveryMethodArg::EventBridge => Self::EventBridge,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum AppWebhookCommand {
    /// Trigger delivery of a sample webhook topic payload.
    #[command(disable_version_flag = true)]
    Trigger {
        #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID", conflicts_with = "config")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
        #[arg(long, env = "SHOPIFY_FLAG_TOPIC")]
        topic: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_API_VERSION")]
        api_version: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_DELIVERY_METHOD")]
        delivery_method: Option<WebhookDeliveryMethodArg>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_SECRET")]
        client_secret: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_ADDRESS")]
        address: Option<String>,
    },
}

fn required_interactive_value(
    value: Option<String>,
    label: &str,
    non_interactive: bool,
) -> Result<String> {
    if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
        return Ok(value);
    }
    if non_interactive || !io::stdin().is_terminal() {
        return Err(Error::invalid_input(format!(
            "{label} is required in non-interactive mode"
        )));
    }
    eprint!("{label}: ");
    io::stderr().flush().ok();
    let mut input = String::new();
    io::stdin().read_line(&mut input).map_err(|error| {
        Error::with_source(ErrorKind::Process, format!("could not read {label}"), error)
    })?;
    let value = input.trim();
    if value.is_empty() {
        return Err(Error::invalid_input(format!("{label} cannot be empty")));
    }
    Ok(value.to_owned())
}

async fn build_app_graph(
    graph: &cfy_config::graph::AppConfigGraph,
) -> Result<cfy_build::BuildReport> {
    if graph
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == cfy_config::graph::DiagnosticSeverity::Error)
    {
        return Err(Error::config(
            "app configuration contains errors; run `cfy app config validate` for details",
        ));
    }
    let inputs = graph
        .apps
        .first()
        .map(|app| {
            app.extensions
                .iter()
                .map(|extension| BuildInput {
                    output_dir: graph.root.join(".catify/build").join(
                        extension
                            .handle
                            .as_deref()
                            .or(extension.name.as_deref())
                            .unwrap_or("extension"),
                    ),
                    memory_mb: 256,
                    configuration: serde_json::to_value(&extension.raw)
                        .unwrap_or(serde_json::Value::Null),
                    extension: extension.clone(),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let supervisor = Supervisor::default();
    let adapter = if inputs.is_empty() {
        None
    } else {
        let command = env::var_os("CFY_EXTENSION_ADAPTER").ok_or_else(|| {
            Error::config(
                "this app contains extensions; set CFY_EXTENSION_ADAPTER to a compatible build adapter executable",
            )
        })?;
        Some(
            Adapter::discover(
                &supervisor,
                AdapterCommand::new(PathBuf::from(command)),
                None,
            )
            .await?,
        )
    };
    BuildPipeline::new(adapter.as_ref(), &supervisor)
        .run(
            graph,
            inputs,
            BuildOptions {
                mode: BuildMode::Incremental,
                parallelism: Parallelism {
                    max_jobs: std::thread::available_parallelism()
                        .map(usize::from)
                        .unwrap_or(1),
                    max_memory_mb: 1024,
                },
            },
        )
        .await
}

fn create_deploy_bundle(
    graph: &cfy_config::graph::AppConfigGraph,
    build: &cfy_build::BuildReport,
) -> Result<cfy_build::BuildReport> {
    let directory = graph.root.join(".catify");
    std::fs::create_dir_all(&directory).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            "could not create deploy directory",
            error,
        )
    })?;
    let path = directory.join("deploy-bundle.tar.br");
    let file = std::fs::File::create(&path).map_err(|error| {
        Error::with_source(ErrorKind::Config, "could not create deploy bundle", error)
    })?;
    let encoder = brotli::CompressorWriter::new(file, 4096, 6, 22);
    let mut archive = tar::Builder::new(encoder);
    let app = graph
        .apps
        .first()
        .ok_or_else(|| Error::config("selected app graph has no app node"))?;
    let manifest = serde_json::json!({
        "name": app.config.name,
        "handle": app.config.raw.get("handle").and_then(toml::Value::as_str),
        "modules": app.extensions.iter().map(|extension| serde_json::json!({
            "type": extension.extension_type,
            "handle": extension.handle,
            "uid": extension.uid,
            "config": extension.raw,
        })).collect::<Vec<_>>(),
    });
    let manifest = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| Error::config(format!("could not encode deploy manifest: {error}")))?;
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    archive
        .append_data(&mut header, "manifest.json", manifest.as_slice())
        .map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                "could not archive deploy manifest",
                error,
            )
        })?;
    for artifact in &build.artifacts {
        let file_name = artifact.path.file_name().ok_or_else(|| {
            Error::config(format!(
                "invalid build artifact path {}",
                artifact.path.display()
            ))
        })?;
        let archive_path = PathBuf::from("artifacts")
            .join(&artifact.extension)
            .join(file_name);
        archive
            .append_path_with_name(&artifact.path, archive_path)
            .map_err(|error| {
                Error::with_source(ErrorKind::Config, "could not archive build artifact", error)
            })?;
    }
    archive.finish().map_err(|error| {
        Error::with_source(ErrorKind::Config, "could not finish deploy bundle", error)
    })?;
    Ok(cfy_build::BuildReport {
        mode: build.mode.clone(),
        skipped: build.skipped.clone(),
        artifacts: vec![cfy_build::Artifact {
            extension: "complete-source".into(),
            path,
        }],
        diagnostics: build.diagnostics.clone(),
    })
}

#[derive(Debug, Args)]
pub struct AppBulkContext {
    #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
    pub(crate) config: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
    auth_alias: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID", conflicts_with = "config")]
    client_id: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_PATH")]
    path: Option<PathBuf>,
    #[arg(long, env = "SHOPIFY_FLAG_RESET")]
    reset: bool,
    #[arg(short = 's', long, env = "SHOPIFY_FLAG_STORE")]
    pub(crate) store: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum AppBulkCommand {
    /// Execute a bulk operation.
    #[command(disable_version_flag = true)]
    Execute {
        #[command(flatten)]
        context: AppBulkContext,
        #[arg(
            short = 'q',
            long,
            env = "SHOPIFY_FLAG_QUERY",
            conflicts_with = "query_file"
        )]
        query: Option<String>,
        #[arg(
            long,
            env = "SHOPIFY_FLAG_QUERY_FILE",
            required_unless_present = "query"
        )]
        query_file: Option<PathBuf>,
        #[arg(short = 'v', long, env = "SHOPIFY_FLAG_VARIABLES", action = ArgAction::Append, conflicts_with = "variable_file")]
        variables: Vec<String>,
        #[arg(long, env = "SHOPIFY_FLAG_VARIABLE_FILE")]
        variable_file: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_OUTPUT_FILE", requires = "watch")]
        output_file: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_WATCH")]
        watch: bool,
        #[arg(long, env = "SHOPIFY_FLAG_VERSION")]
        version: Option<String>,
    },
    /// Check bulk operation status.
    Status {
        #[command(flatten)]
        context: AppBulkContext,
        #[arg(long, env = "SHOPIFY_FLAG_ID")]
        id: Option<String>,
    },
    /// Cancel a bulk operation.
    Cancel {
        #[command(flatten)]
        context: AppBulkContext,
        #[arg(long, env = "SHOPIFY_FLAG_ID", required = true)]
        id: String,
    },
}

async fn app_dev(args: AppDevArgs, output: &Output) -> Result<u8> {
    let AppDevArgs {
        config,
        auth_alias: _,
        client_id,
        path,
        reset,
        store: _,
        skip_dependencies_installation,
        no_update: _,
        subscription_product_url,
        checkout_cart_url,
        install_mkcert,
        use_localhost,
        tunnel_url,
        localhost_port,
        theme,
        theme_app_extension_port,
        store_password,
        notify,
    } = args;
    if subscription_product_url.is_some()
        || checkout_cart_url.is_some()
        || install_mkcert
        || theme.is_some()
        || theme_app_extension_port.is_some()
        || store_password.is_some()
        || notify.is_some()
    {
        return Err(Error::api(
            "this app dev invocation requires preview features that are not wired to the native runtime yet; remove preview/theme/notify flags or track issue #29",
        ));
    }
    if skip_dependencies_installation {
        output
            .lifecycle(
                "warning: --skip-dependencies-installation is deprecated; Catify never installs dependencies during app dev",
            )
            .map_err(|error| Error::process(error.to_string()))?;
    }

    let selected = selected_app_environment(path, config, client_id, reset)?;
    let graph =
        cfy_config::graph::AppConfigGraph::load_selected(&selected.project, &selected.config_path)?;
    let app = graph
        .apps
        .first()
        .ok_or_else(|| Error::config("selected app configuration was not loaded"))?;
    let specs = app
        .webs
        .iter()
        .filter_map(web_dev_component)
        .collect::<Vec<_>>();
    if specs.is_empty() {
        return Err(Error::config(
            "no [commands].dev entries were found in shopify.web.toml files",
        ));
    }

    let supervisor = Supervisor::default();
    let cancellation = Cancellation::default();
    let mut tunnel = None;
    let public_url = if use_localhost {
        None
    } else if let Some(url) = tunnel_url {
        let url = url::Url::parse(&url)
            .map_err(|error| Error::invalid_input(format!("invalid --tunnel-url: {error}")))?;
        if url.scheme() != "https" {
            return Err(Error::invalid_input("--tunnel-url must use HTTPS"));
        }
        Some(url)
    } else {
        let mut session = TunnelSession::new(
            supervisor.clone(),
            CloudflaredAdapter,
            TunnelConfig {
                local_host: "127.0.0.1".into(),
                local_port: localhost_port.unwrap_or(3000),
                public_url: None,
                provider: TunnelProvider::Cloudflared {
                    executable: env::var("CFY_CLOUDFLARED_BIN")
                        .unwrap_or_else(|_| "cloudflared".into()),
                },
                max_reconnects: 2,
                readiness_timeout_ms: 30_000,
            },
        )?;
        let url = session.start(&cancellation).await?;
        tunnel = Some(session);
        Some(url)
    };
    let signal = cancellation.clone();
    let signal_supervisor = supervisor.clone();
    let _signal_task = AbortOnDrop(tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
            let _ = signal_supervisor.shutdown().await;
        }
    }));
    let mut session = DevSession::new(supervisor, &specs, DevOptions::default())?;
    session.start(&specs, &cancellation).await?;
    output
        .lifecycle(&match public_url {
            Some(ref url) => format!(
                "Running {} app component(s); public URL: {url}",
                specs.len()
            ),
            None => format!("Running {} app component(s) on localhost", specs.len()),
        })
        .map_err(|error| Error::process(error.to_string()))?;
    let result = match session.wait(&cancellation).await {
        Ok(()) => Ok(0),
        Err(cfy_dev::DevError::Cancelled) if cancellation.is_cancelled() => Ok(0),
        Err(error) => Err(error.into()),
    };
    if let Some(mut tunnel) = tunnel {
        tunnel.stop().await?;
    }
    result
}

fn web_dev_component(web: &cfy_config::graph::WebConfig) -> Option<ComponentSpec> {
    let command = web.raw.get("commands")?.as_table()?.get("dev")?.as_str()?;
    #[cfg(windows)]
    let process = ProcessSpec::new("cmd")
        .args(["/C", command])
        .current_dir(&web.directory)
        .output(OutputMode::Inherit);
    #[cfg(not(windows))]
    let process = ProcessSpec::new("sh")
        .args(["-c", command])
        .current_dir(&web.directory)
        .output(OutputMode::Inherit);
    Some(ComponentSpec {
        name: web
            .name
            .clone()
            .unwrap_or_else(|| web.directory.display().to_string()),
        process,
        max_restarts: 1,
        restart_backoff_ms: 250,
    })
}

#[derive(Debug, Args)]
pub struct AppDevArgs {
    #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
    config: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
    auth_alias: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID")]
    client_id: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_PATH")]
    path: Option<PathBuf>,
    #[arg(long, env = "SHOPIFY_FLAG_RESET")]
    reset: bool,
    #[arg(short = 's', long, env = "SHOPIFY_FLAG_STORE")]
    store: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_SKIP_DEPENDENCIES_INSTALLATION")]
    skip_dependencies_installation: bool,
    #[arg(long, env = "SHOPIFY_FLAG_NO_UPDATE")]
    no_update: bool,
    #[arg(long, env = "SHOPIFY_FLAG_SUBSCRIPTION_PRODUCT_URL")]
    subscription_product_url: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_CHECKOUT_CART_URL")]
    checkout_cart_url: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_INSTALL_MKCERT")]
    install_mkcert: bool,
    #[arg(long, env = "SHOPIFY_FLAG_USE_LOCALHOST")]
    use_localhost: bool,
    #[arg(long, env = "SHOPIFY_FLAG_TUNNEL_URL")]
    tunnel_url: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_LOCALHOST_PORT", value_parser = clap::value_parser!(u16).range(1..))]
    localhost_port: Option<u16>,
    #[arg(short = 't', long, env = "SHOPIFY_FLAG_THEME")]
    theme: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_THEME_APP_EXTENSION_PORT", value_parser = clap::value_parser!(u16).range(1..))]
    theme_app_extension_port: Option<u16>,
    #[arg(long, env = "SHOPIFY_FLAG_STORE_PASSWORD")]
    store_password: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_NOTIFY")]
    notify: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum AppDevCommand {
    /// Clean local development state for the selected app.
    Clean {
        #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
        #[arg(short = 's', long, env = "SHOPIFY_FLAG_STORE")]
        store: Option<String>,
    },
}

struct AppInitCliOptions {
    name: Option<String>,
    path: PathBuf,
    auth_alias: Option<String>,
    client_id: Option<String>,
    organization_id: Option<String>,
    template: Option<String>,
    flavor: Option<String>,
    package_manager: Option<String>,
    non_interactive: bool,
}

enum AppInitRemoteTarget {
    Existing(cfy_app::RemoteApp),
    New {
        organization: RemoteOrganization,
        name: String,
    },
}

async fn app_init(options: AppInitCliOptions, output: &Output) -> Result<u8> {
    let template = resolve_app_init_template(
        options.template.clone(),
        options.flavor.clone(),
        options.non_interactive,
    )?;
    let package_manager = match options.package_manager.as_deref().unwrap_or("npm") {
        "npm" => AppPackageManager::Npm,
        "yarn" => AppPackageManager::Yarn,
        "pnpm" => AppPackageManager::Pnpm,
        "bun" => AppPackageManager::Bun,
        value => {
            return Err(Error::invalid_input(format!(
                "unsupported package manager `{value}`"
            )));
        }
    };
    if options.non_interactive && options.client_id.is_none() {
        if options
            .name
            .as_deref()
            .is_none_or(|name| name.trim().is_empty())
        {
            return Err(Error::invalid_input(
                "app init requires --name or --client-id in non-interactive mode",
            ));
        }
        if options.organization_id.is_none() {
            return Err(Error::invalid_input(
                "app init requires --organization-id or --client-id in non-interactive mode",
            ));
        }
    }
    let identity = options.auth_alias.unwrap_or_else(|| "default".into());
    let session = authenticated_session(&identity).await?;
    let business = BusinessPlatformClient::from_session(&session).await?;
    let app_management = AppManagementClient::from_session(&session).await?;

    let target = if let Some(client_id) = options.client_id.as_deref() {
        let organizations = business.list_organizations().await?;
        let mut matched = None;
        for organization in organizations {
            let apps = app_management.list_apps(&organization.id).await?;
            if apps.iter().any(|app| app.client_id == client_id) {
                matched = Some(
                    app_management
                        .app_by_client_id_in_organization(&organization.id, client_id)
                        .await?,
                );
                break;
            }
        }
        AppInitRemoteTarget::Existing(matched.ok_or_else(|| {
            Error::invalid_input(format!(
                "no app with client ID `{client_id}` is available to this account"
            ))
        })?)
    } else {
        let organization = if let Some(id) = options.organization_id.as_deref() {
            business
                .list_organizations()
                .await?
                .into_iter()
                .find(|organization| organization.id == id)
                .ok_or_else(|| {
                    Error::invalid_input("the requested organization is not available")
                })?
        } else {
            let organizations = business.list_organizations().await?;
            if organizations.len() == 1 {
                organizations[0].clone()
            } else if options.non_interactive {
                return Err(Error::invalid_input(
                    "app init requires --organization-id in non-interactive mode",
                ));
            } else {
                select_organization(&organizations)?
            }
        };
        if let Some(name) = options.name.filter(|name| !name.trim().is_empty()) {
            AppInitRemoteTarget::New { organization, name }
        } else if options.non_interactive {
            return Err(Error::invalid_input(
                "app init requires --name or --client-id in non-interactive mode",
            ));
        } else {
            let apps = app_management.list_apps(&organization.id).await?;
            let choices = vec![
                "Create a new app".to_owned(),
                "Link to an existing app".to_owned(),
            ];
            if apps.is_empty()
                || select_text_choice("How would you like to initialize this project?", &choices)?
                    == 0
            {
                AppInitRemoteTarget::New {
                    organization,
                    name: required_interactive_value(None, "App name", false)?,
                }
            } else {
                let choices = apps
                    .iter()
                    .map(|app| format!("{} ({})", app.name, app.client_id))
                    .collect::<Vec<_>>();
                let selected = select_text_choice("Which app would you like to link?", &choices)?;
                AppInitRemoteTarget::Existing(
                    app_management
                        .app_by_client_id_in_organization(
                            &organization.id,
                            &apps[selected].client_id,
                        )
                        .await?,
                )
            }
        }
    };
    let name = match &target {
        AppInitRemoteTarget::Existing(app) => app.name.clone(),
        AppInitRemoteTarget::New { name, .. } => name.clone(),
    };
    let directory_name = slugify_app_name(&name);
    if directory_name.is_empty() {
        return Err(Error::invalid_input(
            "app name must contain a letter or number",
        ));
    }

    let mut request = AppInitRequest::new(&options.path, &name, template);
    request.directory_name = directory_name;
    request.package_manager = package_manager;
    request.interactive = !options.non_interactive;
    request.package_manager_executable = env::var_os("CFY_PACKAGE_MANAGER_BIN").map(PathBuf::from);
    let scaffold = initialize_app(request)
        .await
        .map_err(|error| Error::process(error.to_string()))?;

    let remote_app = if let AppInitRemoteTarget::Existing(app) = target {
        app
    } else if let AppInitRemoteTarget::New { organization, .. } = target {
        let (launchable, scopes) = app_creation_shape(&scaffold.destination);
        let created = match app_management
            .create_app(
                &organization.id,
                &name,
                SHOPIFY_API_VERSION,
                launchable,
                &scopes,
            )
            .await
        {
            Ok(created) => created,
            Err(error) => {
                remove_app_init_destination(&scaffold.destination);
                return Err(error);
            }
        };
        match app_management
            .app_by_client_id_in_organization(&organization.id, &created.client_id)
            .await
        {
            Ok(app) => app,
            Err(error) => {
                remove_app_init_destination(&scaffold.destination);
                return Err(Error::api(format!(
                    "Shopify app `{}` was created with client ID `{}`, but Catify could not fetch its configuration: {error}. Retry with `cfy app init --client-id {}`",
                    created.id, created.client_id, created.client_id
                )));
            }
        }
    } else {
        unreachable!("app init remote target is exhaustive")
    };
    let link = match write_linked_config(
        &LinkOptions {
            directory: scaffold.destination.clone(),
            client_id: Some(remote_app.client_id.clone()),
            file_name: Some("shopify.app.toml".into()),
            force: true,
        },
        &remote_app,
    ) {
        Ok(link) => link,
        Err(error) => {
            remove_app_init_destination(&scaffold.destination);
            return Err(error);
        }
    };
    output
        .success(
            &format!(
                "{} is ready for you to build!",
                scaffold.destination.display()
            ),
            &serde_json::json!({"scaffold": scaffold, "app": remote_app, "link": link}),
        )
        .map_err(|error| Error::process(error.to_string()))?;
    Ok(0)
}

fn resolve_app_init_template(
    template: Option<String>,
    flavor: Option<String>,
    non_interactive: bool,
) -> Result<AppTemplate> {
    let template = if let Some(template) = template {
        template
    } else if non_interactive {
        return Err(Error::invalid_input(
            "app init requires --template in non-interactive mode",
        ));
    } else {
        let choices = vec![
            "Build a React Router app (recommended)".to_owned(),
            "Build an extension-only app".to_owned(),
        ];
        if select_text_choice("Get started building your app:", &choices)? == 0 {
            "reactRouter".into()
        } else {
            "none".into()
        }
    };
    match template.as_str() {
        "reactRouter" => {
            let flavor = if let Some(flavor) = flavor {
                flavor
            } else if non_interactive {
                return Err(Error::invalid_input(
                    "React Router app init requires --flavor javascript or typescript",
                ));
            } else {
                let choices = vec!["JavaScript".to_owned(), "TypeScript".to_owned()];
                if select_text_choice(
                    "For your React Router template, which language do you want?",
                    &choices,
                )? == 0
                {
                    "javascript".into()
                } else {
                    "typescript".into()
                }
            };
            match flavor.as_str() {
                "javascript" => Ok(AppTemplate::ReactRouter(ReactRouterFlavor::JavaScript)),
                "typescript" => Ok(AppTemplate::ReactRouter(ReactRouterFlavor::TypeScript)),
                _ => Err(Error::invalid_input(
                    "--flavor must be javascript or typescript for reactRouter",
                )),
            }
        }
        "none" => {
            if flavor.is_some() {
                return Err(Error::invalid_input(
                    "--flavor is only supported by templates that define flavors",
                ));
            }
            Ok(AppTemplate::None)
        }
        "remix" => {
            let flavor = flavor.unwrap_or_else(|| "typescript".into());
            let branch = match flavor.as_str() {
                "javascript" => "javascript",
                "typescript" => "main",
                _ => {
                    return Err(Error::invalid_input(
                        "--flavor must be javascript or typescript for remix",
                    ));
                }
            };
            Ok(AppTemplate::Custom(cfy_app_init::GitTemplate {
                repository: "https://github.com/Shopify/shopify-app-template-remix.git".into(),
                branch: Some(branch.into()),
                subpath: None,
            }))
        }
        "node" | "ruby" => {
            if flavor.is_some() {
                return Err(Error::invalid_input(
                    "--flavor is not supported by the selected template",
                ));
            }
            Ok(AppTemplate::Custom(cfy_app_init::GitTemplate {
                repository: format!(
                    "https://github.com/Shopify/shopify-app-template-{template}.git"
                ),
                branch: None,
                subpath: None,
            }))
        }
        custom => Ok(AppTemplate::Custom(
            parse_github_template_url(custom)
                .map_err(|error| Error::invalid_input(error.to_string()))?,
        )),
    }
}

fn app_creation_shape(directory: &Path) -> (bool, Vec<String>) {
    let graph = discover(directory, Some(ProjectKind::App))
        .ok()
        .and_then(|project| cfy_config::graph::AppConfigGraph::load(&project).ok());
    let launchable = graph.as_ref().is_some_and(|graph| {
        graph.apps.iter().any(|app| {
            app.webs.iter().any(|web| {
                web.roles
                    .iter()
                    .any(|role| matches!(role.as_str(), "frontend" | "backend"))
            })
        })
    });
    let scopes = graph
        .and_then(|graph| graph.apps.into_iter().next())
        .and_then(|app| {
            app.config
                .raw
                .get("access_scopes")?
                .get("scopes")?
                .as_str()
                .map(str::to_owned)
        })
        .map(|scopes| {
            scopes
                .split(',')
                .map(str::trim)
                .filter(|scope| !scope.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    (launchable, scopes)
}

fn remove_app_init_destination(path: &Path) {
    let _ = std::fs::remove_dir_all(path);
}

fn selected_app_environment(
    path: Option<PathBuf>,
    config: Option<String>,
    client_id: Option<String>,
    reset: bool,
) -> Result<cfy_config::project::ProjectEnvironment> {
    let cwd = path.unwrap_or(env::current_dir().map_err(|error| Error::api(error.to_string()))?);
    let project = discover(&cwd, Some(ProjectKind::App))?;
    let state_path = app_state_path();
    let mut state = ActiveConfigState::load(&state_path)?;
    if reset {
        state.clear(project.root());
        state.write(&state_path)?;
    }
    let environment = env::vars().collect::<Environment>();
    let explicit_config = config.or_else(|| {
        environment
            .get("CFY_CONFIG")
            .or_else(|| environment.get("SHOPIFY_FLAG_APP_CONFIG"))
            .cloned()
    });
    let client_config = if explicit_config.is_none() {
        client_id
            .map(|client_id| {
                load_local_app_configs(&project)?
                    .into_iter()
                    .find(|choice| choice.client_id == client_id)
                    .map(|choice| choice.file_name)
                    .ok_or_else(|| {
                        Error::invalid_input(
                            "the specified client ID could not be found in any app TOML file",
                        )
                    })
            })
            .transpose()?
    } else {
        None
    };
    let cached_config = if explicit_config.is_none() {
        state.selected(project.root()).map(ToOwned::to_owned)
    } else {
        None
    };
    resolve_environment(
        project,
        &ProjectOverrides {
            config: explicit_config.or(client_config).or(cached_config),
            ..ProjectOverrides::default()
        },
        &environment,
    )
}

fn app_info(
    path: Option<PathBuf>,
    config: Option<String>,
    client_id: Option<String>,
    reset: bool,
    web_env: bool,
    output: &Output,
) -> Result<u8> {
    let selected = selected_app_environment(path, config, client_id, reset)?;
    if web_env {
        let values = app_environment(&selected);
        let rendered = values
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("\n");
        output
            .success(&rendered, &values)
            .map_err(|error| Error::process(error.to_string()))?;
        return Ok(0);
    }

    let graph =
        cfy_config::graph::AppConfigGraph::load_selected(&selected.project, &selected.config_path)?;
    let app = graph
        .apps
        .first()
        .ok_or_else(|| Error::config("selected app configuration produced no app node"))?;
    let scopes = app
        .config
        .raw
        .get("access_scopes")
        .and_then(toml::Value::as_table)
        .and_then(|table| table.get("scopes"))
        .and_then(toml::Value::as_str)
        .unwrap_or_default();
    let package_manager =
        if graph.root.join("bun.lock").exists() || graph.root.join("bun.lockb").exists() {
            "bun"
        } else if graph.root.join("pnpm-lock.yaml").exists() {
            "pnpm"
        } else if graph.root.join("yarn.lock").exists() {
            "yarn"
        } else if graph.root.join("package-lock.json").exists() {
            "npm"
        } else {
            "unknown"
        };
    let extensions = app
        .extensions
        .iter()
        .map(|extension| {
            serde_json::json!({
                "name": extension.name,
                "handle": extension.handle,
                "type": extension.extension_type,
                "family": format!("{:?}", extension.family),
                "path": extension.path,
            })
        })
        .collect::<Vec<_>>();
    let webs = app
        .webs
        .iter()
        .map(|web| {
            serde_json::json!({
                "name": web.name,
                "roles": web.roles,
                "type": web.web_type,
                "path": web.path,
            })
        })
        .collect::<Vec<_>>();
    let diagnostics = graph
        .diagnostics
        .iter()
        .map(|diagnostic| {
            serde_json::json!({
                "severity": format!("{:?}", diagnostic.severity).to_lowercase(),
                "message": diagnostic.message,
                "file": diagnostic.location.file,
                "line": diagnostic.location.line,
                "column": diagnostic.location.column,
            })
        })
        .collect::<Vec<_>>();
    let report = serde_json::json!({
        "project_root": graph.root,
        "config": selected.config_path,
        "app": {
            "name": app.config.name,
            "client_id": app.config.client_id,
            "application_url": app.config.application_url,
            "embedded": app.config.embedded,
            "scopes": scopes,
        },
        "extensions": extensions,
        "webs": webs,
        "system": {
            "package_manager": package_manager,
            "catify_version": env!("CARGO_PKG_VERSION"),
            "os": env::consts::OS,
            "arch": env::consts::ARCH,
        },
        "diagnostics": diagnostics,
    });
    let human = format!(
        "App information\n\nName: {}\nClient ID: {}\nConfiguration: {}\nApplication URL: {}\nEmbedded: {}\nScopes: {}\nExtensions: {}\nWeb components: {}\nPackage manager: {}\nDiagnostics: {}",
        app.config.name.as_deref().unwrap_or("unknown"),
        app.config.client_id.as_deref().unwrap_or("unknown"),
        selected.config_path.display(),
        app.config.application_url.as_deref().unwrap_or("unknown"),
        app.config
            .embedded
            .map_or("unknown".to_owned(), |value| value.to_string()),
        if scopes.is_empty() { "none" } else { scopes },
        app.extensions.len(),
        app.webs.len(),
        package_manager,
        graph.diagnostics.len(),
    );
    output
        .success(&human, &report)
        .map_err(|error| Error::process(error.to_string()))?;
    Ok(0)
}

async fn app_bulk_client(
    context: AppBulkContext,
    requested_version: Option<&str>,
) -> Result<BulkClient> {
    let selected = selected_app_environment(
        context.path,
        context.config,
        context.client_id,
        context.reset,
    )?;
    let client_id = selected
        .document
        .get("client_id")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| Error::invalid_input("selected app configuration has no client_id"))?;
    let store_domain = context
        .store
        .or(selected.store)
        .ok_or_else(|| Error::invalid_input("a development store is required; pass --store"))?;
    let store_domain = BulkStoreDomain::parse(&store_domain)
        .map_err(|error| Error::invalid_input(error.to_string()))?;
    let identity = context.auth_alias.unwrap_or_else(|| "default".to_owned());
    let store = Arc::new(NativeCredentialStore::default());
    let identity_client = Arc::new(IdentityClient::new(
        HttpIdentityTransport::new()?,
        IdentityConfig::from_env(|key| env::var(key).ok())?,
    ));
    let sessions = cfy_auth::SessionManager::new(Arc::clone(&store), identity_client);
    let session = sessions.session(&identity).await?.ok_or_else(|| {
        Error::api(format!(
            "no authenticated session for `{identity}`; run `cfy auth login` first"
        ))
    })?;
    let app_management = AppManagementClient::from_session(&session).await?;
    let credentials = app_management.app_client_credentials(client_id).await?;
    let credentials = BulkAppCredentials::new(
        credentials.client_id,
        credentials.client_secret.expose().to_owned(),
    );
    let token = exchange_client_credentials(&store_domain, &credentials)
        .await
        .map_err(|error| Error::api(error.to_string()))?;
    let version = resolve_api_version(&store_domain, requested_version)
        .await
        .map_err(|error| Error::api(error.to_string()))?;
    BulkClient::new(&store_domain, &version, token.secret())
        .map_err(|error| Error::api(error.to_string()))
}

async fn app_bulk_command(command: AppBulkCommand, output: &Output) -> Result<u8> {
    match command {
        AppBulkCommand::Execute {
            context,
            query,
            query_file,
            variables,
            variable_file,
            output_file,
            watch,
            version,
        } => {
            let document = match (query, query_file) {
                (Some(query), None) => query,
                (None, Some(path)) => std::fs::read_to_string(&path).map_err(|error| {
                    Error::with_source(
                        ErrorKind::Config,
                        format!("could not read bulk query {}", path.display()),
                        error,
                    )
                })?,
                _ => {
                    return Err(Error::invalid_input(
                        "provide exactly one of --query or --query-file",
                    ));
                }
            };
            let client = app_bulk_client(context, version.as_deref()).await?;
            let operation = match cfy_bulk::operation_kind(&document)
                .map_err(|error| Error::invalid_input(error.to_string()))?
            {
                cfy_bulk::OperationKind::Query => {
                    if !variables.is_empty() || variable_file.is_some() {
                        return Err(Error::invalid_input(
                            "--variables and --variable-file can only be used with mutations",
                        ));
                    }
                    client.execute_query(&document).await
                }
                cfy_bulk::OperationKind::Mutation => {
                    let jsonl = if let Some(path) = variable_file {
                        std::fs::read(&path).map_err(|error| {
                            Error::with_source(
                                ErrorKind::Config,
                                format!("could not read bulk variables {}", path.display()),
                                error,
                            )
                        })?
                    } else {
                        variables.join("\n").into_bytes()
                    };
                    client.execute_mutation(&document, &jsonl).await
                }
            }
            .map_err(|error| Error::api(error.to_string()))?;
            let operation = if watch {
                let cancellation = Cancellation::default();
                client
                    .poll(
                        &BulkOperationId::parse(&operation.id)
                            .map_err(|error| Error::api(error.to_string()))?,
                        cfy_bulk::PollMode::default(),
                        &cancellation,
                    )
                    .await
                    .map_err(|error| Error::api(error.to_string()))?
            } else {
                operation
            };
            if watch
                && operation.status == BulkOperationStatus::Completed
                && operation.url.is_some()
            {
                let results = client
                    .download_jsonl(&operation)
                    .await
                    .map_err(|error| Error::api(error.to_string()))?;
                if let Some(path) = output_file {
                    write_atomic(&path, results.as_bytes()).map_err(|error| {
                        Error::with_source(
                            ErrorKind::Config,
                            format!("could not write bulk results {}", path.display()),
                            error,
                        )
                    })?;
                    output
                        .success(
                            "Bulk results written",
                            &serde_json::json!({"operation": operation, "output_file": path}),
                        )
                        .map_err(|error| Error::process(error.to_string()))?;
                } else {
                    output
                        .success(
                            std::str::from_utf8(results.as_bytes()).unwrap_or_default(),
                            &operation,
                        )
                        .map_err(|error| Error::process(error.to_string()))?;
                }
            } else {
                output
                    .success("Bulk operation", &operation)
                    .map_err(|error| Error::process(error.to_string()))?;
            }
            Ok(0)
        }
        AppBulkCommand::Status { context, id } => {
            let client = app_bulk_client(context, None).await?;
            if let Some(id) = id {
                let id = BulkOperationId::parse(&id)
                    .map_err(|error| Error::invalid_input(error.to_string()))?;
                let operation = client
                    .status(&id)
                    .await
                    .map_err(|error| Error::api(error.to_string()))?;
                output
                    .success("Bulk operation status", &operation)
                    .map_err(|error| Error::process(error.to_string()))?;
            } else {
                let operations = client
                    .list_last_seven_days()
                    .await
                    .map_err(|error| Error::api(error.to_string()))?;
                output
                    .success("Bulk operations", &operations)
                    .map_err(|error| Error::process(error.to_string()))?;
            }
            Ok(0)
        }
        AppBulkCommand::Cancel { context, id } => {
            let client = app_bulk_client(context, Some("2026-01")).await?;
            let id = BulkOperationId::parse(&id)
                .map_err(|error| Error::invalid_input(error.to_string()))?;
            let operation = client
                .cancel(&id)
                .await
                .map_err(|error| Error::api(error.to_string()))?;
            output
                .success("Bulk operation cancellation requested", &operation)
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
    }
}

fn app_log_sources(
    path: Option<PathBuf>,
    config: Option<String>,
    client_id: Option<String>,
    reset: bool,
    output: &Output,
) -> Result<u8> {
    let selected = selected_app_environment(path, config, client_id, reset)?;
    let graph =
        cfy_config::graph::AppConfigGraph::load_selected(&selected.project, &selected.config_path)?;
    let sources = app_log_source_names(&graph);
    let human = if sources.is_empty() {
        "No app log sources found.".to_owned()
    } else {
        format!("extensions\n{}", sources.join("\n"))
    };
    output
        .success(&human, &sources)
        .map_err(|error| Error::process(error.to_string()))?;
    Ok(0)
}

fn app_log_source_names(graph: &cfy_config::graph::AppConfigGraph) -> Vec<String> {
    let mut sources = graph
        .apps
        .first()
        .into_iter()
        .flat_map(|app| &app.extensions)
        .filter(|extension| extension.family == cfy_config::graph::ExtensionFamily::Function)
        .filter_map(|extension| {
            extension
                .handle
                .as_deref()
                .or(extension.name.as_deref())
                .map(|handle| format!("extensions.{handle}"))
        })
        .collect::<Vec<_>>();
    sources.sort();
    sources.dedup();
    sources
}

async fn stream_app_logs(args: AppLogsRunArgs, output: &Output) -> Result<u8> {
    let AppLogsRunArgs {
        config,
        auth_alias,
        client_id,
        path,
        reset,
        stores,
        sources,
        status,
    } = args;
    let selected = selected_app_environment(path, config, client_id, reset)?;
    let graph =
        cfy_config::graph::AppConfigGraph::load_selected(&selected.project, &selected.config_path)?;
    let valid_sources = app_log_source_names(&graph);
    if valid_sources.is_empty() {
        return Err(Error::invalid_input(
            "this app has no function extension log sources",
        ));
    }
    let invalid_sources = sources
        .iter()
        .filter(|source| !valid_sources.contains(source))
        .cloned()
        .collect::<Vec<_>>();
    if !invalid_sources.is_empty() {
        return Err(Error::invalid_input(format!(
            "invalid log sources: {}. Valid sources: {}",
            invalid_sources.join(", "),
            valid_sources.join(", ")
        )));
    }
    let client_id = selected
        .document
        .get("client_id")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| Error::invalid_input("selected app configuration has no client_id"))?;
    let requested_stores = if stores.is_empty() {
        selected.store.into_iter().collect::<Vec<_>>()
    } else {
        stores
    };
    if requested_stores.is_empty() {
        return Err(Error::invalid_input(
            "at least one development or Shopify Plus sandbox store is required; pass --store",
        ));
    }
    let identity = auth_alias.unwrap_or_else(|| "default".to_owned());
    let session = authenticated_session(&identity).await?;
    let app_management = AppManagementClient::from_session(&session).await?;
    let remote_app = app_management.app_by_client_id(client_id).await?;
    let organization_stores =
        OrganizationStoreClient::from_session(&session, &remote_app.organization_id)
            .await
            .map_err(|error| Error::api(error.to_string()))?
            .list()
            .await
            .map_err(|error| Error::api(error.to_string()))?;
    let mut shop_ids = Vec::new();
    let mut store_names = std::collections::BTreeMap::new();
    for requested in requested_stores {
        let target = StoreTarget::parse(&requested)
            .map_err(|error| Error::invalid_input(error.to_string()))?;
        let store = organization_stores
            .stores
            .iter()
            .find(|store| store.store == target.domain)
            .ok_or_else(|| {
                Error::invalid_input(format!(
                    "store `{}` is not an active development or Shopify Plus sandbox store in organization `{}`",
                    target.domain, organization_stores.organization_name
                ))
            })?;
        let id = store
            .id
            .as_deref()
            .and_then(|id| id.rsplit('/').next())
            .and_then(|id| id.parse::<i64>().ok())
            .ok_or_else(|| {
                Error::api(format!(
                    "store `{}` has no numeric Shopify shop ID",
                    target.domain
                ))
            })?;
        shop_ids.push(id);
        store_names.insert(id, target.domain);
    }
    shop_ids.sort_unstable();
    shop_ids.dedup();

    let token = exchange_app_management_token(&session).await?;
    let logs = AppLogsClient::for_organization(&token, &remote_app.organization_id)?;
    let mut subscription = logs.subscribe(&shop_ids, client_id).await?;
    let cancellation = Cancellation::default();
    let signal = cancellation.clone();
    let watcher = AbortOnDrop(tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
        }
    }));
    output
        .lifecycle("Waiting for app logs... Press Ctrl-C to stop.")
        .map_err(|error| Error::process(error.to_string()))?;
    let status = status.map(|status| match status {
        AppLogStatusArg::Success => "success",
        AppLogStatusArg::Failure => "failure",
    });
    let mut cursor = None;
    while !cancellation.is_cancelled() {
        let retry_delay = match logs.poll(&subscription, cursor.as_deref()).await {
            Ok(page) => {
                cursor = page.cursor;
                for log in page.logs.into_iter().filter(|log| {
                    status.is_none_or(|status| log.status == status)
                        && (sources.is_empty() || sources.contains(&log.source_name()))
                }) {
                    let store = store_names
                        .get(&log.shop_id)
                        .cloned()
                        .unwrap_or_else(|| log.shop_id.to_string());
                    let payload = log.parsed_payload();
                    let value = serde_json::json!({
                        "shop_id": log.shop_id,
                        "store": store,
                        "status": log.status,
                        "source": log.source_name(),
                        "log_type": log.log_type,
                        "log_timestamp": log.log_timestamp,
                        "payload": payload,
                    });
                    let human = format!(
                        "{}\t{}\t{}\t{}\n{}",
                        log.log_timestamp,
                        log.status,
                        log.source_name(),
                        store,
                        serde_json::to_string_pretty(&payload)
                            .unwrap_or_else(|_| payload.to_string())
                    );
                    output
                        .success(&human, &value)
                        .map_err(|error| Error::process(error.to_string()))?;
                }
                Duration::from_millis(450)
            }
            Err(error) if error.is_unauthorized() => {
                subscription = logs.subscribe(&shop_ids, client_id).await?;
                Duration::from_millis(450)
            }
            Err(error) if error.is_rate_limited() => {
                output
                    .lifecycle("App logs rate limited; retrying in 60 seconds.")
                    .map_err(|io_error| Error::process(io_error.to_string()))?;
                Duration::from_secs(60)
            }
            Err(error) if error.is_server_error() => {
                output
                    .lifecycle("App logs service unavailable; retrying in 5 seconds.")
                    .map_err(|io_error| Error::process(io_error.to_string()))?;
                Duration::from_secs(5)
            }
            Err(error) => {
                return Err(Error::api(format!(
                    "could not poll app logs: {}",
                    error.messages.join(", ")
                )));
            }
        };
        tokio::time::sleep(retry_delay).await;
    }
    drop(watcher);
    Ok(0)
}

async fn app_webhook_command(
    command: AppWebhookCommand,
    non_interactive: bool,
    output: &Output,
) -> Result<u8> {
    let AppWebhookCommand::Trigger {
        config,
        auth_alias,
        client_id,
        path,
        reset,
        topic,
        api_version,
        delivery_method,
        client_secret,
        address,
    } = command;
    let selected = selected_app_environment(path, config, client_id, reset)?;
    let client_id = selected
        .document
        .get("client_id")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| Error::invalid_input("selected app configuration has no client_id"))?;
    let identity = auth_alias.unwrap_or_else(|| "default".to_owned());
    let session = authenticated_session(&identity).await?;
    let app_management = AppManagementClient::from_session(&session).await?;
    let remote_app = app_management.app_by_client_id(client_id).await?;
    let token = exchange_app_management_token(&session).await?;
    let webhook = WebhookClient::for_organization(&token, &remote_app.organization_id)?;

    let api_versions = webhook.api_versions().await?;
    let api_version = if let Some(api_version) = api_version {
        if !api_versions
            .iter()
            .any(|candidate| candidate == &api_version)
        {
            return Err(Error::invalid_input(format!(
                "webhook API version `{api_version}` is not available"
            )));
        }
        api_version
    } else {
        if non_interactive || !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            return Err(Error::invalid_input(
                "--api-version is required in non-interactive mode",
            ));
        }
        let index = select_text_choice("Which API version would you like to use?", &api_versions)?;
        api_versions[index].clone()
    };

    let topics = webhook.topics(&api_version).await?;
    let topic = if let Some(topic) = topic {
        let normalized = topic.to_ascii_lowercase().replace('_', "/");
        topics
            .iter()
            .find(|candidate| candidate.to_ascii_lowercase() == normalized)
            .cloned()
            .ok_or_else(|| {
                Error::invalid_input(format!(
                    "webhook topic `{topic}` is not available for API version `{api_version}`"
                ))
            })?
    } else {
        if non_interactive || !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            return Err(Error::invalid_input(
                "--topic is required in non-interactive mode",
            ));
        }
        let index = select_text_choice("Which webhook topic would you like to trigger?", &topics)?;
        topics[index].clone()
    };

    let address = required_interactive_value(address, "Webhook address", non_interactive)?;
    let delivery_method = resolve_delivery_method(&address, delivery_method.map(Into::into))?;
    let credentials = if let Some(client_secret) = client_secret {
        (client_id.to_owned(), Secret::new(client_secret))
    } else {
        let credentials = app_management.app_client_credentials(client_id).await?;
        (credentials.client_id, credentials.client_secret)
    };
    let sample = webhook
        .trigger(
            &topic,
            &api_version,
            &address,
            delivery_method,
            &credentials.1,
            (delivery_method == WebhookDeliveryMethod::EventBridge)
                .then_some(credentials.0.as_str()),
        )
        .await?;
    if !sample.success {
        return Err(Error::api(format!(
            "Shopify could not trigger the sample webhook: {}",
            sample.errors.join(", ")
        )));
    }
    let delivered_locally = if delivery_method == WebhookDeliveryMethod::Localhost {
        if !deliver_local_webhook(&address, &sample).await? {
            return Err(Error::api("localhost webhook delivery failed"));
        }
        true
    } else {
        false
    };
    output
        .success(
            if delivered_locally {
                "Localhost delivery successful"
            } else {
                "Webhook has been enqueued for delivery"
            },
            &serde_json::json!({
                "topic": topic,
                "api_version": api_version,
                "address": address,
                "delivery_method": delivery_method,
                "delivered_locally": delivered_locally,
            }),
        )
        .map_err(|error| Error::process(error.to_string()))?;
    Ok(0)
}

pub(crate) async fn app_command(
    command: AppCommand,
    non_interactive: bool,
    output: &Output,
) -> Result<u8> {
    match command {
        AppCommand::Build {
            config,
            auth_alias: _,
            client_id,
            path,
            reset,
            skip_dependencies_installation,
        } => {
            if skip_dependencies_installation {
                output.lifecycle(
                    "warning: --skip-dependencies-installation is deprecated; Catify never installs dependencies during app build",
                ).map_err(|error| Error::process(error.to_string()))?;
            }
            let selected = selected_app_environment(path, config, client_id, reset)?;
            let graph = cfy_config::graph::AppConfigGraph::load_selected(
                &selected.project,
                &selected.config_path,
            )?;
            let report = build_app_graph(&graph).await?;
            output
                .success("App build completed", &report)
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        AppCommand::Deploy {
            config,
            auth_alias,
            client_id,
            path,
            reset,
            no_release,
            allow_updates,
            allow_deletes,
            no_build,
            message,
            version,
            source_control_url,
        } => {
            if non_interactive && !no_release && !allow_updates && !allow_deletes {
                return Err(Error::invalid_input(
                    "non-interactive deploy requires --allow-updates, --allow-deletes, or --no-release",
                ));
            }
            let selected = selected_app_environment(path, config, client_id, reset)?;
            let client_id = selected
                .document
                .get("client_id")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| {
                    Error::invalid_input("selected app configuration has no client_id")
                })?;
            let graph = cfy_config::graph::AppConfigGraph::load_selected(
                &selected.project,
                &selected.config_path,
            )?;
            let build = if no_build {
                let path = graph.root.join(".catify/deploy-bundle.tar.br");
                if !path.is_file() {
                    return Err(Error::config(
                        "--no-build requires an existing .catify/deploy-bundle.tar.br",
                    ));
                }
                cfy_build::BuildReport {
                    mode: "cached".into(),
                    skipped: Vec::new(),
                    artifacts: vec![cfy_build::Artifact {
                        extension: "complete-source".into(),
                        path,
                    }],
                    diagnostics: Vec::new(),
                }
            } else {
                let built = build_app_graph(&graph).await?;
                create_deploy_bundle(&graph, &built)?
            };
            let identity = auth_alias.unwrap_or_else(|| "default".to_owned());
            let credential_store = Arc::new(NativeCredentialStore::default());
            let identity_client = Arc::new(IdentityClient::new(
                HttpIdentityTransport::new()?,
                IdentityConfig::from_env(|key| env::var(key).ok())?,
            ));
            let sessions = cfy_auth::SessionManager::new(credential_store, identity_client);
            let session = sessions.session(&identity).await?.ok_or_else(|| {
                Error::api(format!(
                    "no authenticated session for `{identity}`; run `cfy auth login` first"
                ))
            })?;
            let app_management = AppManagementClient::from_session(&session).await?;
            let app = app_management.app_by_client_id(client_id).await?;
            let token = cfy_app::exchange_app_management_token(&session).await?;
            let endpoint = env::var("CFY_APP_MANAGEMENT_URL").unwrap_or_else(|_| {
                "https://app.shopify.com/app_management/unstable/graphql.json".into()
            });
            let backend = DeployBackend::new(&endpoint, token.expose())?;
            let report = deploy_app(
                &backend,
                &DeployOptions {
                    selection: Some(DeploySelection {
                        app: app.id,
                        environment: app.organization_id,
                    }),
                    non_interactive,
                    dry_run: false,
                    release: !no_release,
                    metadata: VersionMetadata {
                        version_tag: version,
                        message,
                        source_control_url,
                    },
                    upload_policy: Default::default(),
                },
                &build,
                &Cancellation::default(),
            )
            .await?;
            output
                .success("App deployed", &report)
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        AppCommand::Dev { args, command } => match command {
            Some(AppDevCommand::Clean {
                config,
                auth_alias,
                client_id,
                path,
                reset,
                store,
            }) => {
                let selected = selected_app_environment(path, config, client_id, reset)?;
                let client_id = selected
                    .document
                    .get("client_id")
                    .and_then(toml::Value::as_str)
                    .ok_or_else(|| {
                        Error::invalid_input("selected app configuration has no client_id")
                    })?;
                let store_domain = store.or(selected.store.clone()).ok_or_else(|| {
                    Error::invalid_input(
                        "app dev clean requires --store or a store in the selected app config",
                    )
                })?;
                let identity = auth_alias.unwrap_or_else(|| "default".into());
                let session = authenticated_session(&identity).await?;
                let app_management = AppManagementClient::from_session(&session).await?;
                let app = app_management.app_by_client_id(client_id).await?;
                let token = exchange_app_management_token(&session).await?;
                AppDevClient::new(&store_domain, token.expose())?
                    .delete_session(&app.id)
                    .await?;
                let state = selected.project.root().join(".catify/dev");
                if state.exists() {
                    std::fs::remove_dir_all(&state).map_err(|error| {
                        Error::with_source(
                            ErrorKind::Process,
                            format!("could not remove {}", state.display()),
                            error,
                        )
                    })?;
                }
                output
                    .success(
                        "Development state cleaned",
                        &serde_json::json!({"cleaned": true, "path": state}),
                    )
                    .map_err(|error| Error::process(error.to_string()))?;
                Ok(0)
            }
            None => app_dev(*args, output).await,
        },
        AppCommand::Init {
            name,
            path,
            auth_alias,
            client_id,
            organization_id,
            template,
            flavor,
            package_manager,
        } => {
            app_init(
                AppInitCliOptions {
                    name,
                    path,
                    auth_alias,
                    client_id,
                    organization_id,
                    template,
                    flavor,
                    package_manager,
                    non_interactive,
                },
                output,
            )
            .await
        }
        AppCommand::Info {
            config,
            auth_alias: _,
            client_id,
            path,
            reset,
            web_env,
        } => app_info(path, config, client_id, reset, web_env, output),
        AppCommand::Env { command } => app_env_command(command, output),
        AppCommand::Config { command } => {
            app_config_command(command, non_interactive, output).await
        }
        AppCommand::Function { command } => {
            app_function_command(command, non_interactive, output).await
        }
        AppCommand::Bulk { command } => app_bulk_command(command, output).await,
        AppCommand::Versions { command } => app_versions_command(command, output).await,
        AppCommand::Logs {
            config,
            auth_alias: _,
            client_id,
            path,
            reset,
            stores: _,
            sources: _,
            status: _,
            command:
                Some(AppLogsCommand::Sources {
                    config: nested_config,
                    auth_alias: _,
                    client_id: nested_client_id,
                    path: nested_path,
                    reset: nested_reset,
                }),
        } => app_log_sources(
            nested_path.or(path),
            nested_config.or(config),
            nested_client_id.or(client_id),
            nested_reset || reset,
            output,
        ),
        AppCommand::Logs {
            config,
            auth_alias,
            client_id,
            path,
            reset,
            stores,
            sources,
            status,
            command: None,
        } => {
            stream_app_logs(
                AppLogsRunArgs {
                    config,
                    auth_alias,
                    client_id,
                    path,
                    reset,
                    stores,
                    sources,
                    status,
                },
                output,
            )
            .await
        }
        AppCommand::Webhook { command } => {
            app_webhook_command(command, non_interactive, output).await
        }
        AppCommand::Execute {
            context,
            query,
            query_file,
            variables,
            variable_file,
            version,
            output_file,
        } => {
            let document = match (query, query_file) {
                (Some(query), None) => query,
                (None, Some(path)) => std::fs::read_to_string(&path).map_err(|error| {
                    Error::with_source(
                        ErrorKind::Config,
                        format!("could not read GraphQL document {}", path.display()),
                        error,
                    )
                })?,
                _ => {
                    return Err(Error::invalid_input(
                        "provide exactly one of --query or --query-file",
                    ));
                }
            };
            let variables = match (variables, variable_file) {
                (Some(variables), None) => serde_json::from_str(&variables).map_err(|error| {
                    Error::with_source(
                        ErrorKind::Config,
                        "--variables must contain a JSON object",
                        error,
                    )
                })?,
                (None, Some(path)) => {
                    let bytes = std::fs::read(&path).map_err(|error| {
                        Error::with_source(
                            ErrorKind::Config,
                            format!("could not read variables file {}", path.display()),
                            error,
                        )
                    })?;
                    serde_json::from_slice(&bytes).map_err(|error| {
                        Error::with_source(
                            ErrorKind::Config,
                            format!("variables file {} is not valid JSON", path.display()),
                            error,
                        )
                    })?
                }
                (None, None) => serde_json::json!({}),
                (Some(_), Some(_)) => unreachable!("clap rejects conflicting variables flags"),
            };
            if !variables.is_object() {
                return Err(Error::invalid_input(
                    "GraphQL variables must be a JSON object",
                ));
            }
            let client = app_bulk_client(context, version.as_deref()).await?;
            let result = client
                .execute_document(&document, variables)
                .await
                .map_err(|error| Error::api(error.to_string()))?;
            if let Some(path) = output_file {
                let bytes = serde_json::to_vec_pretty(&result).map_err(|error| {
                    Error::with_source(
                        ErrorKind::Config,
                        "could not serialize GraphQL result",
                        error,
                    )
                })?;
                write_atomic(&path, &bytes).map_err(|error| {
                    Error::with_source(
                        ErrorKind::Config,
                        format!("could not write GraphQL result {}", path.display()),
                        error,
                    )
                })?;
                output
                    .success(
                        "GraphQL result written",
                        &serde_json::json!({"output_file": path, "data": result}),
                    )
                    .map_err(|error| Error::process(error.to_string()))?;
            } else {
                output
                    .success("GraphQL request completed", &result)
                    .map_err(|error| Error::process(error.to_string()))?;
            }
            Ok(0)
        }
        AppCommand::Graphiql {
            context,
            port,
            variables,
            version,
        } => {
            if non_interactive || !io::stdin().is_terminal() {
                return Err(Error::invalid_input(
                    "app graphiql requires an interactive terminal; use `cfy app execute` for automation",
                ));
            }
            if let Some(value) = &variables {
                let parsed: serde_json::Value = serde_json::from_str(value).map_err(|error| {
                    Error::with_source(
                        ErrorKind::Config,
                        "--variables must contain valid JSON",
                        error,
                    )
                })?;
                if !parsed.is_object() {
                    return Err(Error::invalid_input(
                        "GraphiQL variables must be a JSON object",
                    ));
                }
            }
            let client = app_bulk_client(context, version.as_deref()).await?;
            let server = GraphiqlServer::bind(client, port.unwrap_or(3457))
                .await
                .map_err(|error| Error::api(error.to_string()))?;
            let url = server
                .url(variables.as_deref())
                .map_err(|error| Error::api(error.to_string()))?;
            output
                .success(
                    &format!("GraphiQL is running at {url}\nPress Ctrl+C to stop."),
                    &serde_json::json!({"url": url}),
                )
                .map_err(|error| Error::process(error.to_string()))?;
            if !open_browser(url.as_str()) {
                output
                    .lifecycle("Browser did not open automatically. Open the URL above manually.")
                    .map_err(|error| Error::process(error.to_string()))?;
            }
            let cancellation = Cancellation::default();
            tokio::select! {
                result = server.run(&cancellation) => result.map_err(|error| Error::api(error.to_string()))?,
                _ = tokio::signal::ctrl_c() => cancellation.cancel(),
            }
            Ok(0)
        }
        AppCommand::Release {
            config,
            auth_alias,
            client_id,
            path,
            reset,
            allow_updates,
            allow_deletes,
            version,
        } => {
            if non_interactive && !allow_updates && !allow_deletes {
                return Err(Error::invalid_input(
                    "app release requires --allow-updates or --allow-deletes in non-interactive mode",
                ));
            }
            if !non_interactive && !allow_updates && !allow_deletes {
                eprint!("Release app version `{version}`? [y/N] ");
                io::stderr().flush().ok();
                let mut answer = String::new();
                io::stdin().read_line(&mut answer).map_err(|error| {
                    Error::with_source(ErrorKind::Process, "could not read confirmation", error)
                })?;
                if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                    return Err(Error::invalid_input("app release was cancelled"));
                }
            }
            let selected = selected_app_environment(path, config, client_id, reset)?;
            let client_id = selected
                .document
                .get("client_id")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| {
                    Error::invalid_input("selected app configuration has no client_id")
                })?;
            let identity = auth_alias.unwrap_or_else(|| "default".to_owned());
            let store = Arc::new(NativeCredentialStore::default());
            let identity_client = Arc::new(IdentityClient::new(
                HttpIdentityTransport::new()?,
                IdentityConfig::from_env(|key| env::var(key).ok())?,
            ));
            let sessions = cfy_auth::SessionManager::new(Arc::clone(&store), identity_client);
            let session = sessions.session(&identity).await?.ok_or_else(|| {
                Error::new(
                    ErrorKind::Api,
                    format!("no authenticated session for `{identity}`; run `cfy auth login --identity {identity}` first"),
                )
            })?;
            let backend = AppManagementClient::from_session(&session).await?;
            let app = backend.app_by_client_id(client_id).await?;
            let report = backend.release_version(&app.id, &version).await?;
            output
                .success("Version released to users", &report)
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        AppCommand::ImportExtensions {
            config,
            auth_alias,
            client_id,
            path,
            reset,
        } => {
            let selected = selected_app_environment(path, config, client_id, reset)?;
            let client_id = selected
                .document
                .get("client_id")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| {
                    Error::invalid_input("selected app configuration has no client_id")
                })?;
            let identity = auth_alias.unwrap_or_else(|| "default".to_owned());
            let session = authenticated_session(&identity).await?;
            let organizations = BusinessPlatformClient::from_session(&session)
                .await?
                .list_organizations()
                .await?;
            let organization = if organizations.len() == 1 {
                organizations[0].clone()
            } else if non_interactive {
                return Err(Error::invalid_input(
                    "multiple organizations are available; run interactively to select one",
                ));
            } else {
                select_organization(&organizations)?
            };
            let backend = AppManagementClient::from_session(&session).await?;
            let dotenv_name = if selected.config_name == "default" {
                ".env".to_owned()
            } else {
                format!(".env.{}", selected.config_name)
            };
            let dotenv_path = selected.project.root().join(dotenv_name);
            let registrations = backend
                .fetch_extension_registrations(client_id, &organization.id)
                .await?
                .into_iter()
                .filter(|registration| {
                    cfy_app::extension_import::is_migratable_type(&registration.extension_type)
                })
                .collect::<Vec<_>>();
            let registrations =
                filter_imported_registrations(registrations, selected.project.root(), &dotenv_path)
                    .map_err(|error| Error::api(error.to_string()))?;
            if registrations.is_empty() {
                return Err(Error::invalid_input(
                    "this app has no dashboard extensions supported by import-extensions",
                ));
            }
            let selection = if non_interactive {
                ImportSelection::All
            } else {
                select_extension_imports(&registrations)?
            };
            let options = ImportExtensionsOptions {
                app_directory: selected.project.root().to_owned(),
                client_id: client_id.to_owned(),
                organization_id: organization.id,
                dotenv_path,
                api_key: client_id.to_owned(),
                selection,
                existing_directory_policy: ExistingDirectoryPolicy::Skip,
                directory_policies: BTreeMap::new(),
            };
            let mut options = options;
            if !non_interactive {
                for conflict in import_directory_conflicts(registrations.clone(), &options)
                    .map_err(|error| Error::api(error.to_string()))?
                {
                    let choices = vec![
                        "Overwrite local TOML with remote configuration".to_owned(),
                        "Keep local TOML".to_owned(),
                        "Cancel".to_owned(),
                    ];
                    match select_text_choice(
                        &format!(
                            "Directory for '{}' already exists. What would you like to do?",
                            conflict.title
                        ),
                        &choices,
                    )? {
                        0 => {
                            options
                                .directory_policies
                                .insert(conflict.uuid, ExistingDirectoryPolicy::Overwrite);
                        }
                        1 => {}
                        _ => return Err(Error::invalid_input("extension import was cancelled")),
                    }
                }
            }
            let report = import_extension_registrations(registrations, &options)
                .map_err(|error| Error::api(error.to_string()))?;
            output
                .success("Extensions imported", &report)
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        AppCommand::Generate {
            command:
                AppGenerateCommand::Extension {
                    config,
                    auth_alias: _,
                    client_id,
                    path,
                    reset,
                    template,
                    name,
                    flavor,
                },
        } => {
            let selected = selected_app_environment(path, config, client_id, reset)?;
            let template =
                required_interactive_value(template, "Extension template", non_interactive)?;
            let name = required_interactive_value(name, "Extension name", non_interactive)?;
            let supervisor = Supervisor::default();
            let report = generate_extension(
                &supervisor,
                &GenerateExtensionOptions {
                    app_directory: selected.project.root().to_owned(),
                    name,
                    template,
                    flavor,
                    repository: env::var("CFY_EXTENSION_TEMPLATE_REPO").ok(),
                },
            )
            .await
            .map_err(|error| Error::process(error.to_string()))?;
            output
                .success("Extension generated", &report)
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        AppCommand::ImportCustomDataDefinitions {
            context,
            include_existing,
        } => {
            let selected = selected_app_environment(
                context.path,
                context.config,
                context.client_id,
                context.reset,
            )?;
            let client_id = selected
                .document
                .get("client_id")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| {
                    Error::invalid_input("selected app configuration has no client_id")
                })?;
            let store_domain = context.store.or(selected.store.clone()).ok_or_else(|| {
                Error::invalid_input("a development store is required; pass --store")
            })?;
            let store_domain = BulkStoreDomain::parse(&store_domain)
                .map_err(|error| Error::invalid_input(error.to_string()))?;
            let identity = context.auth_alias.unwrap_or_else(|| "default".to_owned());
            let session = authenticated_session(&identity).await?;
            let app_management = AppManagementClient::from_session(&session).await?;
            let credentials = app_management.app_client_credentials(client_id).await?;
            let credentials = BulkAppCredentials::new(
                credentials.client_id,
                credentials.client_secret.expose().to_owned(),
            );
            let token = exchange_client_credentials(&store_domain, &credentials)
                .await
                .map_err(|error| Error::api(error.to_string()))?;
            let target = StoreTarget::parse(store_domain.as_str())
                .map_err(|error| Error::invalid_input(error.to_string()))?;
            let backend = AdminStoreBackend::new(&target, token.secret().expose())
                .map_err(|error| Error::api(error.to_string()))?;
            let existing = existing_definitions(&selected.document);
            let report = import_definitions(&backend, &target.domain, &existing, include_existing)
                .await
                .map_err(|error| Error::api(error.to_string()))?;
            let human = format!(
                "Conversion to TOML complete.\n\nConverted {} metafields and {} metaobjects from {}.\n\n{}",
                report.metafield_count, report.metaobject_count, report.store, report.toml
            );
            output
                .success(&human, &report)
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum AppEnvCommand {
    /// Display app and extension environment variables.
    Show {
        #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
    },
    /// Pull app and extension environment variables into a dotenv file.
    Pull {
        #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
        #[arg(long, env = "SHOPIFY_FLAG_ENV_FILE")]
        env_file: Option<PathBuf>,
    },
}

fn app_env_command(command: AppEnvCommand, output: &Output) -> Result<u8> {
    match command {
        AppEnvCommand::Show {
            config,
            auth_alias: _,
            client_id,
            path,
            reset,
        } => {
            let selected = selected_app_environment(path, config, client_id, reset)?;
            let values = app_environment(&selected);
            output
                .success(
                    "App environment",
                    &serde_json::json!({
                        "config": selected.config_name,
                        "config_path": selected.config_path,
                        "values": redact_app_environment(&values),
                        "remote_values_included": false,
                    }),
                )
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        AppEnvCommand::Pull {
            config,
            auth_alias: _,
            client_id,
            path,
            reset,
            env_file,
        } => {
            let selected = selected_app_environment(path, config, client_id, reset)?;
            let values = app_environment(&selected);
            let destination = env_file
                .map(|path| {
                    if path.is_absolute() {
                        path
                    } else {
                        selected.project.root().join(path)
                    }
                })
                .unwrap_or_else(|| selected.project.root().join(".env"));
            let existing = std::fs::read_to_string(&destination).unwrap_or_default();
            let contents = merge_dotenv(&existing, &values);
            write_atomic(&destination, contents.as_bytes()).map_err(|error| {
                Error::with_source(
                    ErrorKind::Config,
                    format!("could not write {}", destination.display()),
                    error,
                )
            })?;
            output
                .success(
                    "App environment written",
                    &serde_json::json!({
                        "config": selected.config_name,
                        "destination": destination,
                        "variables": values.len(),
                        "remote_values_included": false,
                    }),
                )
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum AppVersionsCommand {
    /// List deployed versions of the selected app.
    List {
        #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
    },
}

async fn app_versions_command(command: AppVersionsCommand, output: &Output) -> Result<u8> {
    let AppVersionsCommand::List {
        config,
        auth_alias,
        client_id,
        path,
        reset,
    } = command;
    let selected = selected_app_environment(path, config, client_id, reset)?;
    let client_id = selected
        .document
        .get("client_id")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| Error::invalid_input("selected app configuration has no client_id"))?;
    let identity = auth_alias.unwrap_or_else(|| "default".to_owned());
    let store = Arc::new(NativeCredentialStore::default());
    let identity_client = Arc::new(IdentityClient::new(
        HttpIdentityTransport::new()?,
        IdentityConfig::from_env(|key| env::var(key).ok())?,
    ));
    let sessions = cfy_auth::SessionManager::new(Arc::clone(&store), identity_client);
    let session = sessions.session(&identity).await?.ok_or_else(|| {
        Error::new(
            ErrorKind::Api,
            format!("no authenticated session for `{identity}`; run `cfy auth login --identity {identity}` first"),
        )
    })?;
    let backend = AppManagementClient::from_session(&session).await?;
    let app = backend.app_by_client_id(client_id).await?;
    let report = backend.list_versions(&app.id).await?;
    output
        .success("App versions", &report)
        .map_err(|error| Error::process(error.to_string()))?;
    Ok(0)
}

#[derive(Debug, Subcommand)]
pub enum AppGenerateCommand {
    /// Generate a new app extension.
    #[command(disable_version_flag = true)]
    Extension {
        #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID", conflicts_with = "config")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
        #[arg(short = 't', long, env = "SHOPIFY_FLAG_EXTENSION_TEMPLATE")]
        template: Option<String>,
        #[arg(short = 'n', long, env = "SHOPIFY_FLAG_NAME")]
        name: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_FLAVOR", value_parser = ["vanilla-js", "react", "typescript", "typescript-react", "wasm", "rust"])]
        flavor: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum AppCommand {
    /// Build the app, including extensions.
    Build {
        #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
        #[arg(long, env = "SHOPIFY_FLAG_SKIP_DEPENDENCIES_INSTALLATION")]
        skip_dependencies_installation: bool,
    },
    /// Build, upload, create, and optionally release an app version.
    #[command(disable_version_flag = true)]
    Deploy {
        #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID", conflicts_with = "config")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
        #[arg(long, env = "SHOPIFY_FLAG_NO_RELEASE", conflicts_with_all = ["allow_updates", "allow_deletes"])]
        no_release: bool,
        #[arg(long, env = "SHOPIFY_FLAG_ALLOW_UPDATES")]
        allow_updates: bool,
        #[arg(long, env = "SHOPIFY_FLAG_ALLOW_DELETES")]
        allow_deletes: bool,
        #[arg(long, env = "SHOPIFY_FLAG_NO_BUILD")]
        no_build: bool,
        #[arg(long, env = "SHOPIFY_FLAG_MESSAGE")]
        message: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_VERSION")]
        version: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_SOURCE_CONTROL_URL")]
        source_control_url: Option<String>,
    },
    /// Run the app locally and watch its declared web processes.
    Dev {
        #[command(flatten)]
        args: Box<AppDevArgs>,
        #[command(subcommand)]
        command: Option<AppDevCommand>,
    },
    /// Print basic information about the app and its extensions.
    #[command(alias = "show")]
    Info {
        #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID", conflicts_with = "config")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
        #[arg(long, env = "SHOPIFY_FLAG_OUTPUT_WEB_ENV")]
        web_env: bool,
    },
    /// Initialize a new app project.
    #[command(disable_version_flag = true)]
    Init {
        #[arg(short = 'n', long, env = "SHOPIFY_FLAG_NAME")]
        name: Option<String>,
        #[arg(short = 'p', long, env = "SHOPIFY_FLAG_PATH", default_value = ".")]
        path: PathBuf,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(
            long,
            env = "SHOPIFY_FLAG_CLIENT_ID",
            conflicts_with = "organization_id"
        )]
        client_id: Option<String>,
        #[arg(
            long,
            env = "SHOPIFY_FLAG_ORGANIZATION_ID",
            conflicts_with = "client_id"
        )]
        organization_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_TEMPLATE")]
        template: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_TEMPLATE_FLAVOR")]
        flavor: Option<String>,
        #[arg(short = 'd', long, env = "SHOPIFY_FLAG_PACKAGE_MANAGER", value_parser = ["npm", "yarn", "pnpm", "bun"])]
        package_manager: Option<String>,
    },
    /// Manage app and extension environment variables.
    Env {
        #[command(subcommand)]
        command: AppEnvCommand,
    },
    /// Manage app configuration.
    Config {
        #[command(subcommand)]
        command: AppConfigCommand,
    },
    /// Work with Shopify Functions.
    Function {
        #[command(subcommand)]
        command: AppFunctionCommand,
    },
    /// Execute and manage Admin API bulk operations.
    Bulk {
        #[command(subcommand)]
        command: AppBulkCommand,
    },
    /// Manage deployed app versions.
    Versions {
        #[command(subcommand)]
        command: AppVersionsCommand,
    },
    /// Stream application logs or list available sources.
    #[command(disable_version_flag = true, subcommand_negates_reqs = true)]
    Logs {
        #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID", conflicts_with = "config")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
        #[arg(short = 's', long = "store", env = "SHOPIFY_FLAG_STORE", action = ArgAction::Append)]
        stores: Vec<String>,
        #[arg(long = "source", env = "SHOPIFY_FLAG_SOURCE", action = ArgAction::Append)]
        sources: Vec<String>,
        #[arg(long, env = "SHOPIFY_FLAG_STATUS")]
        status: Option<AppLogStatusArg>,
        #[command(subcommand)]
        command: Option<AppLogsCommand>,
    },
    /// Work with app webhooks.
    Webhook {
        #[command(subcommand)]
        command: AppWebhookCommand,
    },
    /// Execute an Admin API query for the app.
    #[command(disable_version_flag = true)]
    Execute {
        #[command(flatten)]
        context: AppBulkContext,
        #[arg(
            short = 'q',
            long,
            env = "SHOPIFY_FLAG_QUERY",
            conflicts_with = "query_file"
        )]
        query: Option<String>,
        #[arg(
            long,
            env = "SHOPIFY_FLAG_QUERY_FILE",
            required_unless_present = "query"
        )]
        query_file: Option<PathBuf>,
        #[arg(
            short = 'v',
            long,
            env = "SHOPIFY_FLAG_VARIABLES",
            conflicts_with = "variable_file"
        )]
        variables: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_VARIABLE_FILE")]
        variable_file: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_VERSION")]
        version: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_OUTPUT_FILE")]
        output_file: Option<PathBuf>,
    },
    /// Open GraphiQL for the app.
    #[command(disable_version_flag = true)]
    Graphiql {
        #[command(flatten)]
        context: AppBulkContext,
        #[arg(long, env = "SHOPIFY_FLAG_PORT", value_parser = clap::value_parser!(u16).range(1..))]
        port: Option<u16>,
        #[arg(short = 'v', long, env = "SHOPIFY_FLAG_VARIABLES")]
        variables: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_VERSION")]
        version: Option<String>,
    },
    /// Release an app version.
    #[command(disable_version_flag = true)]
    Release {
        #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
        #[arg(long, env = "SHOPIFY_FLAG_ALLOW_UPDATES")]
        allow_updates: bool,
        #[arg(long, env = "SHOPIFY_FLAG_ALLOW_DELETES")]
        allow_deletes: bool,
        #[arg(long, env = "SHOPIFY_FLAG_VERSION")]
        version: String,
    },
    /// Import dashboard-managed app extensions.
    #[command(disable_version_flag = true)]
    ImportExtensions {
        #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID", conflicts_with = "config")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
    },
    /// Generate app resources.
    Generate {
        #[command(subcommand)]
        command: AppGenerateCommand,
    },
    /// Import metafield and metaobject definitions.
    #[command(disable_version_flag = true)]
    ImportCustomDataDefinitions {
        #[command(flatten)]
        context: AppBulkContext,
        #[arg(long, env = "SHOPIFY_FLAG_INCLUDE_EXISTING")]
        include_existing: bool,
    },
}
