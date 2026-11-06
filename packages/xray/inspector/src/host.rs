//! Loopback-only static inspector launcher. This is an observation capability, not session control.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse as _, Response};
use bcode_session_models::SessionId;

use rand::TryRngCore as _;
use serde::Serialize;

use crate::InspectorError as CliError;

#[path = "backend.rs"]
mod backend;
#[path = "gateway.rs"]
mod gateway;
#[cfg(test)]
#[path = "gateway_tests.rs"]
mod gateway_tests;
#[path = "transport.rs"]
mod transport;

const MAX_ASSET_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 2048;
const PAIRING_LIFETIME: Duration = Duration::from_secs(45);
const PAIRING_ATTEMPTS: u8 = 8;

/// Launch a local, one-way browser inspector for an explicitly selected session.
#[derive(Debug)]
pub struct XrayWebArgs {
    /// Session to observe; never submits or cancels a turn.
    pub session_id: SessionId,
    /// Built Vite asset directory (must contain index.html).
    pub asset_dir: Option<PathBuf>,
    /// Print the local URL without opening a browser.
    pub no_open: bool,
    /// Explicitly authorize sensitive deep capture for this observation endpoint.
    pub deep: bool,
    /// Endpoint Origin grant lifetime; expiry stops this launcher, not session execution.
    pub lifetime_seconds: u64,
    pub local_browser: bool,
}

// Intentionally not Debug: bearer bytes must never enter diagnostic formatting.
#[derive(Serialize)]
struct InspectorBootstrap {
    schema_version: u16,
    endpoint: String,
    bearer: String,
    session_id: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    demo: bool,
    deep_capture: bool,
}

struct Pairing {
    code: String,
    expires: Instant,
    attempts: u8,
    bootstrap: Option<InspectorBootstrap>,
}

impl Pairing {
    fn redeem(&mut self, code: &str, now: Instant) -> Option<InspectorBootstrap> {
        if now >= self.expires || self.attempts == 0 {
            self.bootstrap = None;
            return None;
        }
        self.attempts -= 1;
        if !constant_time_equal(self.code.as_bytes(), code.as_bytes()) {
            if self.attempts == 0 {
                self.bootstrap = None;
            }
            return None;
        }
        self.bootstrap.take()
    }
}

fn constant_time_equal(expected: &[u8], actual: &[u8]) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    expected
        .iter()
        .zip(actual)
        .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

struct Asset {
    bytes: axum::body::Bytes,
    mime: &'static str,
}

struct HostState {
    authority: String,
    origin: String,
    csp: String,
    assets: BTreeMap<String, Asset>,
    pairing: Mutex<Pairing>,
    gateway: Option<gateway::Gateway>,
    stopped: tokio::sync::watch::Receiver<bool>,
}

/// Serve only after assets and endpoint authorization are ready. Always revoke our endpoint on exit.
///
/// # Errors
/// Returns an error for unavailable/unconfined assets, unavailable loopback hosting,
/// rejected session capability provisioning, or unconfirmed endpoint revocation.
pub async fn run(
    owner: bcode_client::BcodeClient,
    args: &XrayWebArgs,
    shutdown: tokio::sync::watch::Receiver<bool>,
    ready: tokio::sync::oneshot::Sender<()>,
) -> Result<(), CliError> {
    let assets = launch_assets(args.asset_dir.as_deref())?;
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|_| invalid("could not bind loopback inspector listener"))?;
    let authority = listener
        .local_addr()
        .map_err(|_| invalid("inspector listener address unavailable"))?
        .to_string();
    let origin = format!("http://{authority}");
    let mut connection = owner.connect("bcode-xray-web").await?;
    let endpoint = connection
        .open_xray_endpoint_with_origin(
            args.session_id,
            args.deep,
            Some(bcode_ipc::XrayOriginGrantRequest {
                origin: origin.clone(),
                lifetime_seconds: args.lifetime_seconds,
            }),
        )
        .await?;
    // This task also revokes if the launch future is cancelled/dropped mid-flight.
    let endpoint_id = endpoint.endpoint_id.clone();
    let (revoke, revoked) = tokio::sync::oneshot::channel::<()>();
    let cleanup = tokio::spawn(async move {
        let _ = revoked.await;
        matches!(
            tokio::time::timeout(
                Duration::from_secs(5),
                connection.close_xray_endpoint(&endpoint_id)
            )
            .await,
            Ok(Ok(()))
        )
    });
    let result = async {
        let csp = content_security_policy(&endpoint.endpoint)?;
        let client = bcode_client::xray::XrayClient::from_credential_file(
            &endpoint.endpoint,
            &endpoint.credential_file,
        )
        .map_err(|_| invalid("invalid inspector endpoint capability"))?;
        host(
            args,
            assets,
            listener,
            backend::Backend::Live(client),
            csp,
            shutdown,
            ready,
        )
        .await
    }
    .await;
    let _ = revoke.send(());
    match cleanup.await {
        Ok(true) => result,
        _ => Err(invalid(
            "inspector endpoint revocation failed; observation lease cleanup unconfirmed",
        )),
    }
}

