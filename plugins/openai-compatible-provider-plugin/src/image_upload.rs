//! Explicit, bounded upload verification; only deletes the ID created by this invocation.
use super::{AuthSettings, OpenAiCompatibleDialect, settings_for_context};
use base64::Engine as _;
use bcode_model::image_upload::{VerifyImageUploadRequest, VerifyImageUploadResponse};
use std::time::Duration;

pub fn failure_report(code: &'static str) -> VerifyImageUploadResponse {
    let before_upload = matches!(
        code,
        "image_upload_not_authorized_or_unsupported_version"
            | "image_upload_requires_api_key"
            | "image_upload_requires_responses_api"
            | "image_upload_unsupported_mime"
            | "image_upload_fixture_too_large"
            | "image_upload_invalid_base64"
            | "image_upload_empty_fixture"
            | "image_upload_client_failed"
            | "image_upload_invalid_endpoint"
            | "image_upload_unsafe_endpoint"
            | "image_upload_invalid_mime"
    );
    VerifyImageUploadResponse {
        schema_version: 1,
        bytes_verified: false,
        deletion_confirmed: false,
        image_bytes: 0,
        diagnostic: Some(code.to_string()),
        upload_attempted: Some(!before_upload),
    }
}

const LIMIT: usize = 1024 * 1024;

pub async fn verify(
    request: VerifyImageUploadRequest,
) -> Result<VerifyImageUploadResponse, &'static str> {
    if request.schema_version != 1 || !request.allow_remote_storage {
        return Err("image_upload_not_authorized_or_unsupported_version");
    }
    let settings = settings_for_context(&request.provider_context);
    verify_with_settings(request, &settings).await
}

async fn verify_with_settings(
    request: VerifyImageUploadRequest,
    settings: &super::Settings,
) -> Result<VerifyImageUploadResponse, &'static str> {
    let AuthSettings::ApiKey(key) = &settings.auth else {
        return Err("image_upload_requires_api_key");
    };
    if !matches!(settings.dialect, OpenAiCompatibleDialect::ResponsesApi) {
        return Err("image_upload_requires_responses_api");
    }
    let suffix = match request.image.mime_type.as_str() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => return Err("image_upload_unsupported_mime"),
    };
    if request.image.data_base64.len() > LIMIT {
        return Err("image_upload_fixture_too_large");
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&request.image.data_base64)
        .map_err(|_| "image_upload_invalid_base64")?;
    if bytes.is_empty() {
        return Err("image_upload_empty_fixture");
    }
    let client = upload_client()?;
    let endpoint = files_endpoint(&settings.base_url)?;
    let part = reqwest::multipart::Part::bytes(bytes.clone())
        .file_name(format!("bcode-probe.{suffix}"))
        .mime_str(&request.image.mime_type)
        .map_err(|_| "image_upload_invalid_mime")?;
    // Provider expiry is a backstop if the process dies before receiving an ID or deleting it.
    let form = reqwest::multipart::Form::new()
        .text("purpose", "vision")
        .text("expires_after[anchor]", "created_at")
        .text("expires_after[seconds]", "3600")
        .part("file", part);
    let response = client
        .post(&endpoint)
        .bearer_auth(key)
        .multipart(form)
        .send()
        .await
        .map_err(|_| "image_upload_outcome_unknown")?;
    if !response.status().is_success() {
        return Err("image_upload_rejected");
    }
    let body = bounded(response, 16 * 1024)
        .await
        .map_err(|()| "image_upload_receipt_unavailable_cleanup_unknown")?;
    let receipt: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|_| "image_upload_receipt_invalid_cleanup_unknown")?;
    let id = receipt
        .get("id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 256
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
        .ok_or("image_upload_id_invalid_cleanup_unknown")?;
    let file_url = format!("{endpoint}/{id}");
    let retrieved = async {
        let response = client
            .get(format!("{file_url}/content"))
            .bearer_auth(key)
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        bounded(response, bytes.len()).await.ok()
    }
    .await;
    let bytes_verified = retrieved
        .as_ref()
        .is_some_and(|retrieved| retrieved == &bytes);
    let deletion_confirmed = delete_created_file(&client, &file_url, key, id)
        .await
        .unwrap_or(false);
    Ok(VerifyImageUploadResponse {
        schema_version: 1,
        upload_attempted: Some(true),
        bytes_verified,
        deletion_confirmed,
        image_bytes: bytes.len() as u64,
        diagnostic: if !deletion_confirmed {
            Some("image_upload_cleanup_unconfirmed".to_string())
        } else if !bytes_verified {
            Some("image_upload_bytes_unverified".to_string())
        } else {
            None
        },
    })
}

fn files_endpoint(base: &str) -> Result<String, &'static str> {
    let url = reqwest::Url::parse(base).map_err(|_| "image_upload_invalid_endpoint")?;
    let loopback = url.host_str().is_some_and(|host| {
        host.trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    });
    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("image_upload_unsafe_endpoint");
    }
    Ok(format!("{}/files", url.as_str().trim_end_matches('/')))
}

fn upload_client() -> Result<reqwest::Client, &'static str> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "image_upload_client_failed")
}

