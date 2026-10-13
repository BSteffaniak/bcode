use super::*;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};

fn input(directory: PathBuf) -> PrepareRequest {
    PrepareRequest {
        directory,
        broker_url: "https://broker.example".into(),
        audience: "bcode".into(),
        timeout_secs: 600,
    }
}
#[test]
fn endpoints_and_credential_bounds_fail_closed() {
    for value in [
        "http://broker.example",
        "https://u:p@broker.example",
        "https://broker.example/path",
        "https://broker.example/?x=secret",
        "https://broker.example/#secret",
        " https://broker.example",
    ] {
        assert!(origin(value).is_err(), "{value}");
    }
    for value in [
        "https://actions.githubusercontent.com/token",
        "https://x.actions.githubusercontent.com.evil/token",
        "https://x.actions.githubusercontent.com:444/token",
        "http://x.actions.githubusercontent.com/token",
        "https://u@x.actions.githubusercontent.com/token",
    ] {
        assert!(issuer(value, "bcode").is_err());
    }
    let url = issuer(
        "https://x.actions.githubusercontent.com/token?a=1&audience=old&audience=other",
        "bcode & private",
    )
    .expect("issuer");
    assert_eq!(
        url.query_pairs()
            .filter(|(name, _)| name == "audience")
            .collect::<Vec<_>>(),
        vec![("audience".into(), "bcode & private".into())]
    );
    for value in ["", "two words", "line\nbreak", "nonascii-é"] {
        assert!(token(value).is_err());
    }
    assert!(token(&"a".repeat(TOKEN_LIMIT + 1)).is_err());
    assert!(
        issuer(
            "https://x.actions.githubusercontent.com/token",
            "bad\naudience"
        )
        .is_err()
    );
}
#[test]
fn lease_budget_uses_actual_expiry_and_supported_nine_hundred_second_cap() {
    let lease = Lease {
        lease: "private-lease".into(),
        expires_at: 1900,
    };
    assert_eq!(
        validate_lease(
            &Lease {
                lease: "private-lease".into(),
                expires_at: 1609
            },
            600,
            1000
        ),
        Err("execution_budget_exceeds_lease")
    );
    assert!(
        validate_lease(
            &Lease {
                lease: "private-lease".into(),
                expires_at: 1610
            },
            600,
            1000
        )
        .is_ok()
    );
    assert!(validate_lease(&lease, 600, 1000).is_ok());
    assert!(validate_lease(&lease, 890, 1000).is_ok());
    assert!(validate_lease(&lease, 891, 1000).is_err());
    assert!(validate_lease(&lease, 0, 1000).is_err());
    assert!(validate_lease(&lease, 1, 1900).is_err());
    assert!(validate_lease(&lease, 1, 999).is_err());
    assert!(!format!("{lease:?}").contains("private-lease"));
    assert_eq!(
        decode::<Lease>(b"{\"lease\":\"secret\",\"expires_at\":true}").expect_err("typed expiry"),
        "broker_setup_invalid_response"
    );
    assert!(decode::<Identity>(&vec![b'a'; LIMIT + 1]).is_err());
    assert!(serde_json::from_value::<PrepareRequest>(serde_json::json!({"directory":"/tmp", "broker_url":"https://broker.example", "audience":"bcode", "timeout_secs":10, "token":"secret"})).is_err());
}
#[test]
fn typed_toml_preserves_paths_without_injection_or_secrets() {
    let input = input(PathBuf::from("/private/quote\"[model.profiles.evil]"));
    let text = config(&input, &origin(&input.broker_url).expect("origin")).expect("encode");
    let parsed: Config = toml::from_str(&text).expect("typed roundtrip");
    assert_eq!(parsed.model.profiles.len(), 1);
    let profile = &parsed.model.profiles["github-broker"];
    assert_eq!(profile.provider_plugin_id, crate::PROVIDER);
    assert_eq!(
        profile.settings["broker_lease_file"],
        "/private/quote\"[model.profiles.evil]/broker-token"
    );
    assert!(!text.contains("private-lease"));
}
#[cfg(unix)]
#[test]
fn private_files_are_exclusive_owner_only_and_collision_cleanup_is_narrow() {
    let temp = tempfile::tempdir().expect("directory");
    let path = std::fs::canonicalize(temp.path()).expect("canonical");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("permissions");
    let directory = private::Directory::open(&path).expect("private directory");
    std::fs::write(path.join("broker-expiry"), "preexisting").expect("collision");
    std::fs::set_permissions(
        path.join("broker-expiry"),
        std::fs::Permissions::from_mode(0o600),
    )
    .expect("permissions");
    assert!(
        directory
            .publish("private-lease", 1900, "nonsecret")
            .is_err()
    );
    assert!(!path.join("broker-token").exists());
    assert_eq!(
        std::fs::read_to_string(path.join("broker-expiry")).expect("preserved"),
        "preexisting"
    );
    std::fs::remove_file(path.join("broker-expiry")).expect("remove fixture");
    directory
        .publish("private-lease", 1900, "nonsecret")
        .expect("publish");
    for name in ["broker-token", "broker-expiry", "broker.toml"] {
        assert_eq!(
            std::fs::metadata(path.join(name))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let lease = read_lease(
        path.join("broker-token").to_str().expect("path"),
        path.join("broker-expiry").to_str().expect("path"),
    )
    .expect("read");
    assert_eq!(lease, ("private-lease".into(), 1900));
    assert!(
        directory
            .publish("replacement", 2000, "replacement")
            .is_err()
    );
    std::fs::set_permissions(
        path.join("broker-token"),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("permissions");
    assert!(
        read_lease(
            path.join("broker-token").to_str().expect("path"),
            path.join("broker-expiry").to_str().expect("path")
        )
        .is_err()
    );
}
#[cfg(unix)]
#[test]
fn symlinks_public_directories_and_nonregular_files_are_rejected() {
    let temp = tempfile::tempdir().expect("directory");
    let path = std::fs::canonicalize(temp.path()).expect("canonical");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("permissions");
    let directory = private::Directory::open(&path).expect("private");
    symlink("absent", path.join("broker-token")).expect("symlink");
    assert!(directory.publish("secret", 1900, "config").is_err());
    assert!(
        std::fs::symlink_metadata(path.join("broker-token"))
            .expect("link retained")
            .file_type()
            .is_symlink()
    );
    assert!(!path.join("broker-expiry").exists());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("permissions");
    assert!(private::Directory::open(&path).is_err());
}
#[test]
fn transport_rejects_redirect_without_forwarding_bearer_and_bounds_chunked_body() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };
    for (status, body, expected) in [
        ("302 Found", "redirect", "broker_setup_rejected"),
        ("200 OK", "oversize", "broker_setup_response_too_large"),
        (
            "200 OK",
            "secret malformed",
            "broker_setup_invalid_response",
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let server = thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < deadline, "bounded accept");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream.set_nonblocking(false).expect("blocking stream");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("timeout");
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .expect("timeout");
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).expect("request");
            let body = if body == "oversize" {
                "x".repeat(LIMIT + 1)
            } else {
                body.to_owned()
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nLocation: http://127.0.0.1:1/stolen\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });
        let runtime = bcode_model_provider_runtime::ProviderRuntime::new().expect("runtime");
        // Test-only HTTP transport exercises the same response handling, not a production bypass.
        let result = runtime
            .block_on(async move {
                let client = Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(Duration::from_secs(5))
                    .build()
                    .expect("client");
                request::<Identity>(
                    &client,
                    Url::parse(&format!("http://{address}/oidc")).expect("url"),
                    "synthetic-secret",
                    false,
                )
                .await
            })
            .expect("runtime call");
        assert_eq!(result.err(), Some(expected));
        server.join().expect("server");
    }
}