/// Launch an explicitly synthetic inspector without session access or provider spending.
/// The supplied session ID is a synthetic correlation label, not an opened session.
/// Deep capture is confined to the in-memory demo engine, never production authority.
///
/// # Errors
/// Returns unavailable asset, random capability, or loopback hosting errors.
pub async fn run_demo(
    args: &XrayWebArgs,
    shutdown: tokio::sync::watch::Receiver<bool>,
    ready: tokio::sync::oneshot::Sender<()>,
) -> Result<(), CliError> {
    let assets = launch_assets(args.asset_dir.as_deref())?;
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|_| invalid("could not bind synthetic inspector listener"))?;
    let csp = content_security_policy("http://127.0.0.1:1/")?;
    let synthetic = XrayWebArgs {
        session_id: args.session_id,
        asset_dir: None,
        no_open: args.no_open,
        deep: true,
        lifetime_seconds: args.lifetime_seconds,
        local_browser: args.local_browser,
    };
    eprintln!("SYNTHETIC X-ray demo. No session is opened and no model/provider request is made.");
    host(
        &synthetic,
        assets,
        listener,
        backend::Backend::demo(),
        csp,
        shutdown,
        ready,
    )
    .await
}

async fn host(
    args: &XrayWebArgs,
    assets: BTreeMap<String, Asset>,
    listener: tokio::net::TcpListener,
    backend: backend::Backend,
    csp: String,
    mut shutdown_request: tokio::sync::watch::Receiver<bool>,
    ready: tokio::sync::oneshot::Sender<()>,
) -> Result<(), CliError> {
    let authority = listener
        .local_addr()
        .map_err(|_| invalid("inspector listener address unavailable"))?
        .to_string();
    let origin = format!("http://{authority}");
    let code = pairing_code()?;
    let capability = pairing_code()?;
    let is_demo = matches!(&backend, backend::Backend::Demo(_));
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let result = async {
        let state = Arc::new(HostState {
            authority,
            origin: origin.clone(),
            csp,
            gateway: Some(gateway::Gateway::new(backend, capability.clone(), args.session_id.to_string(), args.deep)),
            stopped,
            assets,
            pairing: Mutex::new(Pairing {
                code: code.clone(),
                expires: Instant::now() + PAIRING_LIFETIME,
                attempts: PAIRING_ATTEMPTS,
                bootstrap: Some(InspectorBootstrap {
                    schema_version: 1,
                    endpoint: "/".to_owned(),
                    bearer: capability,
                    session_id: args.session_id.to_string(),
                    demo: is_demo,
                    deep_capture: args.deep,
                }),
            }),
        });
        eprintln!("X-ray inspector: {origin}/");
        eprintln!(
            "Enter this one-time pairing code in the inspector (expires in 45 seconds): {code}"
        );
        eprintln!("Observation only. Ctrl-C closes this endpoint, not the session.");
        let remote = !args.local_browser && (std::env::var_os("SSH_CONNECTION").is_some()
            || std::env::var_os("SSH_CLIENT").is_some()
            || std::env::var_os("SSH_TTY").is_some());
        if remote {
            let port = listener.local_addr().map_err(|_| invalid("inspector address unavailable"))?.port();
            eprintln!("SSH: forward this single port, preserving the local port: ssh -L {port}:127.0.0.1:{port} <your-ssh-destination>");
            eprintln!("Then open {origin}/ locally and enter the pairing code. The SSH destination is not inferred.");
        }
        if !remote && !args.no_open && open::that_detached(format!("{origin}/#xray-ticket={code}")).is_err() {
            eprintln!("Browser could not be opened; open the local URL above manually.");
        }
        let app = Router::new().fallback(serve).with_state(state.clone());
        drop(state);
        let _ = ready.send(());
        let (shutdown, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = axum::serve(transport::Listener::new(listener), app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .into_future();
        tokio::pin!(server);
        tokio::select! {
            result = &mut server => {
                return result.map_err(|_| invalid("local inspector server stopped unexpectedly"));
            },
            _ = shutdown_request.wait_for(|stopped| *stopped) => {},
            () = tokio::time::sleep(Duration::from_secs(args.lifetime_seconds)) => {},
        }
        stop.send_replace(true);
        let _ = shutdown.send(());
        // Drain local asset/bootstrap responses briefly. The separate observation
        // endpoint is revoked below even if a local browser connection will not drain.
        match tokio::time::timeout(Duration::from_secs(2), &mut server).await {
            Ok(Err(_)) => Err(invalid("local inspector server stopped unexpectedly")),
            Ok(Ok(())) | Err(_) => Ok(()),
        }
    }.await;
    stop.send_replace(true);
    result
}

fn pairing_code() -> Result<String, CliError> {
    let mut bytes = [0_u8; 16];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| invalid("secure inspector pairing code unavailable"))?;
    let mut code = String::with_capacity(32);
    for byte in bytes {
        write!(code, "{byte:02x}").map_err(|_| invalid("pairing code unavailable"))?;
    }
    Ok(code)
}

fn content_security_policy(endpoint: &str) -> Result<String, CliError> {
    // Client validates literal loopback endpoints; never interpolate arbitrary server text into headers.
    if endpoint.contains(['#', '\r', '\n']) {
        return Err(invalid("invalid inspector endpoint"));
    }
    let uri: axum::http::Uri = endpoint
        .parse()
        .map_err(|_| invalid("invalid inspector endpoint"))?;
    let host = uri
        .host()
        .ok_or_else(|| invalid("invalid inspector endpoint"))?;
    let loopback = host
        .parse::<std::net::Ipv4Addr>()
        .is_ok_and(|ip| ip.is_loopback())
        || host == "[::1]";
    if uri.scheme_str() != Some("http")
        || !loopback
        || uri.port_u16().is_none()
        || uri.path() != "/"
        || uri.query().is_some()
        || uri
            .authority()
            .is_none_or(|authority| authority.as_str().contains('@'))
    {
        return Err(invalid("invalid inspector loopback endpoint"));
    }
    Ok(String::from(
        "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; object-src 'none'",
    ))
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        None
    } else {
        Some(first)
    }
}

