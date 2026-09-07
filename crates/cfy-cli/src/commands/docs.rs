use super::super::output::Output;
use cfy_core::{Error, Result};
use cfy_docs::{Cache as DocsCache, DocsClient, HttpDocsTransport};
use clap::Subcommand;
use std::{env, path::PathBuf};

pub(super) fn docs_cache_root() -> PathBuf {
    env::var_os("CFY_CACHE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("XDG_CACHE_HOME").map(|path| PathBuf::from(path).join("catify/docs"))
        })
        .or_else(|| env::var_os("HOME").map(|path| PathBuf::from(path).join(".cache/catify/docs")))
        .unwrap_or_else(|| PathBuf::from(".catify-cache/docs"))
}

fn docs_cache() -> DocsCache {
    DocsCache::new(docs_cache_root())
}

fn docs_client() -> Result<DocsClient<HttpDocsTransport>> {
    Ok(DocsClient::new(HttpDocsTransport::new()?))
}

pub(crate) fn docs_command(command: DocCommand, output: &Output) -> Result<u8> {
    match command {
        DocCommand::ClearCache => {
            docs_cache().clear()?;
            output
                .success(
                    "Documentation cache cleared",
                    &serde_json::json!({"cleared": true}),
                )
                .map_err(|error| Error::process(error.to_string()))?;
        }
        DocCommand::Search { query } => {
            let query = query.join(" ");
            let results = docs_client()?.with_cache(docs_cache()).search(&query)?;
            let human = if results.is_empty() {
                "No documentation results found".to_owned()
            } else {
                results
                    .iter()
                    .map(|result| {
                        let snippet = result
                            .snippet
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ");
                        format!("{}\n{}\n{}", result.title, result.url, snippet)
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n")
            };
            output
                .success(&human, &results)
                .map_err(|error| Error::process(error.to_string()))?;
        }
        DocCommand::Fetch { url } => {
            let document = docs_client()?.with_cache(docs_cache()).fetch(&url)?;
            output
                .success(&document.url, &document)
                .map_err(|error| Error::process(error.to_string()))?;
        }
    }
    Ok(0)
}

#[derive(Debug, Subcommand)]
pub enum DocCommand {
    /// Search Shopify developer documentation.
    Search { query: Vec<String> },
    /// Fetch a complete document from shopify.dev.
    Fetch { url: String },
    /// Clear the local documentation cache.
    ClearCache,
}
