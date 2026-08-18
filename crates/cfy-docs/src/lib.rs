use std::{fs, path::PathBuf, sync::OnceLock};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

const DOCS_ORIGIN: &str = "https://shopify.dev";
const DEFAULT_CACHE_LIMIT: u64 = 32 * 1024 * 1024;
static TLS_PROVIDER: OnceLock<Result<(), String>> = OnceLock::new();

fn install_tls_provider() -> Result<(), DocsError> {
    TLS_PROVIDER
        .get_or_init(|| {
            rustls::crypto::ring::default_provider()
                .install_default()
                .map_err(|_| "a different Rustls crypto provider is already installed".to_owned())
        })
        .clone()
        .map_err(DocsError::Network)
}

#[derive(Debug, Error)]
pub enum DocsError {
    #[error("invalid Shopify developer URL: {0}")]
    InvalidUrl(String),
    #[error("documentation cache path is invalid: {0}")]
    CachePath(String),
    #[error("documentation cache I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("documentation response is invalid: {0}")]
    InvalidResponse(String),
    #[error("documentation network request failed: {0}")]
    Network(String),
    #[error("documentation search is unavailable offline; use a cached result or remove --offline")]
    OfflineMiss,
}

impl From<DocsError> for cfy_core::Error {
    fn from(error: DocsError) -> Self {
        cfy_core::Error::with_source(
            cfy_core::ErrorKind::Api,
            "documentation request failed",
            error,
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub score: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Document {
    pub title: Option<String>,
    pub url: String,
    pub content: String,
}

pub trait DocsTransport: Send + Sync {
    fn search(&self, query: &str) -> Result<Vec<SearchResult>, DocsError>;
    fn fetch(&self, url: &Url) -> Result<String, DocsError>;
}

#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
    max_bytes: u64,
}

impl Cache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_bytes: DEFAULT_CACHE_LIMIT,
        }
    }

    #[must_use]
    pub fn with_limit(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    pub fn clear(&self) -> Result<(), DocsError> {
        if self.root.exists() {
            fs::remove_dir_all(&self.root)?;
        }
        Ok(())
    }

    fn key(url: &str) -> String {
        let mut digest = Sha256::new();
        digest.update(url.as_bytes());
        format!("{:x}.json", digest.finalize())
    }

    fn read(&self, key: &str) -> Result<Option<Document>, DocsError> {
        let path = self.root.join(key);
        if !path.is_file() {
            return Ok(None);
        }
        let bytes = fs::read(path)?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| DocsError::CachePath(format!("corrupt entry: {error}")))
    }

    fn write(&self, key: &str, document: &Document) -> Result<(), DocsError> {
        let bytes = serde_json::to_vec(document)
            .map_err(|error| DocsError::CachePath(format!("encode entry: {error}")))?;
        if bytes.len() as u64 > self.max_bytes {
            return Err(DocsError::CachePath(
                "document exceeds cache size limit".into(),
            ));
        }
        fs::create_dir_all(&self.root)?;
        let temp = self.root.join(format!(".{key}.tmp"));
        fs::write(&temp, &bytes)?;
        fs::rename(temp, self.root.join(key))?;
        self.prune(bytes.len() as u64)?;
        Ok(())
    }

    fn prune(&self, incoming: u64) -> Result<(), DocsError> {
        let mut entries = Vec::new();
        let mut total = 0u64;
        if !self.root.is_dir() {
            return Ok(());
        }
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_file() {
                total += metadata.len();
                entries.push((metadata.modified().ok(), metadata.len(), entry.path()));
            }
        }
        if total <= self.max_bytes {
            return Ok(());
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, size, path) in entries {
            if total <= self.max_bytes {
                break;
            }
            fs::remove_file(path)?;
            total = total.saturating_sub(size);
        }
        let _ = incoming;
        Ok(())
    }
}

pub struct DocsClient<T> {
    transport: T,
    cache: Option<Cache>,
    offline: bool,
}

impl<T: DocsTransport> DocsClient<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            cache: None,
            offline: false,
        }
    }

    #[must_use]
    pub fn with_cache(mut self, cache: Cache) -> Self {
        self.cache = Some(cache);
        self
    }

    #[must_use]
    pub fn offline(mut self, offline: bool) -> Self {
        self.offline = offline;
        self
    }

    pub fn search(&self, query: &str) -> Result<Vec<SearchResult>, DocsError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(DocsError::InvalidResponse(
                "search query cannot be empty".into(),
            ));
        }
        let mut results = self.transport.search(query)?;
        results.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
                .then_with(|| a.url.cmp(&b.url))
        });
        Ok(results)
    }

    pub fn fetch(&self, raw_url: &str) -> Result<Document, DocsError> {
        let url = canonical_url(raw_url)?;
        let key = Cache::key(url.as_str());
        if let Some(cache) = &self.cache
            && let Some(document) = cache.read(&key)?
        {
            return Ok(document);
        }
        if self.offline {
            return Err(DocsError::OfflineMiss);
        }
        let content = self.transport.fetch(&url)?;
        if content.is_empty() {
            return Err(DocsError::InvalidResponse("document body is empty".into()));
        }
        let document = Document {
            title: None,
            url: url.to_string(),
            content,
        };
        if let Some(cache) = &self.cache {
            cache.write(&key, &document)?;
        }
        Ok(document)
    }
}

