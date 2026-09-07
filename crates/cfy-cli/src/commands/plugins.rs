use super::super::{output::Output, select_text_choice};
use cfy_core::{Error, Result};
use cfy_plugins::{
    InstallOptions as PluginInstallOptions, LinkOptions as PluginLinkOptions,
    MutationResult as PluginMutationResult, PackageManagerConfig, PluginKind, PluginService,
    ResetOptions as PluginResetOptions, UpdateOptions as PluginUpdateOptions,
};
use cfy_process::Supervisor;
use clap::Subcommand;
use std::{
    env,
    io::{self, IsTerminal},
    path::PathBuf,
};

#[derive(Debug, Subcommand)]
pub enum PluginsCommand {
    /// Add one or more plugins.
    Add {
        #[arg(required = true, num_args = 1..)]
        plugin: Vec<String>,
        #[arg(short = 'f', long)]
        force: bool,
        #[arg(short = 's', long, conflicts_with_all = ["plugin_verbose", "verbose"])]
        silent: bool,
        #[arg(short = 'v')]
        plugin_verbose: bool,
    },
    /// Install one or more plugins.
    Install {
        #[arg(required = true, num_args = 1..)]
        plugin: Vec<String>,
        #[arg(short = 'f', long)]
        force: bool,
        #[arg(short = 's', long, conflicts_with_all = ["plugin_verbose", "verbose"])]
        silent: bool,
        #[arg(short = 'v')]
        plugin_verbose: bool,
    },
    /// Inspect one or more plugins.
    Inspect {
        #[arg(default_value = ".", num_args = 0..)]
        plugin: Vec<String>,
        #[arg(short = 'v')]
        plugin_verbose: bool,
    },
    /// Link a plugin directory.
    Link {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, conflicts_with = "no_install")]
        install: bool,
        #[arg(long)]
        no_install: bool,
        #[arg(short = 'v')]
        plugin_verbose: bool,
    },
    /// Remove effective plugin registrations.
    Remove {
        #[arg(num_args = 0..)]
        plugin: Vec<String>,
        #[arg(short = 'v')]
        plugin_verbose: bool,
    },
    /// Reset plugin registry state.
    Reset {
        #[arg(long)]
        hard: bool,
        #[arg(long)]
        reinstall: bool,
    },
    /// Uninstall installed plugins.
    Uninstall {
        #[arg(num_args = 0..)]
        plugin: Vec<String>,
        #[arg(short = 'v')]
        plugin_verbose: bool,
    },
    /// Unlink linked plugins.
    Unlink {
        #[arg(num_args = 0..)]
        plugin: Vec<String>,
        #[arg(short = 'v')]
        plugin_verbose: bool,
    },
    /// Update installed plugins.
    Update {
        #[arg(short = 'v')]
        plugin_verbose: bool,
    },
}

fn plugin_registry_root() -> PathBuf {
    if let Some(root) = env::var_os("CFY_PLUGIN_ROOT") {
        return PathBuf::from(root);
    }
    if let Some(root) = env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(root).join("catify/plugins");
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/catify/plugins");
    }
    PathBuf::from(".catify/plugins")
}

fn plugin_service() -> PluginService {
    PluginService::new(
        plugin_registry_root(),
        PackageManagerConfig {
            executable: env::var_os("CFY_PACKAGE_MANAGER")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("npm")),
        },
        Supervisor::default(),
    )
}

fn plugin_process_exit(result: &PluginMutationResult) -> Option<u8> {
    result.process.as_ref().and_then(|process| {
        (process.exit_code != Some(0)).then(|| {
            process
                .exit_code
                .and_then(|code| u8::try_from(code).ok())
                .unwrap_or(1)
        })
    })
}

fn plugin_names_for_kind(service: &PluginService, kind: Option<PluginKind>) -> Result<Vec<String>> {
    Ok(service
        .registry()
        .all()?
        .into_iter()
        .filter(|record| kind.is_none_or(|kind| record.kind == kind))
        .map(|record| record.name)
        .collect())
}

fn selected_plugin_names(
    service: &PluginService,
    names: Vec<String>,
    kind: Option<PluginKind>,
    non_interactive: bool,
) -> Result<Vec<String>> {
    if !names.is_empty() {
        return Ok(names);
    }
    let choices = plugin_names_for_kind(service, kind)?;
    if choices.is_empty() {
        return Ok(Vec::new());
    }
    if non_interactive || !io::stdin().is_terminal() {
        return Err(Error::invalid_input(
            "plugin names are required in non-interactive mode",
        ));
    }
    let index = select_text_choice("Which plugin do you want to remove?", &choices)?;
    Ok(vec![choices[index].clone()])
}

