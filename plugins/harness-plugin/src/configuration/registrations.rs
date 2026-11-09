//! Durable, plugin-owned installation registration. No credential or readiness persistence.
use bcode_confined_fs::{Creation, Dir, OpenOptions, RenameMode};
use bcode_harness_models::{
    Registration, RegistrationCommit, RegistrationResolution, RegistrationResolveRequest,
    RegistrationSnapshot,
};
use bcode_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    io::{Read, Write},
};

const FILE: &str = "native-registrations.json";
const MAX_BYTES: u64 = 1024 * 1024;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    registrations: BTreeMap<String, Registration>,
}
fn invalid() -> ServiceResponse {
    ServiceResponse::error(
        "registration_unavailable",
        "Native registration state is unavailable, changed or requires maintenance",
    )
}
fn load(root: Option<&Dir>) -> Result<RegistrationSnapshot, ServiceResponse> {
    let mut bytes = Vec::new();
    let mut present = false;
    if let Some(root) = root {
        match root.open_file(OsStr::new(FILE), OpenOptions::new().read(true)) {
            Ok(file) => {
                present = true;
                file.take(MAX_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|_| invalid())?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(invalid()),
        }
    }
    if bytes.len() as u64 > MAX_BYTES {
        return Err(invalid());
    }
    let state = if present {
        serde_json::from_slice::<State>(&bytes).map_err(|_| invalid())?
    } else {
        State {
            version: 1,
            registrations: BTreeMap::new(),
        }
    };
    if state.version != 1 || state.registrations.len() > 128 {
        return Err(invalid());
    }
    for (id, entry) in &state.registrations {
        validate(entry)?;
        if id != &entry.instance.instance_id {
            return Err(invalid());
        }
    }
    Ok(RegistrationSnapshot {
        revision: Sha256::digest(&bytes)
            .iter()
            .fold(String::new(), |mut text, byte| {
                use std::fmt::Write as _;
                let _ = write!(text, "{byte:02x}");
                text
            }),
        registrations: state.registrations,
    })
}
fn validate(entry: &Registration) -> Result<(), ServiceResponse> {
    entry.instance.validate().map_err(|_| invalid())?;
    if !entry.adapter_settings.is_object()
        || serde_json::to_vec(&entry.adapter_settings)
            .map_err(|_| invalid())?
            .len()
            > 64 * 1024
    {
        return Err(invalid());
    }
    Ok(())
}
fn root(context: &NativeServiceContext, create: bool) -> Result<Option<Dir>, ServiceResponse> {
    let Some(path) = &context.config.durable_state_root else {
        return if create { Err(invalid()) } else { Ok(None) };
    };
    if create {
        std::fs::create_dir_all(path).map_err(|_| invalid())?;
    }
    match Dir::open(path) {
        Ok(dir) => Ok(Some(dir)),
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(invalid()),
    }
}
fn resolve(
    config: &serde_json::Value,
    request: RegistrationResolveRequest,
    snapshot: RegistrationSnapshot,
) -> Result<RegistrationResolution, ServiceResponse> {
    let mut generic = if config.is_null() {
        serde_json::json!({})
    } else {
        config.clone()
    };
    let disabled = generic.get("enabled").and_then(serde_json::Value::as_bool) == Some(false);
    let object = generic.as_object_mut().ok_or_else(invalid)?;
    object.entry("enabled").or_insert(serde_json::json!(true));
    let instances = object
        .entry("instances")
        .or_insert(serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(invalid)?;
    let mut configurations = request.adapter_configurations;
    let mut origins = BTreeMap::new();
    let mut shadowed = Vec::new();
    for id in instances.keys() {
        origins.insert(id.clone(), "configured".into());
    }
    for (id, entry) in snapshot.registrations {
        if let Some(declared) = instances.get(&id) {
            if declared.get("instance")
                != Some(&serde_json::to_value(&entry.instance).map_err(|_| invalid())?)
                || configurations
                    .get(&entry.instance.plugin_id)
                    .and_then(|value| value.get("instances"))
                    .and_then(|value| value.get(&id))
                    != Some(&entry.adapter_settings)
            {
                shadowed.push(id);
            }
            continue;
        }
        if disabled {
            continue;
        }
        let Some(adapter) = configurations.get_mut(&entry.instance.plugin_id) else {
            continue;
        };
        if adapter.is_null() {
            *adapter = serde_json::json!({});
        }
        let adapter_entries = adapter
            .as_object_mut()
            .ok_or_else(invalid)?
            .entry("instances")
            .or_insert(serde_json::json!({}))
            .as_object_mut()
            .ok_or_else(invalid)?;
        // An adapter declaration is also authoritative: never splice a local generic identity
        // into different declared executable/account settings.
        if adapter_entries.contains_key(&id) {
            shadowed.push(id);
            continue;
        }
        adapter_entries.insert(id.clone(), entry.adapter_settings);
        instances.insert(
            id.clone(),
            serde_json::json!({"enabled":true,"instance":entry.instance}),
        );
        origins.insert(id, "local".into());
    }
    configurations.insert(crate::PLUGIN_ID.into(), generic);
    Ok(RegistrationResolution {
        revision: snapshot.revision,
        configurations,
        origins,
        shadowed,
    })
}
pub(super) fn invoke(context: &NativeServiceContext) -> ServiceResponse {
    let result = (|| match context.request.operation.as_str() {
        "registrations_snapshot" => {
            let root = root(context, false)?;
            Ok(crate::json_response(&load(root.as_ref())?))
        }
        "registrations_resolve" => {
            let request = context
                .request
                .payload_json::<RegistrationResolveRequest>()
                .map_err(|_| invalid())?;
            let root = root(context, false)?;
            Ok(crate::json_response(&resolve(
                &context.config.config,
                request,
                load(root.as_ref())?,
            )?))
        }
        "registrations_commit" => {
            let request = context
                .request
                .payload_json::<RegistrationCommit>()
                .map_err(|_| invalid())?;
            validate(&request.registration)?;
            if context.config.config.get("enabled") == Some(&serde_json::json!(false))
                || context
                    .config
                    .config
                    .get("instances")
                    .and_then(|v| v.get(&request.registration.instance.instance_id))
                    .is_some()
            {
                return Err(invalid());
            }
            let dir = root(context, true)?.ok_or_else(invalid)?;
            let lock = dir
                .open_file(
                    OsStr::new("registrations.lock"),
                    OpenOptions::new()
                        .read(true)
                        .write(true)
                        .creation(Creation::IfMissing),
                )
                .map_err(|_| invalid())?;
            lock.lock().map_err(|_| invalid())?;
            let mut snapshot = load(Some(&dir))?;
            if snapshot.revision != request.expected_revision {
                return Err(invalid());
            }
            snapshot.registrations.insert(
                request.registration.instance.instance_id.clone(),
                request.registration,
            );
            let bytes = serde_json::to_vec(&State {
                version: 1,
                registrations: snapshot.registrations,
            })
            .map_err(|_| invalid())?;
            if bytes.len() as u64 > MAX_BYTES {
                return Err(invalid());
            }
            let name = format!("registrations-{}.tmp", uuid::Uuid::new_v4());
            let mut file = dir
                .open_file(
                    OsStr::new(&name),
                    OpenOptions::new()
                        .write(true)
                        .creation(Creation::New)
                        .private(true),
                )
                .map_err(|_| invalid())?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|_| invalid())?;
            dir.rename(
                OsStr::new(&name),
                &dir,
                OsStr::new(FILE),
                RenameMode::Replace,
            )
            .map_err(|_| invalid())?;
            dir.sync().map_err(|_| invalid())?;
            Ok(crate::json_response(&load(Some(&dir))?))
        }
        _ => Err(invalid()),
    })();
    result.unwrap_or_else(|error| error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn entry() -> Registration {
        serde_json::from_value(json!({"instance":{"instance_id":"claude", "plugin_id":"example.native", "executable":"/bin/native", "config_namespace":"/native", "auth":{"provider_id":"native", "profile":"native", "credential_namespace":"/native", "custody":"native_managed"}}, "adapter_settings":{"executable":"/bin/native","config_directory":"/native"}})).unwrap()
    }
    fn snapshot() -> RegistrationSnapshot {
        RegistrationSnapshot {
            revision: "r".into(),
            registrations: BTreeMap::from([("claude".into(), entry())]),
        }
    }
    #[test]
    fn declarations_win_as_whole_instances_and_disable_is_authoritative() {
        let request = || RegistrationResolveRequest {
            adapter_configurations: BTreeMap::from([("example.native".into(), json!({}))]),
        };
        let local = resolve(&json!({}), request(), snapshot()).unwrap();
        assert_eq!(local.origins["claude"], "local");
        assert_eq!(
            local.configurations["example.native"]["instances"]["claude"],
            entry().adapter_settings
        );
        let declared = json!({"enabled":true,"instances":{"claude":{"enabled":false,"instance":entry().instance}}});
        let result = resolve(&declared, request(), snapshot()).unwrap();
        assert_eq!(result.configurations["bcode.harness"], declared);
        assert!(
            result.configurations["example.native"]
                .get("instances")
                .is_none()
        );
        let disabled = resolve(&json!({"enabled":false}), request(), snapshot()).unwrap();
        assert!(
            disabled.configurations["bcode.harness"]["instances"]
                .as_object()
                .unwrap()
                .is_empty()
        );
        let mut foreign = entry();
        foreign.instance.config_namespace = "/different".into();
        let declared = json!({"enabled":true,"instances":{"claude":{"enabled":true,"instance":foreign.instance}}});
        let result = resolve(&declared, request(), snapshot()).unwrap();
        assert_eq!(result.shadowed, vec!["claude"]);
        assert_eq!(result.configurations["bcode.harness"], declared);
    }
    #[test]
    fn missing_read_is_non_mutating_and_future_corrupt_or_linked_state_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let dir = Dir::open(temp.path()).unwrap();
        assert!(load(Some(&dir)).unwrap().registrations.is_empty());
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
        for contents in ["", "{", "{\"version\":2,\"registrations\":{}}"] {
            std::fs::write(temp.path().join(FILE), contents).unwrap();
            assert!(load(Some(&dir)).is_err());
            assert_eq!(
                std::fs::read_to_string(temp.path().join(FILE)).unwrap(),
                contents
            );
        }
        #[cfg(unix)]
        {
            std::fs::remove_file(temp.path().join(FILE)).unwrap();
            let target = temp.path().join("outside");
            std::fs::write(&target, "{}").unwrap();
            std::os::unix::fs::symlink(&target, temp.path().join(FILE)).unwrap();
            assert!(load(Some(&dir)).is_err());
        }
    }
    #[test]
    fn commit_is_revision_checked_durable_and_confined() {
        let temp = tempfile::tempdir().unwrap();
        let mut context =
            super::super::tests::context(json!({}), "registrations_commit", &json!({}));
        context.config.durable_state_root = Some(temp.path().to_path_buf());
        let initial = load(None).unwrap();
        let request = RegistrationCommit {
            expected_revision: initial.revision,
            registration: entry(),
        };
        context.request.payload = serde_json::to_vec(&request).unwrap();
        let response = invoke(&context);
        assert!(response.error.is_none(), "{:?}", response.error);
        let stored: RegistrationSnapshot = serde_json::from_slice(&response.payload).unwrap();
        assert_eq!(stored.registrations["claude"], entry());
        assert!(invoke(&context).error.is_some());
        let dir = Dir::open(temp.path()).unwrap();
        assert_eq!(load(Some(&dir)).unwrap().revision, stored.revision);
        assert!(temp.path().join(FILE).is_file());
    }
}