fn authorized_request(state: &HostState, request: &Request, bootstrap: bool) -> bool {
    let headers = request.headers();
    if single_header(headers, "host") != Some(state.authority.as_str())
        || request.uri().authority().is_some()
        || request.uri().scheme().is_some()
        || request.uri().query().is_some()
        || headers.contains_key("cookie")
    {
        return false;
    }
    let origin = single_header(headers, "origin");
    let site = single_header(headers, "sec-fetch-site");
    if bootstrap {
        origin == Some(state.origin.as_str())
            && site == Some("same-origin")
            && single_header(headers, "sec-fetch-dest") == Some("empty")
    } else {
        (!headers.contains_key("origin") || origin == Some(state.origin.as_str()))
            && matches!(site, Some("none" | "same-origin"))
    }
}

async fn serve(State(state): State<Arc<HostState>>, request: Request) -> Response {
    let bootstrap = request.uri().path() == "/__xray/bootstrap";
    let operation = request.uri().path().starts_with("/v1/");
    let response = if !authorized_request(&state, &request, bootstrap || operation) {
        StatusCode::FORBIDDEN.into_response()
    } else if operation {
        gateway::forward(state.clone(), request).await
    } else if bootstrap && request.method() == Method::POST {
        // No request body is consumed. All authority is in this one-time memory-only handoff.
        let code = single_header(request.headers(), "x-xray-pairing-code").unwrap_or("");
        let value = state
            .pairing
            .lock()
            .ok()
            .and_then(|mut pairing| pairing.redeem(code, Instant::now()));
        value.map_or_else(
            || StatusCode::FORBIDDEN.into_response(),
            |value| axum::Json(value).into_response(),
        )
    } else if !bootstrap && matches!(*request.method(), Method::GET | Method::HEAD) {
        let path = if request.uri().path() == "/" {
            "/index.html"
        } else {
            request.uri().path()
        };
        state.assets.get(path).map_or_else(
            || StatusCode::NOT_FOUND.into_response(),
            |asset| {
                let body = if request.method() == Method::HEAD {
                    Body::empty()
                } else {
                    Body::from(asset.bytes.clone())
                };
                ([("content-type", asset.mime)], body).into_response()
            },
        )
    } else {
        StatusCode::METHOD_NOT_ALLOWED.into_response()
    };
    secure_response(response, &state.csp)
}

