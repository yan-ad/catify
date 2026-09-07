use super::super::{Cli, output::Output};
use cfy_core::{Error, ErrorKind, Result};
use clap::{CommandFactory, ValueEnum};

fn command_column_title(column: &CommandColumn) -> &'static str {
    match column {
        CommandColumn::Id => "Id",
        CommandColumn::Plugin => "Plugin",
        CommandColumn::Summary => "Summary",
        CommandColumn::Type => "Type",
    }
}

fn command_column_value(command: &CommandRecord, column: CommandColumn) -> String {
    match column {
        CommandColumn::Id => command.name.clone(),
        CommandColumn::Plugin => command.plugin_name.clone().unwrap_or_default(),
        CommandColumn::Summary => command.summary.clone(),
        CommandColumn::Type => command.plugin_type.clone().unwrap_or_default(),
    }
}

const COMMAND_INVENTORY: &str = include_str!("../../../../inventory/runtime-shopify-cli.json");

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum CommandColumn {
    Id,
    Plugin,
    Summary,
    Type,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum CommandSort {
    #[default]
    Id,
    Plugin,
    Summary,
    Type,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct CommandRecord {
    name: String,
    id: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    flags: Vec<serde_json::Value>,
    #[serde(default)]
    environment_variables: Vec<String>,
    #[serde(default)]
    summary: String,
    plugin_name: Option<String>,
    plugin_type: Option<String>,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    deprecated: bool,
}

#[derive(serde::Deserialize)]
struct CommandInventory {
    commands: Vec<CommandRecord>,
}

pub(crate) fn print_help(topic: Option<&str>) {
    let mut command = Cli::command();
    if let Some(topic) = topic
        && let Some(subcommand) = command.find_subcommand_mut(topic)
    {
        let _ = subcommand.print_long_help();
        println!();
        return;
    }
    let _ = command.print_long_help();
    println!();
}

pub(crate) fn print_commands(
    columns: Vec<CommandColumn>,
    extended: bool,
    hidden: bool,
    deprecated: bool,
    sort: CommandSort,
    tree: bool,
    output: &Output,
) -> Result<()> {
    let mut commands = serde_json::from_str::<CommandInventory>(COMMAND_INVENTORY)
        .map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                "embedded command inventory is invalid",
                error,
            )
        })?
        .commands;
    commands.retain(|command| (hidden || !command.hidden) && (deprecated || !command.deprecated));
    commands.sort_by(|left, right| {
        let ordering = match sort {
            CommandSort::Id => left.name.cmp(&right.name),
            CommandSort::Plugin => left.plugin_name.cmp(&right.plugin_name),
            CommandSort::Summary => left.summary.cmp(&right.summary),
            CommandSort::Type => left.plugin_type.cmp(&right.plugin_type),
        };
        ordering.then_with(|| left.name.cmp(&right.name))
    });

    let columns = if columns.is_empty() {
        if extended {
            vec![
                CommandColumn::Id,
                CommandColumn::Plugin,
                CommandColumn::Summary,
                CommandColumn::Type,
            ]
        } else {
            vec![CommandColumn::Id, CommandColumn::Summary]
        }
    } else {
        columns
    };
    let human = if tree {
        commands
            .iter()
            .map(|command| {
                let depth = command.name.split_whitespace().count().saturating_sub(1);
                let leaf = command
                    .name
                    .split_whitespace()
                    .last()
                    .unwrap_or(&command.name);
                format!("{}{}\t{}", "  ".repeat(depth), leaf, command.summary)
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        let mut rows = vec![
            columns
                .iter()
                .map(command_column_title)
                .collect::<Vec<_>>()
                .join("\t"),
        ];
        rows.extend(commands.iter().map(|command| {
            columns
                .iter()
                .map(|column| command_column_value(command, *column))
                .collect::<Vec<_>>()
                .join("\t")
        }));
        rows.join("\n")
    };
    output.success(&human, &commands).map_err(|error| {
        Error::with_source(
            cfy_core::ErrorKind::Process,
            "could not write command listing",
            error,
        )
    })
}
