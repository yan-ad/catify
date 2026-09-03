use crate::{AdminStoreBackend, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

const OWNER_TYPES: &[(&str, &str)] = &[
    ("article", "ARTICLE"),
    ("blog", "BLOG"),
    ("cart_transform", "CARTTRANSFORM"),
    ("collection", "COLLECTION"),
    ("company", "COMPANY"),
    ("company_location", "COMPANY_LOCATION"),
    ("customer", "CUSTOMER"),
    ("delivery_customization", "DELIVERY_CUSTOMIZATION"),
    ("delivery_method", "DELIVERY_METHOD"),
    ("delivery_option_generator", "DELIVERY_OPTION_GENERATOR"),
    ("discount", "DISCOUNT"),
    ("draft_order", "DRAFTORDER"),
    ("fulfillment_constraint_rule", "FULFILLMENT_CONSTRAINT_RULE"),
    ("gift_card_transaction", "GIFT_CARD_TRANSACTION"),
    ("location", "LOCATION"),
    ("market", "MARKET"),
    ("order", "ORDER"),
    ("order_routing_location_rule", "ORDER_ROUTING_LOCATION_RULE"),
    ("page", "PAGE"),
    ("payment_customization", "PAYMENT_CUSTOMIZATION"),
    ("product", "PRODUCT"),
    ("selling_plan", "SELLING_PLAN"),
    ("shop", "SHOP"),
    ("validation", "VALIDATION"),
    ("variant", "PRODUCTVARIANT"),
];

const METAFIELD_QUERY: &str = r#"
query metafieldDefinitions($ownerType: MetafieldOwnerType!, $after: String) {
  metafieldDefinitions(ownerType: $ownerType, first: 30, after: $after) {
    pageInfo { hasNextPage endCursor }
    nodes {
      key name namespace description
      type { category name }
      access { admin storefront customerAccount }
      capabilities { adminFilterable { enabled } }
      validations { name value }
    }
  }
}
"#;

const METAOBJECT_QUERY: &str = r#"
query metaobjectDefinitions($after: String) {
  metaobjectDefinitions(first: 10, after: $after) {
    pageInfo { hasNextPage endCursor }
    nodes {
      type name description displayNameKey
      access { admin storefront }
      capabilities {
        publishable { enabled }
        translatable { enabled }
        renderable { enabled data { metaTitleKey metaDescriptionKey } }
      }
      fieldDefinitions {
        key name description required
        type { category name }
        validations { name value }
      }
    }
  }
}
"#;

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExistingDefinitions {
    pub metafields: BTreeSet<(String, String, String)>,
    pub metaobjects: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImportDefinitionsReport {
    pub store: String,
    pub metafield_count: usize,
    pub metaobject_count: usize,
    pub skipped_scope_owner_types: Vec<String>,
    pub toml: String,
}

pub async fn import_definitions(
    backend: &AdminStoreBackend,
    store: &str,
    existing: &ExistingDefinitions,
    include_existing: bool,
) -> Result<ImportDefinitionsReport, StoreError> {
    let mut document = toml::Table::new();
    let mut metafield_count = 0;
    let mut skipped_scope_owner_types = Vec::new();

    for (owner, graph_owner) in OWNER_TYPES {
        match load_connection(
            backend,
            METAFIELD_QUERY,
            "metafieldDefinitions",
            serde_json::json!({"ownerType": graph_owner}),
        )
        .await
        {
            Ok(nodes) => {
                for node in nodes {
                    let Some(namespace) = node
                        .get("namespace")
                        .and_then(Value::as_str)
                        .and_then(simplify_namespace)
                    else {
                        continue;
                    };
                    let Some(key) = node.get("key").and_then(Value::as_str) else {
                        continue;
                    };
                    if !include_existing
                        && existing.metafields.contains(&(
                            owner.to_string(),
                            namespace.clone(),
                            key.to_string(),
                        ))
                    {
                        continue;
                    }
                    insert_metafield(&mut document, owner, &namespace, key, &node)?;
                    metafield_count += 1;
                }
            }
            Err(StoreError::Backend(message)) if message.contains("ACCESS_DENIED") => {
                skipped_scope_owner_types.push((*owner).to_owned())
            }
            Err(error) => return Err(error),
        }
    }

    let mut metaobject_count = 0;
    for node in load_connection(
        backend,
        METAOBJECT_QUERY,
        "metaobjectDefinitions",
        serde_json::json!({}),
    )
    .await?
    {
        let Some(type_name) = node
            .get("type")
            .and_then(Value::as_str)
            .and_then(simplify_namespace)
        else {
            continue;
        };
        if !include_existing && existing.metaobjects.contains(&type_name) {
            continue;
        }
        insert_metaobject(&mut document, &type_name, &node)?;
        metaobject_count += 1;
    }

    let toml = toml::to_string_pretty(&document).map_err(|error| {
        StoreError::Backend(format!("could not render declarative definitions: {error}"))
    })?;
    Ok(ImportDefinitionsReport {
        store: store.to_owned(),
        metafield_count,
        metaobject_count,
        skipped_scope_owner_types,
        toml,
    })
}

async fn load_connection(
    backend: &AdminStoreBackend,
    query: &str,
    field: &str,
    base_variables: Value,
) -> Result<Vec<Value>, StoreError> {
    let mut cursor: Option<String> = None;
    let mut all = Vec::new();
    loop {
        let mut variables = base_variables.as_object().cloned().unwrap_or_default();
        variables.insert(
            "after".into(),
            cursor.clone().map(Value::String).unwrap_or(Value::Null),
        );
        let data = backend
            .execute_with_variables(query, Value::Object(variables))
            .await?;
        let connection = data
            .get(field)
            .ok_or_else(|| StoreError::Backend(format!("response omitted {field}")))?;
        all.extend(
            connection
                .get("nodes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        );
        let page = connection.get("pageInfo").and_then(Value::as_object);
        if !page
            .and_then(|value| value.get("hasNextPage"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            break;
        }
        cursor = page
            .and_then(|value| value.get("endCursor"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        if cursor.is_none() {
            return Err(StoreError::Backend(format!(
                "{field} indicated another page without an end cursor"
            )));
        }
    }
    Ok(all)
}

fn simplify_namespace(value: &str) -> Option<String> {
    let rest = value.strip_prefix("app--")?;
    let (_, suffix) = rest.split_once("--").unwrap_or((rest, "app"));
    Some(if suffix.is_empty() {
        "app".into()
    } else {
        suffix.into()
    })
}

fn insert_metafield(
    table: &mut toml::Table,
    owner: &str,
    namespace: &str,
    key: &str,
    node: &Value,
) -> Result<(), StoreError> {
    let mut definition = toml::Table::new();
    definition.insert(
        "type".into(),
        toml::Value::String(
            string_at(node, "/type/name")
                .unwrap_or("single_line_text_field")
                .into(),
        ),
    );
    insert_distinct_string(&mut definition, "name", node.get("name"), Some(key));
    insert_string(&mut definition, "description", node.get("description"));
    insert_access(&mut definition, node.get("access"), true);
    if node
        .pointer("/capabilities/adminFilterable/enabled")
        .and_then(Value::as_bool)
        == Some(true)
    {
        definition.insert(
            "capabilities".into(),
            table_value([("admin_filterable", toml::Value::Boolean(true))]),
        );
    }
    insert_validations(&mut definition, node.get("validations"), None);
    insert_nested(
        table,
        &[owner, "metafields", namespace, key],
        toml::Value::Table(definition),
    )
}

fn insert_metaobject(
    table: &mut toml::Table,
    type_name: &str,
    node: &Value,
) -> Result<(), StoreError> {
    let mut definition = toml::Table::new();
    insert_distinct_string(&mut definition, "name", node.get("name"), Some(type_name));
    insert_string(&mut definition, "description", node.get("description"));
    insert_string(
        &mut definition,
        "display_name_field",
        node.get("displayNameKey"),
    );
    insert_access(&mut definition, node.get("access"), false);
    let mut capabilities = toml::Table::new();
    for (source, target) in [
        ("publishable", "publishable"),
        ("translatable", "translatable"),
        ("renderable", "renderable"),
    ] {
        if node
            .pointer(&format!("/capabilities/{source}/enabled"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            capabilities.insert(target.into(), toml::Value::Boolean(true));
        }
    }
    if !capabilities.is_empty() {
        definition.insert("capabilities".into(), toml::Value::Table(capabilities));
    }
    let mut fields = toml::Table::new();
    for field in node
        .get("fieldDefinitions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(key) = field.get("key").and_then(Value::as_str) else {
            continue;
        };
        let mut rendered = toml::Table::new();
        let (field_type, remove_validation) = reference_type(field);
        rendered.insert("type".into(), toml::Value::String(field_type));
        insert_distinct_string(&mut rendered, "name", field.get("name"), Some(key));
        insert_string(&mut rendered, "description", field.get("description"));
        if field.get("required").and_then(Value::as_bool) == Some(true) {
            rendered.insert("required".into(), toml::Value::Boolean(true));
        }
        insert_validations(
            &mut rendered,
            field.get("validations"),
            remove_validation.as_deref(),
        );
        fields.insert(key.into(), toml::Value::Table(rendered));
    }
    if !fields.is_empty() {
        definition.insert("fields".into(), toml::Value::Table(fields));
    }
    insert_nested(
        table,
        &["metaobjects", "app", type_name],
        toml::Value::Table(definition),
    )
}

fn reference_type(field: &Value) -> (String, Option<String>) {
    let base = string_at(field, "/type/name").unwrap_or("single_line_text_field");
    for validation in field
        .get("validations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = validation.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(raw) = validation.get("value").and_then(Value::as_str) else {
            continue;
        };
        if name == "metaobject_definition_type"
            && let Some(target) = simplify_namespace(raw)
        {
            return (format!("{base}<$app:{target}>"), Some(name.into()));
        }
        if name == "metaobject_definition_types"
            && let Ok(values) = serde_json::from_str::<Vec<String>>(raw)
        {
            let targets = values
                .into_iter()
                .filter_map(|value| simplify_namespace(&value).map(|value| format!("$app:{value}")))
                .collect::<Vec<_>>();
            if !targets.is_empty() {
                return (format!("{base}<{}>", targets.join(",")), Some(name.into()));
            }
        }
    }
    (base.into(), None)
}

fn insert_access(target: &mut toml::Table, access: Option<&Value>, customer: bool) {
    let Some(access) = access.and_then(Value::as_object) else {
        return;
    };
    let mut rendered = toml::Table::new();
    for (source, key) in [
        ("admin", "admin"),
        ("storefront", "storefront"),
        ("customerAccount", "customer_account"),
    ] {
        if !customer && source == "customerAccount" {
            continue;
        }
        if let Some(value) = access
            .get(source)
            .and_then(Value::as_str)
            .and_then(map_access)
        {
            rendered.insert(key.into(), toml::Value::String(value));
        }
    }
    if !rendered.is_empty() {
        target.insert("access".into(), toml::Value::Table(rendered));
    }
}

fn map_access(value: &str) -> Option<String> {
    match value {
        "MERCHANT_READ" => Some("merchant_read".into()),
        "MERCHANT_READ_WRITE" => Some("merchant_read_write".into()),
        "PUBLIC_READ" => Some("public_read".into()),
        "READ" => Some("read".into()),
        "READ_WRITE" => Some("read_write".into()),
        "NONE" => None,
        other => Some(other.to_ascii_lowercase()),
    }
}

fn insert_validations(target: &mut toml::Table, validations: Option<&Value>, skip: Option<&str>) {
    let mut rendered = toml::Table::new();
    for validation in validations.and_then(Value::as_array).into_iter().flatten() {
        let Some(name) = validation.get("name").and_then(Value::as_str) else {
            continue;
        };
        if skip == Some(name)
            || matches!(
                name,
                "metaobject_definition_id" | "metaobject_definition_ids"
            )
        {
            continue;
        }
        let Some(raw) = validation.get("value").and_then(Value::as_str) else {
            continue;
        };
        let value = serde_json::from_str::<Value>(raw)
            .ok()
            .and_then(json_to_toml)
            .unwrap_or_else(|| toml::Value::String(raw.into()));
        rendered.insert(name.into(), value);
    }
    if !rendered.is_empty() {
        target.insert("validations".into(), toml::Value::Table(rendered));
    }
}

fn json_to_toml(value: Value) -> Option<toml::Value> {
    match value {
        Value::Bool(value) => Some(toml::Value::Boolean(value)),
        Value::Number(value) => value
            .as_i64()
            .map(toml::Value::Integer)
            .or_else(|| value.as_f64().map(toml::Value::Float)),
        Value::String(value) => Some(toml::Value::String(value)),
        Value::Array(values) => Some(toml::Value::Array(
            values.into_iter().filter_map(json_to_toml).collect(),
        )),
        _ => None,
    }
}

fn insert_nested(
    root: &mut toml::Table,
    path: &[&str],
    value: toml::Value,
) -> Result<(), StoreError> {
    let Some((last, parents)) = path.split_last() else {
        return Ok(());
    };
    let mut current = root;
    for segment in parents {
        let entry = current
            .entry((*segment).to_owned())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        current = entry.as_table_mut().ok_or_else(|| {
            StoreError::Backend(format!(
                "TOML path {} conflicts with an existing value",
                path.join(".")
            ))
        })?;
    }
    current.insert((*last).into(), value);
    Ok(())
}

fn insert_string(target: &mut toml::Table, key: &str, value: Option<&Value>) {
    if let Some(value) = value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        target.insert(key.into(), toml::Value::String(value.into()));
    }
}
fn insert_distinct_string(
    target: &mut toml::Table,
    key: &str,
    value: Option<&Value>,
    same_as: Option<&str>,
) {
    if let Some(value) = value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && Some(*value) != same_as)
    {
        target.insert(key.into(), toml::Value::String(value.into()));
    }
}
fn string_at<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(Value::as_str)
}
fn table_value<const N: usize>(entries: [(&str, toml::Value); N]) -> toml::Value {
    toml::Value::Table(
        entries
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}

pub fn existing_definitions(document: &toml::Value) -> ExistingDefinitions {
    let mut result = ExistingDefinitions::default();
    let Some(root) = document.as_table() else {
        return result;
    };
    for (owner, _) in OWNER_TYPES {
        let Some(namespaces) = root
            .get(*owner)
            .and_then(toml::Value::as_table)
            .and_then(|owner| owner.get("metafields"))
            .and_then(toml::Value::as_table)
        else {
            continue;
        };
        for (namespace, keys) in namespaces {
            let Some(keys) = keys.as_table() else {
                continue;
            };
            for key in keys.keys() {
                result
                    .metafields
                    .insert(((*owner).into(), namespace.clone(), key.clone()));
            }
        }
    }
    if let Some(types) = root
        .get("metaobjects")
        .and_then(toml::Value::as_table)
        .and_then(|value| value.get("app"))
        .and_then(toml::Value::as_table)
    {
        result.metaobjects.extend(types.keys().cloned());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[test]
    fn simplifies_only_app_reserved_namespaces() {
        assert_eq!(simplify_namespace("app--123"), Some("app".into()));
        assert_eq!(
            simplify_namespace("app--123--custom"),
            Some("custom".into())
        );
        assert_eq!(simplify_namespace("custom"), None);
    }

    #[test]
    fn renders_metafields_metaobjects_and_reference_types() {
        let mut table = toml::Table::new();
        insert_metafield(&mut table, "product", "app", "color", &serde_json::json!({
            "key":"color","name":"Color","description":"Product color","type":{"name":"single_line_text_field"},
            "access":{"admin":"MERCHANT_READ_WRITE","storefront":"PUBLIC_READ","customerAccount":"NONE"},
            "capabilities":{"adminFilterable":{"enabled":true}},"validations":[{"name":"choices","value":"[\"red\",\"blue\"]"}]
        })).unwrap();
        insert_metaobject(&mut table, "author", &serde_json::json!({
            "type":"app--1--author","name":"Author","displayNameKey":"name","access":{"admin":"MERCHANT_READ","storefront":"PUBLIC_READ"},
            "capabilities":{"publishable":{"enabled":true},"translatable":{"enabled":false}},
            "fieldDefinitions":[{"key":"parent","name":"Parent","required":true,"type":{"name":"metaobject_reference"},"validations":[{"name":"metaobject_definition_type","value":"app--1--author"}]}]
        })).unwrap();
        let rendered = toml::to_string(&table).unwrap();
        assert!(rendered.contains("[product.metafields.app.color]"));
        assert!(rendered.contains("admin_filterable = true"));
        assert!(rendered.contains("[metaobjects.app.author.fields.parent]"));
        assert!(rendered.contains("type = \"metaobject_reference<$app:author>\""));
    }

    #[test]
    fn finds_existing_declarative_definitions() {
        let document: toml::Value = toml::from_str(
            "[product.metafields.app.color]\ntype='single_line_text_field'\n[metaobjects.app.author]\nname='Author'",
        )
        .unwrap();
        let existing = existing_definitions(&document);
        assert!(
            existing
                .metafields
                .contains(&("product".into(), "app".into(), "color".into()))
        );
        assert!(existing.metaobjects.contains("author"));
    }

    #[tokio::test]
    async fn imports_only_app_reserved_and_undeclared_definitions() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..=OWNER_TYPES.len() {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 64 * 1024];
                let read = socket.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                let body = if request.contains("metaobjectDefinitions") {
                    serde_json::json!({"data":{"metaobjectDefinitions":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[
                        {"type":"app--9--author","name":"Author","description":null,"displayNameKey":"name","access":{"admin":"MERCHANT_READ","storefront":"PUBLIC_READ"},"capabilities":{"publishable":{"enabled":false},"translatable":{"enabled":false},"renderable":null},"fieldDefinitions":[]},
                        {"type":"custom","name":"Ignored","description":null,"displayNameKey":null,"access":{"admin":"MERCHANT_READ","storefront":"NONE"},"capabilities":{"publishable":{"enabled":false},"translatable":{"enabled":false},"renderable":null},"fieldDefinitions":[]}
                    ]}}}).to_string()
                } else if request.contains("\"ownerType\":\"PRODUCT\"") {
                    serde_json::json!({"data":{"metafieldDefinitions":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[
                        {"key":"existing","name":"Existing","namespace":"app--9","description":null,"type":{"category":"TEXT","name":"single_line_text_field"},"access":{"admin":"MERCHANT_READ","storefront":"NONE","customerAccount":"NONE"},"capabilities":{"adminFilterable":{"enabled":false}},"validations":[]},
                        {"key":"subtitle","name":"Subtitle","namespace":"app--9","description":"Text","type":{"category":"TEXT","name":"single_line_text_field"},"access":{"admin":"MERCHANT_READ_WRITE","storefront":"PUBLIC_READ","customerAccount":"NONE"},"capabilities":{"adminFilterable":{"enabled":false}},"validations":[]},
                        {"key":"ignored","name":"Ignored","namespace":"custom","description":null,"type":{"category":"TEXT","name":"single_line_text_field"},"access":{"admin":"MERCHANT_READ","storefront":"NONE","customerAccount":"NONE"},"capabilities":{"adminFilterable":{"enabled":false}},"validations":[]}
                    ]}}}).to_string()
                } else {
                    serde_json::json!({"data":{"metafieldDefinitions":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]}}}).to_string()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let backend =
            AdminStoreBackend::new_at(&format!("http://{address}/graphql"), "secret").unwrap();
        let existing = ExistingDefinitions {
            metafields: BTreeSet::from([("product".into(), "app".into(), "existing".into())]),
            metaobjects: BTreeSet::new(),
        };
        let report = import_definitions(&backend, "demo.myshopify.com", &existing, false)
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(report.metafield_count, 1);
        assert_eq!(report.metaobject_count, 1);
        assert!(report.toml.contains("[product.metafields.app.subtitle]"));
        assert!(!report.toml.contains("existing"));
        assert!(!report.toml.contains("ignored"));
        assert!(report.toml.contains("[metaobjects.app.author]"));
    }
}