fn secure_response(mut response: Response, csp: &str) -> Response {
    let headers = response.headers_mut();
    for (name, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("cross-origin-resource-policy", "same-origin"),
        ("x-frame-options", "DENY"),
        (
            "permissions-policy",
            "camera=(), microphone=(), geolocation=()",
        ),
    ] {
        headers.insert(
            axum::http::HeaderName::from_static(name),
            axum::http::HeaderValue::from_static(value),
        );
    }
    if let Ok(value) = axum::http::HeaderValue::from_str(csp) {
        headers.insert("content-security-policy", value);
    }
    response
}

fn launch_assets(explicit: Option<&Path>) -> Result<BTreeMap<String, Asset>, CliError> {
    if explicit.is_some() {
        return load_assets(&resolve_assets(explicit)?);
    }
    let bundled = crate::assets::bundled_assets();
    if bundled.is_empty() {
        return load_assets(&resolve_assets(None)?);
    }
    let mut assets = BTreeMap::new();
    let mut total = 0_usize;
    for &(path, bytes) in bundled {
        let relative = path.strip_prefix('/').ok_or_else(missing_assets)?;
        if relative.is_empty()
            || !Path::new(relative)
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
            || path.contains(['?', '#', '\\'])
            || assets.len() >= MAX_ENTRIES
            || bytes.len() as u64 > MAX_ASSET_BYTES
        {
            return Err(missing_assets());
        }
        total = total.checked_add(bytes.len()).ok_or_else(missing_assets)?;
        if total > MAX_TOTAL_BYTES
            || assets
                .insert(
                    path.to_owned(),
                    Asset {
                        bytes: axum::body::Bytes::from_static(bytes),
                        mime: mime(Path::new(relative)),
                    },
                )
                .is_some()
        {
            return Err(missing_assets());
        }
    }
    if !assets.contains_key("/index.html") {
        return Err(missing_assets());
    }
    Ok(assets)
}

fn resolve_assets(explicit: Option<&Path>) -> Result<PathBuf, CliError> {
    if let Some(path) = explicit {
        return std::fs::canonicalize(path).map_err(|_| missing_assets());
    }
    let mut candidates = vec![PathBuf::from("packages/xray/web/dist")];
    if let Ok(executable) = std::env::current_exe()
        && let Some(parent) = executable.parent()
    {
        candidates.push(parent.join("xray-web"));
    }
    candidates
        .into_iter()
        .find_map(|path| {
            std::fs::canonicalize(path)
                .ok()
                .filter(|root| root.join("index.html").is_file())
        })
        .ok_or_else(missing_assets)
}

fn missing_assets() -> CliError {
    invalid(
        "X-ray web assets unavailable. Build packages/xray/web, then pass --asset-dir /absolute/path/to/dist (or install xray-web beside bcode). Other Bcode commands do not require these assets.",
    )
}

