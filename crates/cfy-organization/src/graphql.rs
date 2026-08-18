use crate::{Organization, OrganizationBackend, OrganizationError, OrganizationPage};
use async_trait::async_trait;
use cfy_api::{GraphQlClient, GraphQlRequest, HttpClient};
use cfy_auth::Session;
use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};

const ORGANIZATIONS_QUERY: &str = r#"query Organizations($first: Int!, $after: String) {
  organizations(first: $first, after: $after) {
    nodes { id name handle }
    pageInfo { hasNextPage endCursor }
  }
}"#;

#[derive(Clone, Debug, Serialize)]
struct Variables<'a> {
    first: i32,
    after: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct Data {
    organizations: Connection,
}

#[derive(Debug, Deserialize)]
struct Connection {
    nodes: Vec<Node>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

#[derive(Debug, Deserialize)]
struct Node {
    id: String,
    name: String,
    handle: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
}

pub struct ShopifyOrganizationBackend {
    client: GraphQlClient,
}

impl ShopifyOrganizationBackend {
    pub fn new(base_url: &str, session: &Session) -> Result<Self, OrganizationError> {
        let http = HttpClient::new(base_url)
            .map_err(|error| OrganizationError::Backend(error.to_string()))?
            .with_sensitive_header(
                HeaderName::from_static("authorization"),
                HeaderValue::from_static(""),
            );
        let mut http = http;
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", session.access_token.expose()))
            .map_err(|error| {
                OrganizationError::Backend(format!("invalid authorization header: {error}"))
            })?;
        auth.set_sensitive(true);
        http = http.with_sensitive_header(HeaderName::from_static("authorization"), auth);
        Ok(Self {
            client: GraphQlClient::new(http, "/admin/api/graphql.json"),
        })
    }
}

#[async_trait]
impl OrganizationBackend for ShopifyOrganizationBackend {
    async fn list_page(&self, cursor: Option<&str>) -> Result<OrganizationPage, OrganizationError> {
        let request = GraphQlRequest::query(
            ORGANIZATIONS_QUERY,
            Variables {
                first: 50,
                after: cursor,
            },
        );
        let response = self
            .client
            .execute::<_, Data>(&request)
            .await
            .map_err(|error| {
                let message = error.to_string();
                if message.contains("forbidden") || message.contains("unauthorized") {
                    OrganizationError::Forbidden
                } else {
                    OrganizationError::Backend(message)
                }
            })?;
        Ok(OrganizationPage {
            organizations: response
                .data
                .organizations
                .nodes
                .into_iter()
                .map(|node| Organization {
                    id: node.id,
                    name: node.name,
                    handle: node.handle,
                })
                .collect(),
            has_next_page: response.data.organizations.page_info.has_next_page,
            cursor: response.data.organizations.page_info.end_cursor,
        })
    }
}
