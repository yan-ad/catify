use crate::{
    AbortOnDrop, open_browser, output::Output, select_organization, store_access_token,
    store_bulk_client,
};
use cfy_app::BusinessPlatformClient;
use cfy_auth::{
    NativeCredentialStore,
    identity::{HttpIdentityTransport, IdentityClient, IdentityConfig},
};
use cfy_bulk::{
    BulkClient, BulkOperationId, BulkOperationStatus, GraphiqlServer, MutationPolicy,
    StoreDomain as BulkStoreDomain, resolve_api_version,
};
use cfy_config::write_atomic;
use cfy_core::{Cancellation, Error, ErrorKind, Result};
use cfy_store::{
    AdminStoreBackend, OrganizationStoreClient, StoreBackend, StoreCommand as StoreOperation,
    StoreManagementBackend, StoreTarget, browser_url,
    preview_store::{PreviewStoreClient, PreviewStoreRequest},
    store_auth::{
        PreviewStoreSession, StoreAuthBootstrap, StoreAuthCallback, StoreAuthRegistry,
        exchange_code,
    },
};
use clap::{ArgAction, Subcommand};
use std::{
    env,
    io::{self, IsTerminal},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

#[derive(Debug, Subcommand)]
pub enum StoreAuthCommand {
    /// List stores authenticated directly with store auth.
    #[command(disable_version_flag = true)]
    List,
}

#[derive(Debug, Subcommand)]
pub enum StoreCreateCommand {
    /// Create a preview Shopify store.
    #[command(disable_version_flag = true)]
    Preview {
        #[arg(short = 'j', long, env = "SHOPIFY_FLAG_JSON")]
        json: bool,
        #[arg(long, env = "SHOPIFY_FLAG_PREVIEW_STORE_NAME")]
        name: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_STORE_COUNTRY")]
        country: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum StoreBulkCommand {
    /// Execute a bulk operation.
    #[command(disable_version_flag = true)]
    Execute {
        #[arg(short = 's', long, env = "SHOPIFY_FLAG_STORE")]
        store: String,
        #[arg(
            short = 'q',
            long,
            env = "SHOPIFY_FLAG_QUERY",
            conflicts_with = "query_file"
        )]
        query: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_QUERY_FILE")]
        query_file: Option<PathBuf>,
        #[arg(short = 'v', long, env = "SHOPIFY_FLAG_VARIABLES", action = ArgAction::Append, conflicts_with = "variable_file")]
        variables: Vec<String>,
        #[arg(long, env = "SHOPIFY_FLAG_VARIABLE_FILE")]
        variable_file: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_OUTPUT_FILE")]
        output_file: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_WATCH")]
        watch: bool,
        #[arg(long, env = "SHOPIFY_FLAG_VERSION")]
        version: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_ALLOW_MUTATIONS")]
        allow_mutations: bool,
    },
    /// Show bulk operation status.
    Status {
        #[arg(short = 's', long, env = "SHOPIFY_FLAG_STORE")]
        store: String,
        #[arg(long, env = "SHOPIFY_FLAG_ID")]
        id: Option<String>,
    },
    /// Cancel a bulk operation.
    Cancel {
        #[arg(short = 's', long, env = "SHOPIFY_FLAG_STORE")]
        store: String,
        #[arg(long, env = "SHOPIFY_FLAG_ID")]
        id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum StoreCliCommand {
    /// Authenticate against a store.
    #[command(
        disable_version_flag = true,
        subcommand_negates_reqs = true,
        args_conflicts_with_subcommands = true
    )]
    Auth {
        #[command(subcommand)]
        command: Option<StoreAuthCommand>,
        #[arg(short = 's', long, env = "SHOPIFY_FLAG_STORE", required = true)]
        store: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_SCOPES", required = true)]
        scopes: Option<String>,
    },
    /// Run, check, and cancel bulk Admin API operations.
    Bulk {
        #[command(subcommand)]
        command: StoreBulkCommand,
    },
    /// Create Shopify stores.
    Create {
        #[command(subcommand)]
        command: StoreCreateCommand,
    },
    /// List stores in a Shopify organization.
    #[command(disable_version_flag = true)]
    List {
        #[arg(long, env = "SHOPIFY_FLAG_ORGANIZATION_ID")]
        organization_id: Option<String>,
    },
    /// Show store information.
    Info {
        #[arg(long)]
        store: String,
    },
    /// Open your Shopify storefront in the default browser.
    #[command(disable_version_flag = true)]
    Open {
        #[arg(short = 's', long, env = "SHOPIFY_FLAG_STORE", required = true)]
        store: String,
    },
    /// Open a local GraphiQL UI for a store.
    #[command(disable_version_flag = true)]
    Graphiql {
        #[arg(short = 's', long, env = "SHOPIFY_FLAG_STORE", required = true)]
        store: String,
        #[arg(short = 'v', long, env = "SHOPIFY_FLAG_VARIABLES")]
        variables: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_ALLOW_MUTATIONS")]
        allow_mutations: bool,
        #[arg(long, env = "SHOPIFY_FLAG_PORT")]
        port: Option<u16>,
        #[arg(long, env = "SHOPIFY_FLAG_VERSION")]
        version: Option<String>,
    },
    /// Execute an Admin API request.
    Execute {
        #[arg(long)]
        store: String,
        query: String,
    },
    /// Delete a store. Requires --confirm.
    Delete {
        #[arg(long)]
        store: String,
        #[arg(long)]
        confirm: bool,
    },
    /// Authenticate Stripe for the selected store.
    StripeAuth {
        #[arg(long)]
        store: String,
    },
}

fn store_token() -> Result<String> {
    env::var("SHOPIFY_CLI_TOKEN")
        .or_else(|_| env::var("SHOPIFY_CLI_THEME_TOKEN"))
        .map_err(|_| {
            Error::new(
                ErrorKind::Api,
                "store authentication is required; set SHOPIFY_CLI_TOKEN or complete cfy auth login",
            )
        })
}

pub(crate) async fn store_command(
    command: StoreCliCommand,
    non_interactive: bool,
    output: &Output,
) -> Result<u8> {
    match command {
        StoreCliCommand::Auth {
            command: Some(StoreAuthCommand::List),
            ..
        } => {
            let entries = StoreAuthRegistry::default()
                .list_current()
                .await?
                .iter()
                .map(cfy_store::store_auth::StoreAuthSummary::public)
                .collect::<Vec<_>>();
            let human = if entries.is_empty() {
                "No stores authenticated directly with `cfy store auth`.".to_owned()
            } else {
                entries
                    .iter()
                    .map(|entry| {
                        format!(
                            "{}\t{}\t{}\t{}",
                            entry.store,
                            entry
                                .associated_user
                                .email
                                .as_deref()
                                .unwrap_or(&entry.user_id),
                            entry.scopes.join(","),
                            entry.acquired_at
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            output
                .success(&human, &entries)
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        StoreCliCommand::Graphiql {
            store,
            variables,
            allow_mutations,
            port,
            version,
        } => {
            if non_interactive || !io::stdin().is_terminal() {
                return Err(Error::invalid_input(
                    "store graphiql requires an interactive terminal; use `cfy store execute` for automation",
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
            let target = StoreTarget::parse(&store)?;
            let token = store_access_token(&target.domain).await?;
            let domain = BulkStoreDomain::parse(&target.domain)
                .map_err(|error| Error::api(error.to_string()))?;
            let version = resolve_api_version(&domain, version.as_deref())
                .await
                .map_err(|error| Error::api(error.to_string()))?;
            let secret = cfy_bulk::Secret::new(token);
            let client = BulkClient::new(&domain, &version, &secret)
                .map_err(|error| Error::api(error.to_string()))?;
            let policy = if allow_mutations {
                MutationPolicy::Allow
            } else {
                MutationPolicy::Deny
            };
            let server = GraphiqlServer::bind_with_policy(client, port.unwrap_or(3457), policy)
                .await
                .map_err(|error| Error::api(error.to_string()))?;
            let url = server
                .url(variables.as_deref())
                .map_err(|error| Error::api(error.to_string()))?;
            output
                .success(
                    &format!("GraphiQL is running at {url}\nPress Ctrl+C to stop."),
                    &serde_json::json!({
                        "url": url,
                        "store": target.domain,
                        "version": version.as_str(),
                        "mutations_allowed": allow_mutations,
                    }),
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
            return Ok(0);
        }
        StoreCliCommand::Auth {
            command: None,
            store,
            scopes,
        } => {
            if non_interactive || !io::stdin().is_terminal() {
                return Err(Error::invalid_input(
                    "store auth requires an interactive browser flow",
                ));
            }
            let store = store.ok_or_else(|| Error::invalid_input("store auth requires --store"))?;
            let requested_scopes =
                scopes.ok_or_else(|| Error::invalid_input("store auth requires --scopes"))?;
            let registry = StoreAuthRegistry::default();
            let normalized_store = StoreTarget::parse(&store)?.domain;
            let mut scopes = requested_scopes
                .split([',', ' '])
                .map(str::trim)
                .filter(|scope| !scope.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if let Some(previous) = registry
                .list()?
                .into_iter()
                .find(|summary| summary.store == normalized_store)
            {
                scopes.extend(previous.scopes);
            }
            scopes.sort();
            scopes.dedup();
            let bootstrap = StoreAuthBootstrap::new(&store, &scopes.join(","))?;
            let callback = StoreAuthCallback::bind(&bootstrap).await?;
            output
                .lifecycle("Opening Shopify store authentication in your browser...")
                .map_err(|error| Error::process(error.to_string()))?;
            let opened = open_browser(&bootstrap.authorization_url);
            if !opened {
                output
                    .lifecycle(&format!(
                        "Open this URL manually:\n{}",
                        bootstrap.authorization_url
                    ))
                    .map_err(|error| Error::process(error.to_string()))?;
            }
            let code = callback.wait(Duration::from_secs(5 * 60)).await?;
            let result = exchange_code(&bootstrap, &code).await?;
            registry.save(&result).await?;
            output
                .success(&format!("Authenticated {}", result.store), &result.public())
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        StoreCliCommand::List { organization_id } => {
            let identity = "default";
            let credential_store = Arc::new(NativeCredentialStore::default());
            let identity_client = Arc::new(IdentityClient::new(
                HttpIdentityTransport::new()?,
                IdentityConfig::from_env(|key| env::var(key).ok())?,
            ));
            let sessions = cfy_auth::SessionManager::new(credential_store, identity_client);
            let session = sessions.session(identity).await?.ok_or_else(|| {
                Error::new(
                    ErrorKind::Api,
                    "no authenticated session; run `cfy auth login` first",
                )
            })?;
            let organizations = BusinessPlatformClient::from_session(&session)
                .await?
                .list_organizations()
                .await?;
            if organizations.is_empty() {
                output
                    .success(
                        "No stores found in your Shopify organization.",
                        &serde_json::json!({"stores": []}),
                    )
                    .map_err(|error| Error::process(error.to_string()))?;
                return Ok(0);
            }
            let organization = if let Some(requested) = organization_id {
                organizations
                    .iter()
                    .find(|organization| organization.id == requested)
                    .cloned()
                    .ok_or_else(|| {
                        let available = organizations
                            .iter()
                            .map(|organization| format!("{} ({})", organization.name, organization.id))
                            .collect::<Vec<_>>()
                            .join(", ");
                        Error::invalid_input(format!(
                            "organization with ID {requested} was not found; available organizations: {available}"
                        ))
                    })?
            } else if organizations.len() == 1 {
                organizations[0].clone()
            } else if non_interactive {
                return Err(Error::invalid_input(
                    "an organization ID is required to list stores non-interactively; pass --organization-id or run `cfy organization list`",
                ));
            } else {
                select_organization(&organizations)?
            };
            let result = OrganizationStoreClient::from_session(&session, &organization.id)
                .await
                .map_err(Error::from)?
                .list()
                .await
                .map_err(Error::from)?;
            if result.truncated {
                output
                    .lifecycle(&format!(
                        "Showing the {} most recent stores in {}. More stores exist.",
                        cfy_store::STORE_LIST_LIMIT,
                        result.organization_name
                    ))
                    .map_err(|error| Error::process(error.to_string()))?;
            }
            let human = if result.stores.is_empty() {
                format!("No stores found in {}.", result.organization_name)
            } else {
                let rows = result
                    .stores
                    .iter()
                    .map(|store| {
                        format!(
                            "{}\t{}\t{}\t{}",
                            store.store,
                            store.name.as_deref().unwrap_or(""),
                            store.store_type.as_deref().unwrap_or(""),
                            store.created_at
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "Organization: {} ({})\nSubdomain\tName\tType\tCreated\n{}",
                    result.organization_name, result.organization_id, rows
                )
            };
            output
                .success(&human, &result)
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        StoreCliCommand::Create {
            command:
                StoreCreateCommand::Preview {
                    json,
                    name,
                    country,
                },
        } => {
            let result = PreviewStoreClient::new()?
                .create(PreviewStoreRequest { name, country })
                .await?;
            StoreAuthRegistry::default()
                .save_preview(PreviewStoreSession {
                    store: &result.domain,
                    shop_id: &result.shop_id,
                    name: &result.name,
                    country: result.country.as_deref(),
                    placeholder_account_uuid: result.placeholder_account_uuid.as_deref(),
                    scopes: &result.admin_api_scopes,
                    access_token: &result.admin_api_token,
                })
                .await?;
            let public = result.public();
            let human = format!(
                "{}\n\nNext steps\n{}",
                public.message,
                public.next_steps.join("\n")
            );
            output
                .with_json(json)
                .success(&human, &public)
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        StoreCliCommand::Delete { store, confirm } => {
            if !confirm {
                return Err(Error::invalid_input("store delete requires --confirm"));
            }
            let endpoint = env::var("CFY_PARTNER_API_URL").map_err(|_| {
                Error::new(
                    ErrorKind::Api,
                    "store lifecycle API is not configured; set CFY_PARTNER_API_URL",
                )
            })?;
            let token = store_token()?;
            let backend = StoreManagementBackend::new(&endpoint, &token).map_err(Error::from)?;
            let value = backend.delete(&store).await.map_err(Error::from)?;
            output
                .success("Store deleted", &value)
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        StoreCliCommand::Bulk {
            command: StoreBulkCommand::Status { store, id },
        } => {
            let client = store_bulk_client(&store, None).await?;
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
            return Ok(0);
        }
        StoreCliCommand::Bulk {
            command: StoreBulkCommand::Cancel { store, id },
        } => {
            let client = store_bulk_client(&store, None).await?;
            let id = BulkOperationId::parse(&id)
                .map_err(|error| Error::invalid_input(error.to_string()))?;
            let operation = client
                .cancel(&id)
                .await
                .map_err(|error| Error::api(error.to_string()))?;
            output
                .success("Bulk operation cancellation requested", &operation)
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        StoreCliCommand::Info { store } => {
            let target = StoreTarget::parse(&store)?;
            let token = store_access_token(&target.domain).await?;
            let backend = AdminStoreBackend::new(&target, &token).map_err(Error::from)?;
            let info = backend.info(&target).await.map_err(Error::from)?;
            output
                .success(&format!("Store {}", target.domain), &info)
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        StoreCliCommand::Execute { store, query } => {
            let target = StoreTarget::parse(&store)?;
            let token = store_access_token(&target.domain).await?;
            let backend = AdminStoreBackend::new(&target, &token).map_err(Error::from)?;
            let data = backend
                .execute(&target, &query)
                .await
                .map_err(Error::from)?;
            output
                .success("Store query completed", &data)
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        StoreCliCommand::Bulk {
            command:
                StoreBulkCommand::Execute {
                    store,
                    query,
                    query_file,
                    variables,
                    variable_file,
                    output_file,
                    watch,
                    version,
                    allow_mutations,
                },
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
                        "store bulk execute requires exactly one of --query or --query-file",
                    ));
                }
            };
            let client = store_bulk_client(&store, version.as_deref()).await?;
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
                    if !allow_mutations {
                        return Err(Error::invalid_input(
                            "bulk mutations are disabled by default; pass --allow-mutations",
                        ));
                    }
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
                    client
                        .execute_mutation_with_policy(&document, &jsonl, MutationPolicy::Allow)
                        .await
                }
            }
            .map_err(|error| Error::api(error.to_string()))?;
            let operation = if watch {
                let cancellation = Cancellation::default();
                let signal = cancellation.clone();
                let _ctrl_c = AbortOnDrop(tokio::spawn(async move {
                    if tokio::signal::ctrl_c().await.is_ok() {
                        signal.cancel();
                    }
                }));
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
            return Ok(0);
        }
        _ => {}
    }
    let (operation, target, destructive, confirm) = match command {
        StoreCliCommand::Open { store } => {
            let target = StoreTarget::parse(&store)?;
            let url = browser_url(StoreOperation::Open, &target)?;
            let opened = !non_interactive && open_browser(url.as_ref());
            output
                .success(
                    url.as_ref(),
                    &serde_json::json!({ "url": url, "opened": opened }),
                )
                .map_err(|error| Error::process(error.to_string()))?;
            if !opened && !non_interactive {
                output
                    .lifecycle("Browser did not open automatically. Open the store URL manually.")
                    .map_err(|error| Error::process(error.to_string()))?;
            }
            return Ok(0);
        }
        StoreCliCommand::Delete { store, confirm } => {
            (StoreOperation::Delete, store, true, confirm)
        }
        StoreCliCommand::Auth { .. } => unreachable!("store auth returns before fallback dispatch"),
        StoreCliCommand::Graphiql { .. } => {
            unreachable!("store graphiql returns before fallback dispatch")
        }
        StoreCliCommand::Info { store } => (StoreOperation::Info, store, false, false),
        StoreCliCommand::Execute { store, .. } => (StoreOperation::Execute, store, false, false),
        StoreCliCommand::Create {
            command: StoreCreateCommand::Preview { name, .. },
        } => (
            StoreOperation::CreatePreview,
            name.unwrap_or_else(|| "preview".into()),
            true,
            false,
        ),
        StoreCliCommand::Bulk {
            command: StoreBulkCommand::Execute { store, .. },
        } => (StoreOperation::BulkExecute, store, false, false),
        StoreCliCommand::Bulk {
            command: StoreBulkCommand::Status { store, .. },
        } => (StoreOperation::BulkStatus, store, false, false),
        StoreCliCommand::Bulk {
            command: StoreBulkCommand::Cancel { store, .. },
        } => (StoreOperation::BulkCancel, store, true, true),
        StoreCliCommand::StripeAuth { store } => (StoreOperation::StripeAuth, store, false, false),
        StoreCliCommand::List { .. } => unreachable!("store list returns before fallback dispatch"),
    };

    let _target = StoreTarget::parse(&target)?;
    cfy_store::ConfirmationPolicy {
        non_interactive,
        confirm,
        destructive,
    }
    .authorize()?;
    Err(cfy_store::StoreError::Unsupported(operation).into())
}