fn load_assets(root: &Path) -> Result<BTreeMap<String, Asset>, CliError> {
    let root_file = open_asset_root(root)?;
    let mut assets = BTreeMap::new();
    let mut pending = vec![PathBuf::new()];
    let mut entries = 0_usize;
    let mut total = 0_usize;
    while let Some(relative) = pending.pop() {
        if relative.components().count() > 16 {
            return Err(invalid("inspector assets exceed directory depth bound"));
        }
        for name in directory_names(&root_file, &relative)? {
            entries += 1;
            if entries > MAX_ENTRIES {
                return Err(invalid("inspector assets exceed entry bound"));
            }
            let path = relative.join(name);
            let file = open_relative(&root_file, &path, false)?;
            let metadata = file.metadata().map_err(|_| missing_assets())?;
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            if !metadata.is_file() {
                return Err(invalid("inspector assets must be regular files"));
            }
            let key = path
                .to_str()
                .filter(|name| {
                    name.bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte))
                })
                .ok_or_else(|| invalid("unsupported inspector asset name"))?;
            if file.metadata().map_err(|_| missing_assets())?.len() > MAX_ASSET_BYTES {
                return Err(invalid("inspector asset exceeds file byte bound"));
            }
            let mut bytes = Vec::new();
            file.take(MAX_ASSET_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| missing_assets())?;
            total = total
                .checked_add(bytes.len())
                .ok_or_else(|| invalid("asset size overflow"))?;
            if bytes.len() as u64 > MAX_ASSET_BYTES || total > MAX_TOTAL_BYTES {
                return Err(invalid("inspector assets exceed byte bound"));
            }
            assets.insert(
                format!("/{key}"),
                Asset {
                    bytes: bytes.into(),
                    mime: mime(&path),
                },
            );
        }
    }
    if !assets.contains_key("/index.html") {
        return Err(missing_assets());
    }
    Ok(assets)
}

fn open_asset_root(root: &Path) -> Result<std::fs::File, CliError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    options.open(root).map_err(|_| missing_assets())
}

#[cfg(unix)]
fn open_relative(
    root: &std::fs::File,
    relative: &Path,
    is_directory: bool,
) -> Result<std::fs::File, CliError> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::ffi::OsStrExt as _;
    let mut current = root.try_clone().map_err(|_| missing_assets())?;
    let components = relative.components().collect::<Vec<_>>();
    if components.is_empty() {
        return Err(invalid("empty asset path"));
    }
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(invalid("unconfined asset path"));
        };
        let name =
            std::ffi::CString::new(name.as_bytes()).map_err(|_| invalid("invalid asset name"))?;
        let directory = if index + 1 < components.len() || is_directory {
            libc::O_DIRECTORY
        } else {
            0
        };
        // SAFETY: the parent descriptor remains live; the name is NUL-terminated. Every
        // component is relative and O_NOFOLLOW prevents symlink races outside the root.
        let fd = unsafe {
            libc::openat(
                current.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK | directory,
            )
        };
        if fd < 0 {
            return Err(invalid("inspector asset is unavailable or not confined"));
        }
        // SAFETY: openat returned a new owned descriptor.
        current = unsafe { std::fs::File::from_raw_fd(fd) };
    }
    let metadata = current.metadata().map_err(|_| missing_assets())?;
    if (is_directory && !metadata.is_dir())
        || (!is_directory && !metadata.is_file() && !metadata.is_dir())
    {
        return Err(invalid("inspector asset must be a regular file"));
    }
    Ok(current)
}

#[cfg(unix)]
fn directory_names(
    root: &std::fs::File,
    relative: &Path,
) -> Result<Vec<std::ffi::OsString>, CliError> {
    struct Directory(*mut libc::DIR);
    impl Drop for Directory {
        fn drop(&mut self) {
            // SAFETY: this wrapper owns the valid DIR and closes it exactly once.
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    use std::os::fd::IntoRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;
    let directory = if relative.as_os_str().is_empty() {
        root.try_clone().map_err(|_| missing_assets())?
    } else {
        open_relative(root, relative, true)?
    };
    let fd = directory.into_raw_fd();
    // SAFETY: ownership of this directory descriptor is transferred to DIR on success.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        // SAFETY: fdopendir did not take ownership on failure.
        unsafe {
            libc::close(fd);
        }
        return Err(missing_assets());
    }
    let stream = Directory(stream);
    let mut names = Vec::new();
    loop {
        // SAFETY: DIR is live, private to this function, and used synchronously.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            break;
        }
        // SAFETY: readdir returns a NUL-terminated name valid until its next call.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        if names.len() >= MAX_ENTRIES {
            return Err(invalid("inspector assets exceed entry bound"));
        }
        names.push(std::ffi::OsStr::from_bytes(name).to_os_string());
    }
    Ok(names)
}