async fn bounded(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, ()> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn delete_created_file(
    client: &reqwest::Client,
    file_url: &str,
    key: &str,
    id: &str,
) -> Option<bool> {
    let response = client.delete(file_url).bearer_auth(key).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = bounded(response, 16 * 1024).await.ok()?;
    let result: serde_json::Value = serde_json::from_slice(&body).ok()?;
    Some(
        result.get("id").and_then(serde_json::Value::as_str) == Some(id)
            && result.get("deleted").and_then(serde_json::Value::as_bool) == Some(true),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> VerifyImageUploadRequest {
        VerifyImageUploadRequest {
            schema_version: 1,
            provider_context: bcode_model::ProviderRequestContext::default(),
            image: bcode_model::ImageContent {
                mime_type: "image/png".to_string(),
                data_base64: "AQID".to_string(),
                metadata: bcode_model::ImageMetadata::default(),
            },
            allow_remote_storage: true,
        }
    }

    fn endpoint(
        responses: Vec<(&'static str, &'static str, Vec<u8>)>,
    ) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{BufRead as _, Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let url = format!("http://{}", listener.local_addr().expect("address"));
        let worker = std::thread::spawn(move || {
            for (expected_path, status, response) in responses {
                let (mut stream, _) = listener.accept().expect("connection");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("timeout");
                let mut reader = std::io::BufReader::new(&mut stream);
                let mut first = String::new();
                reader.read_line(&mut first).expect("request line");
                assert!(first.starts_with(expected_path), "{first}");
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("header");
                    assert!(!line.is_empty());
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().expect("length");
                    }
                }
                assert!(length < 2 * LIMIT);
                let mut body = vec![0; length];
                reader.read_exact(&mut body).expect("body");
                if expected_path.starts_with("POST") {
                    let multipart = String::from_utf8_lossy(&body);
                    assert!(multipart.contains("name=\"purpose\""));
                    assert!(multipart.contains("vision"));
                    assert!(multipart.contains("expires_after[seconds]"));
                    assert!(multipart.contains("3600"));
                    assert!(body.windows(3).any(|bytes| bytes == [1, 2, 3]));
                }
                drop(reader);
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.len()
                )
                .expect("response headers");
                stream.write_all(&response).expect("response");
            }
        });
        (url, worker)
    }

    #[tokio::test]
    async fn lifecycle_cleans_up_after_download_failure_and_reports_delete_failure() {
        for (download, deletion, expected_bytes, expected_deleted) in [
            (vec![1, 2, 3], true, true, true),
            (vec![9, 9, 9, 9], true, false, true),
            (vec![1, 2, 3], false, true, false),
        ] {
            let receipt = br#"{"id":"file-probe"}"#.to_vec();
            let deleted = format!("{{\"id\":\"file-probe\",\"deleted\":{deletion}}}").into_bytes();
            let (url, worker) = endpoint(vec![
                ("POST /files ", "200 OK", receipt),
                ("GET /files/file-probe/content ", "200 OK", download),
                ("DELETE /files/file-probe ", "200 OK", deleted),
            ]);
            let mut settings = super::super::tests::test_settings(
                AuthSettings::ApiKey("test".to_string()),
                OpenAiCompatibleDialect::ResponsesApi,
            );
            settings.base_url = url;
            let report = verify_with_settings(fixture(), &settings)
                .await
                .expect("report");
            worker.join().expect("server");
            assert_eq!(report.bytes_verified, expected_bytes);
            assert_eq!(report.deletion_confirmed, expected_deleted);
            assert_eq!(
                report.diagnostic.is_none(),
                expected_bytes && expected_deleted
            );
            assert!(
                !serde_json::to_string(&report)
                    .expect("JSON")
                    .contains("file-probe")
            );
        }
    }

    #[tokio::test]
    async fn unsafe_receipt_id_never_becomes_a_download_or_delete_path() {
        let (url, worker) = endpoint(vec![(
            "POST /files ",
            "200 OK",
            br#"{"id":"../other"}"#.to_vec(),
        )]);
        let mut settings = super::super::tests::test_settings(
            AuthSettings::ApiKey("test".to_string()),
            OpenAiCompatibleDialect::ResponsesApi,
        );
        settings.base_url = url;
        assert_eq!(
            verify_with_settings(fixture(), &settings)
                .await
                .expect_err("unsafe ID"),
            "image_upload_id_invalid_cleanup_unknown"
        );
        worker.join().expect("server");
    }

    #[test]
    fn upload_endpoint_requires_tls_and_has_no_ambiguous_components() {
        assert_eq!(
            files_endpoint("https://api.example/v1/").expect("HTTPS"),
            "https://api.example/v1/files"
        );
        assert!(files_endpoint("http://127.0.0.1:1234").is_ok());
        assert!(files_endpoint("http://[::1]:1234").is_ok());
        for endpoint in [
            "http://api.example/v1",
            "https://user:secret@api.example/v1",
            "https://api.example/v1?token=secret",
            "https://api.example/v1#fragment",
            "file:///tmp/files",
            "not a URL",
            "http://127.0.0.1.example/v1",
        ] {
            assert!(files_endpoint(endpoint).is_err(), "{endpoint}");
        }
    }

    #[tokio::test]
    async fn authorization_and_version_precede_auth_and_network() {
        let mut request = VerifyImageUploadRequest {
            schema_version: 1,
            provider_context: bcode_model::ProviderRequestContext::default(),
            image: bcode_model::ImageContent {
                mime_type: "image/png".to_string(),
                data_base64: "AQID".to_string(),
                metadata: bcode_model::ImageMetadata::default(),
            },
            allow_remote_storage: false,
        };
        assert_eq!(
            verify(request.clone()).await.expect_err("denied"),
            "image_upload_not_authorized_or_unsupported_version"
        );
        request.allow_remote_storage = true;
        request.schema_version = 2;
        assert_eq!(
            verify(request).await.expect_err("future"),
            "image_upload_not_authorized_or_unsupported_version"
        );
    }
}
