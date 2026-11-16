//! Add shared declaration fields without changing the product manifest data.
//!
//! Product discovery continues decoding its own fields; BMUX ignores those fields
//! and consumes only its generic declaration projection.

use super::{PRODUCT_EVENT_KIND, PluginMetadata};

pub(super) fn shared_metadata(mut metadata: PluginMetadata) -> (PluginMetadata, Option<String>) {
    let Some(manifest) = metadata.manifest_toml.as_deref() else {
        return (metadata, None);
    };
    match project(manifest) {
        Ok(manifest) => {
            metadata.manifest_toml = Some(manifest);
            (metadata, None)
        }
        Err(()) => (
            metadata,
            Some("invalid product plugin declaration".to_owned()),
        ),
    }
}

fn project(manifest: &str) -> Result<String, ()> {
    let mut value: toml::Value = toml::from_str(manifest).map_err(|_| ())?;
    let table = value.as_table_mut().ok_or(())?;
    if let Some(services) = table.get_mut("services") {
        for service in services.as_array_mut().ok_or(())? {
            let service = service.as_table_mut().ok_or(())?;
            if service
                .get("interface_id")
                .and_then(toml::Value::as_str)
                .is_none()
            {
                return Err(());
            }
            // Existing product class controls Bcode scheduling/authorization.
            // Generic invocation remains a command unless a shared kind is declared.
            service
                .entry("capability")
                .or_insert_with(|| toml::Value::String("bcode.product".to_owned()));
            service
                .entry("kind")
                .or_insert_with(|| toml::Value::String("command".to_owned()));
            if let Some(concurrency) = service.get_mut("concurrency") {
                let concurrency = concurrency.as_table_mut().ok_or(())?;
                if !concurrency.contains_key("mode") {
                    let mode = concurrency
                        .get("type")
                        .and_then(toml::Value::as_str)
                        .ok_or(())?
                        .to_owned();
                    concurrency.insert("mode".to_owned(), toml::Value::String(mode));
                }
            }
        }
    }
    if let Some(subscriptions) = table.get_mut("event_subscriptions") {
        for subscription in subscriptions.as_array_mut().ok_or(())? {
            let subscription = subscription.as_table_mut().ok_or(())?;
            if subscription.contains_key("topic") {
                subscription.entry("kinds").or_insert_with(|| {
                    toml::Value::Array(vec![toml::Value::String(PRODUCT_EVENT_KIND.to_owned())])
                });
            }
        }
    }
    let projected = toml::to_string(&value).map_err(|_| ())?;
    // Parse only; authority and dependency policy remain in the shared host.
    toml::from_str::<bmux_plugin_sdk::definition::PluginDeclarations>(&projected)
        .map_err(|_| ())?;
    Ok(projected)
}

#[cfg(test)]
mod tests {
    use super::project;

    #[test]
    fn product_fields_and_generic_projection_coexist() {
        let projected = project(
            r#"
id = "bcode.example"
name = "Example"
version = "1.0.0"
[[services]]
interface_id = "bcode.example/v1"
class = "tool_execution"
name = "Example service"
[[event_subscriptions]]
topic = "session.changed"
timeout_ms = 300
[runtime]
type = "native"
library = "example.dylib"
"#,
        )
        .unwrap();
        let declarations: bmux_plugin_sdk::definition::PluginDeclarations =
            toml::from_str(&projected).unwrap();
        assert_eq!(declarations.services[0].interface_id, "bcode.example/v1");
        assert_eq!(
            declarations.services[0].capability.as_str(),
            "bcode.product"
        );
        let product: toml::Value = toml::from_str(&projected).unwrap();
        assert_eq!(
            product["services"][0]["class"].as_str(),
            Some("tool_execution")
        );
        assert_eq!(
            product["event_subscriptions"][0]["topic"].as_str(),
            Some("session.changed")
        );
        assert_eq!(
            product["runtime"]["library"].as_str(),
            Some("example.dylib")
        );
    }

    #[test]
    fn malformed_product_declarations_are_not_replaced_with_empty_metadata() {
        assert!(project("services = [1]").is_err());
        assert!(project("[[services]]\nname = 'missing interface'").is_err());
    }
}