#[cfg(not(unix))]
fn directory_names(_: &std::fs::File, _: &Path) -> Result<Vec<std::ffi::OsString>, CliError> {
    Err(invalid(
        "confined inspector asset loading is unavailable on this platform",
    ))
}

#[cfg(not(unix))]
fn open_relative(_: &std::fs::File, _: &Path, _: bool) -> Result<std::fs::File, CliError> {
    Err(invalid(
        "confined inspector asset loading is unavailable on this platform",
    ))
}

fn mime(path: &Path) -> &'static str {
    match path.extension().and_then(std::ffi::OsStr::to_str) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn invalid(message: &str) -> CliError {
    CliError::InvalidArguments(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bootstrap() -> InspectorBootstrap {
        InspectorBootstrap {
            schema_version: 1,
            endpoint: "/".to_owned(),
            bearer: "test-observation-capability".to_owned(),
            session_id: "session".to_owned(),
            demo: false,
            deep_capture: false,
        }
    }

    fn state() -> HostState {
        HostState {
            authority: "127.0.0.1:4321".to_owned(),
            origin: "http://127.0.0.1:4321".to_owned(),
            csp: content_security_policy("http://127.0.0.1:1234/").unwrap(),
            assets: BTreeMap::new(),
            gateway: None,
            stopped: tokio::sync::watch::channel(false).1,
            pairing: Mutex::new(Pairing {
                code: "test-pairing".to_owned(),
                expires: Instant::now() + PAIRING_LIFETIME,
                attempts: PAIRING_ATTEMPTS,
                bootstrap: Some(bootstrap()),
            }),
        }
    }

    fn request() -> Request {
        Request::builder()
            .method(Method::POST)
            .uri("/__xray/bootstrap")
            .header("host", "127.0.0.1:4321")
            .header("origin", "http://127.0.0.1:4321")
            .header("sec-fetch-site", "same-origin")
            .header("sec-fetch-dest", "empty")
            .header("x-xray-pairing-code", "test-pairing")
            .body(Body::empty())
            .unwrap()
    }

    #[test]
    fn pairing_is_single_use_expiring_and_attempt_bounded() {
        let now = Instant::now();
        let mut pairing = Pairing {
            code: "correct".to_owned(),
            expires: now + PAIRING_LIFETIME,
            attempts: PAIRING_ATTEMPTS,
            bootstrap: Some(bootstrap()),
        };
        assert!(pairing.redeem("correct", now).is_some());
        assert!(pairing.redeem("correct", now).is_none());
        pairing.bootstrap = Some(bootstrap());
        assert!(pairing.redeem("correct", now + PAIRING_LIFETIME).is_none());
        assert!(pairing.bootstrap.is_none());
        pairing.bootstrap = Some(bootstrap());
        pairing.expires = now + PAIRING_LIFETIME;
        pairing.attempts = PAIRING_ATTEMPTS;
        for _ in 0..PAIRING_ATTEMPTS {
            assert!(pairing.redeem("wrong", now).is_none());
        }
        assert!(pairing.redeem("correct", now).is_none());
        assert!(pairing.bootstrap.is_none());
    }

    #[test]
    fn exact_origin_host_metadata_and_no_url_secrets_are_required() {
        let state = state();
        assert!(authorized_request(&state, &request(), true));
        for (header, value) in [
            ("host", "localhost:4321"),
            ("origin", "null"),
            ("origin", "http://127.0.0.1:4322"),
            ("sec-fetch-site", "cross-site"),
            ("sec-fetch-dest", "document"),
            ("cookie", "ambient=credential"),
        ] {
            let mut request = request();
            request.headers_mut().insert(
                axum::http::HeaderName::from_static(header),
                value.parse().unwrap(),
            );
            assert!(!authorized_request(&state, &request, true));
        }
        for header in ["host", "origin", "sec-fetch-site", "sec-fetch-dest"] {
            let mut request = request();
            request.headers_mut().remove(header);
            assert!(!authorized_request(&state, &request, true));
        }
        let mut duplicate = request();
        duplicate
            .headers_mut()
            .append("origin", "http://127.0.0.1:4321".parse().unwrap());
        assert!(!authorized_request(&state, &duplicate, true));
        for uri in [
            "/__xray/bootstrap?code=secret",
            "http://127.0.0.1:4321/__xray/bootstrap",
        ] {
            let mut request = request();
            *request.uri_mut() = uri.parse().unwrap();
            assert!(!authorized_request(&state, &request, true));
        }
    }

    #[tokio::test]
    async fn bootstrap_requires_pairing_and_returns_exact_contract_once() {
        let state = Arc::new(state());
        let mut wrong = request();
        wrong.headers_mut().remove("x-xray-pairing-code");
        assert_eq!(
            serve(State(state.clone()), wrong).await.status(),
            StatusCode::FORBIDDEN
        );
        let response = serve(State(state.clone()), request()).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(
            !response
                .headers()
                .contains_key("access-control-allow-origin")
        );
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "schema_version": 1, "endpoint": "/",
                "bearer": "test-observation-capability", "session_id": "session", "deep_capture": false,
            })
        );
        assert_eq!(
            serve(State(state), request()).await.status(),
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn static_routes_are_exact_and_bootstrap_never_accepts_get() {
        let mut shared_host = Arc::new(state());
        Arc::get_mut(&mut shared_host).unwrap().assets.insert(
            "/index.html".to_owned(),
            Asset {
                bytes: axum::body::Bytes::from_static(b"inspector"),
                mime: "text/html; charset=utf-8",
            },
        );
        for (path, method, status) in [
            ("/", Method::GET, StatusCode::OK),
            ("/index.html", Method::HEAD, StatusCode::OK),
            ("/../index.html", Method::GET, StatusCode::NOT_FOUND),
            ("/%2e%2e/index.html", Method::GET, StatusCode::NOT_FOUND),
            ("/missing", Method::GET, StatusCode::NOT_FOUND),
            (
                "/__xray/bootstrap",
                Method::GET,
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            ("/", Method::POST, StatusCode::METHOD_NOT_ALLOWED),
        ] {
            let mut request = request();
            *request.uri_mut() = path.parse().unwrap();
            *request.method_mut() = method;
            let response = serve(State(shared_host.clone()), request).await;
            assert_eq!(response.status(), status, "{path}");
            assert_eq!(response.headers()["x-content-type-options"], "nosniff");
            assert!(response.headers().contains_key("content-security-policy"));
        }
        drop(shared_host);
    }

    #[test]
    fn csp_rejects_external_or_injected_endpoints() {
        for endpoint in [
            "https://example.com/",
            "http://localhost:1234/",
            "http://127.0.0.1:1234/path",
            "http://user@127.0.0.1:1234/",
            "http://127.0.0.1:1234/?token=secret",
            "http://127.0.0.1:1234/; connect-src *",
            "http://127.0.0.1:1234/#fragment",
        ] {
            assert!(content_security_policy(endpoint).is_err(), "{endpoint}");
        }
        let csp = content_security_policy("http://127.0.0.1:1234/").unwrap();
        assert!(csp.contains("connect-src 'self';"));
    }

    #[cfg(unix)]
    #[test]
    fn asset_loading_rejects_symlinks_traversal_and_missing_builds() {
        use std::os::unix::fs::symlink;
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("dist");
        std::fs::create_dir(&root).unwrap();
        assert!(load_assets(&root).is_err());
        std::fs::write(root.join("index.html"), b"<html></html>").unwrap();
        std::fs::create_dir(root.join("assets")).unwrap();
        std::fs::write(root.join("assets/app.js"), b"export {};").unwrap();
        let assets = load_assets(&root).unwrap();
        assert_eq!(
            assets["/assets/app.js"].mime,
            "text/javascript; charset=utf-8"
        );
        let descriptor = open_asset_root(&root).unwrap();
        assert!(open_relative(&descriptor, Path::new("../secret"), false).is_err());
        assert!(open_relative(&descriptor, Path::new("/etc/passwd"), false).is_err());
        symlink(temporary.path(), root.join("escape")).unwrap();
        assert!(load_assets(&root).is_err());
        assert!(open_relative(&descriptor, Path::new("escape/secret"), false).is_err());
    }
}
