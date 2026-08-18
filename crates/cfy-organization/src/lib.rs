//! Organization listing contracts and stable output models.

pub mod graphql;

use async_trait::async_trait;
use cfy_core::{Error, ErrorKind};
use serde::{Deserialize, Serialize};
use thiserror::Error as ThisError;

type Result<T> = std::result::Result<T, OrganizationError>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Organization {
    pub id: String,
    pub name: String,
    pub handle: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OrganizationPage {
    pub organizations: Vec<Organization>,
    pub has_next_page: bool,
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OrganizationList {
    pub organizations: Vec<Organization>,
    pub pages: usize,
}

#[derive(Debug, ThisError)]
pub enum OrganizationError {
    #[error(
        "organization access is denied; verify the logged-in account has organization read permission"
    )]
    Forbidden,
    #[error("organization API returned an invalid page: {0}")]
    InvalidPage(String),
    #[error("organization listing failed: {0}")]
    Backend(String),
}

impl From<OrganizationError> for Error {
    fn from(error: OrganizationError) -> Self {
        let kind = match error {
            OrganizationError::Forbidden => ErrorKind::Api,
            OrganizationError::InvalidPage(_) => ErrorKind::Api,
            OrganizationError::Backend(_) => ErrorKind::Api,
        };
        Error::new(kind, error.to_string())
    }
}

#[async_trait]
pub trait OrganizationBackend: Send + Sync {
    async fn list_page(&self, cursor: Option<&str>) -> Result<OrganizationPage>;
}

pub async fn list_all<B: OrganizationBackend>(backend: &B) -> Result<OrganizationList> {
    let mut organizations = Vec::new();
    let mut cursor = None;
    let mut pages = 0;
    loop {
        let page = backend.list_page(cursor.as_deref()).await?;
        pages += 1;
        organizations.extend(page.organizations);
        if !page.has_next_page {
            break;
        }
        let Some(next) = page.cursor.filter(|value| !value.is_empty()) else {
            return Err(OrganizationError::InvalidPage(
                "has_next_page was true without a cursor".into(),
            ));
        };
        cursor = Some(next);
    }
    organizations.sort_by(|left, right| left.name.to_lowercase().cmp(&right.name.to_lowercase()));
    Ok(OrganizationList {
        organizations,
        pages,
    })
}

pub fn human_lines(list: &OrganizationList) -> String {
    if list.organizations.is_empty() {
        return "No organizations found.".into();
    }
    list.organizations
        .iter()
        .map(|organization| {
            format!(
                "{}\t{}",
                organization.name,
                organization.handle.as_deref().unwrap_or("-")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Fake {
        calls: Mutex<Vec<Option<String>>>,
    }
    #[async_trait]
    impl OrganizationBackend for Fake {
        async fn list_page(&self, cursor: Option<&str>) -> Result<OrganizationPage> {
            self.calls.lock().unwrap().push(cursor.map(str::to_owned));
            if cursor.is_none() {
                Ok(OrganizationPage {
                    organizations: vec![Organization {
                        id: "2".into(),
                        name: "Zulu".into(),
                        handle: None,
                    }],
                    has_next_page: true,
                    cursor: Some("next".into()),
                })
            } else {
                Ok(OrganizationPage {
                    organizations: vec![Organization {
                        id: "1".into(),
                        name: "Acme".into(),
                        handle: Some("acme".into()),
                    }],
                    has_next_page: false,
                    cursor: None,
                })
            }
        }
    }

    #[tokio::test]
    async fn lists_pages_sorts_and_formats_stably() {
        let list = list_all(&Fake {
            calls: Mutex::new(Vec::new()),
        })
        .await
        .unwrap();
        assert_eq!(list.pages, 2);
        assert_eq!(list.organizations[0].name, "Acme");
        assert_eq!(human_lines(&list), "Acme\tacme\nZulu\t-");
    }

    #[test]
    fn malformed_pagination_is_actionable() {
        let error = OrganizationError::InvalidPage("missing cursor".into());
        assert!(error.to_string().contains("cursor"));
    }
}