pub fn canonical_url(raw: &str) -> Result<Url, DocsError> {
    let url = Url::parse(raw).map_err(|error| DocsError::InvalidUrl(error.to_string()))?;
    if url.scheme() != "https" || url.origin().ascii_serialization() != DOCS_ORIGIN {
        return Err(DocsError::InvalidUrl(
            "only https://shopify.dev URLs are allowed".into(),
        ));
    }
    if url.username() != "" || url.password().is_some() || url.port().is_some() {
        return Err(DocsError::InvalidUrl(
            "credentials and custom ports are not allowed".into(),
        ));
    }
    Ok(url)
}

#[derive(Debug, Clone, Default)]
pub struct HttpDocsTransport {
    client: reqwest::blocking::Client,
}

impl HttpDocsTransport {
    pub fn new() -> Result<Self, DocsError> {
        install_tls_provider()?;
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("crabpify/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| DocsError::Network(error.to_string()))?;
        Ok(Self { client })
    }
}

impl DocsTransport for HttpDocsTransport {
    fn search(&self, query: &str) -> Result<Vec<SearchResult>, DocsError> {
        let url =
            Url::parse_with_params("https://shopify.dev/assistant/search", [("query", query)])
                .map_err(|error| DocsError::InvalidUrl(error.to_string()))?;
        let response = self
            .client
            .get(url)
            .send()
            .map_err(|error| DocsError::Network(error.to_string()))?;
        if !response.status().is_success() {
            return Err(DocsError::Network(format!("HTTP {}", response.status())));
        }
        response
            .json()
            .map_err(|error| DocsError::InvalidResponse(error.to_string()))
    }

    fn fetch(&self, url: &Url) -> Result<String, DocsError> {
        let response = self
            .client
            .get(url.clone())
            .send()
            .map_err(|error| DocsError::Network(error.to_string()))?;
        if response.status().is_redirection() {
            return Err(DocsError::Network(
                "redirect refused; use the canonical shopify.dev URL".into(),
            ));
        }
        if response.status().as_u16() == 429 {
            return Err(DocsError::Network(
                "rate limited by shopify.dev; retry later".into(),
            ));
        }
        if !response.status().is_success() {
            return Err(DocsError::Network(format!("HTTP {}", response.status())));
        }
        response
            .text()
            .map_err(|error| DocsError::Network(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Fake {
        fetched: Arc<Mutex<u32>>,
    }

    impl DocsTransport for Fake {
        fn search(&self, _: &str) -> Result<Vec<SearchResult>, DocsError> {
            Ok(vec![
                SearchResult {
                    title: "Zebra".into(),
                    url: "https://shopify.dev/z".into(),
                    snippet: "z".into(),
                    score: 1,
                },
                SearchResult {
                    title: "Alpha".into(),
                    url: "https://shopify.dev/a".into(),
                    snippet: "a".into(),
                    score: 2,
                },
            ])
        }
        fn fetch(&self, url: &Url) -> Result<String, DocsError> {
            *self.fetched.lock().expect("lock") += 1;
            Ok(format!("document for {url}"))
        }
    }

    #[test]
    fn ranking_is_stable() {
        let results = DocsClient::new(Fake::default())
            .search("theme")
            .expect("search");
        assert_eq!(results[0].title, "Alpha");
    }

    #[test]
    fn rejects_non_canonical_urls() {
        assert!(canonical_url("http://shopify.dev/a").is_err());
        assert!(canonical_url("https://evil.example/a").is_err());
        assert!(canonical_url("https://shopify.dev/a").is_ok());
    }

    #[test]
    fn cache_serves_offline_fetch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = Fake::default();
        let client = DocsClient::new(fake.clone()).with_cache(Cache::new(dir.path()));
        client
            .fetch("https://shopify.dev/docs/a")
            .expect("first fetch");
        let offline = DocsClient::new(fake.clone())
            .with_cache(Cache::new(dir.path()))
            .offline(true);
        offline
            .fetch("https://shopify.dev/docs/a")
            .expect("cached fetch");
        assert_eq!(*fake.fetched.lock().expect("lock"), 1);
    }

    #[test]
    fn cache_clear_removes_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = Cache::new(dir.path());
        fs::create_dir_all(&cache.root).expect("mkdir");
        fs::write(cache.root.join("entry"), "x").expect("write");
        cache.clear().expect("clear");
        assert!(!cache.root.exists());
    }
}