pub(crate) async fn plugins_command(
    command: PluginsCommand,
    non_interactive: bool,
    output: &Output,
) -> Result<u8> {
    let service = plugin_service();
    match command {
        PluginsCommand::Add {
            plugin,
            force,
            silent,
            plugin_verbose,
        } => {
            let mut results = Vec::with_capacity(plugin.len());
            for source in plugin {
                let result = service
                    .add_with_options(
                        &source,
                        PluginInstallOptions {
                            force,
                            silent,
                            verbose: plugin_verbose,
                        },
                    )
                    .await?;
                let exit = plugin_process_exit(&result);
                results.push(result);
                if let Some(code) = exit {
                    output
                        .success("Plugin installation failed", &results)
                        .map_err(|error| Error::process(error.to_string()))?;
                    return Ok(code);
                }
            }
            output
                .success("Plugins installed", &results)
                .map_err(|error| Error::process(error.to_string()))?;
        }
        PluginsCommand::Install {
            plugin,
            force,
            silent,
            plugin_verbose,
        } => {
            let mut results = Vec::with_capacity(plugin.len());
            for source in plugin {
                let result = service
                    .install_with_options(
                        &source,
                        PluginInstallOptions {
                            force,
                            silent,
                            verbose: plugin_verbose,
                        },
                    )
                    .await?;
                let exit = plugin_process_exit(&result);
                results.push(result);
                if let Some(code) = exit {
                    output
                        .success("Plugin installation failed", &results)
                        .map_err(|error| Error::process(error.to_string()))?;
                    return Ok(code);
                }
            }
            output
                .success("Plugins installed", &results)
                .map_err(|error| Error::process(error.to_string()))?;
        }
        PluginsCommand::Inspect {
            plugin,
            plugin_verbose,
        } => {
            let records = service.inspect(&plugin)?;
            let human = records
                .iter()
                .map(|record| {
                    if plugin_verbose {
                        format!(
                            "{}\t{:?}\t{}\t{}",
                            record.name,
                            record.kind,
                            record.version.as_deref().unwrap_or("unknown"),
                            record.path.display()
                        )
                    } else {
                        format!(
                            "{}\t{}",
                            record.name,
                            record.version.as_deref().unwrap_or("unknown")
                        )
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            output
                .success(&human, &records)
                .map_err(|error| Error::process(error.to_string()))?;
        }
        PluginsCommand::Link {
            path,
            install,
            no_install,
            plugin_verbose,
        } => {
            let result = service
                .link(
                    path,
                    PluginLinkOptions {
                        install_dependencies: install || !no_install,
                        verbose: plugin_verbose,
                    },
                )
                .await?;
            let exit = plugin_process_exit(&result);
            output
                .success("Plugin linked", &result)
                .map_err(|error| Error::process(error.to_string()))?;
            if let Some(code) = exit {
                return Ok(code);
            }
        }
        PluginsCommand::Remove {
            plugin,
            plugin_verbose: _,
        } => {
            let names = selected_plugin_names(&service, plugin, None, non_interactive)?;
            let results = names
                .iter()
                .map(|name| service.remove(name))
                .collect::<Result<Vec<_>>>()?;
            output
                .success("Plugins removed", &results)
                .map_err(|error| Error::process(error.to_string()))?;
        }
        PluginsCommand::Reset { hard, reinstall } => {
            let result = service
                .reset(PluginResetOptions { hard, reinstall })
                .await?;
            let exit = result.reinstalled.iter().find_map(plugin_process_exit);
            output
                .success("Plugin registry reset", &result)
                .map_err(|error| Error::process(error.to_string()))?;
            if let Some(code) = exit {
                return Ok(code);
            }
        }
        PluginsCommand::Uninstall {
            plugin,
            plugin_verbose: _,
        } => {
            let names = selected_plugin_names(
                &service,
                plugin,
                Some(PluginKind::Installed),
                non_interactive,
            )?;
            let results = names
                .iter()
                .map(|name| service.uninstall(name))
                .collect::<Result<Vec<_>>>()?;
            output
                .success("Plugins uninstalled", &results)
                .map_err(|error| Error::process(error.to_string()))?;
        }
        PluginsCommand::Unlink {
            plugin,
            plugin_verbose: _,
        } => {
            let names =
                selected_plugin_names(&service, plugin, Some(PluginKind::Linked), non_interactive)?;
            let results = names
                .iter()
                .map(|name| service.unlink(name))
                .collect::<Result<Vec<_>>>()?;
            output
                .success("Plugins unlinked", &results)
                .map_err(|error| Error::process(error.to_string()))?;
        }
        PluginsCommand::Update { plugin_verbose } => {
            let results = service
                .update_with_options(PluginUpdateOptions {
                    verbose: plugin_verbose,
                })
                .await?;
            let exit = results.iter().find_map(plugin_process_exit);
            output
                .success("Plugins updated", &results)
                .map_err(|error| Error::process(error.to_string()))?;
            if let Some(code) = exit {
                return Ok(code);
            }
        }
    }
    Ok(0)
}
