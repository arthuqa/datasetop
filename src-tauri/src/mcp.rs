//! MCP is the only model capability surface. No server is bundled or enabled by default.
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use rmcp::{
    model::{CallToolRequestParams, ClientConfig, ProtocolVersion},
    service::{RoleClient, RunningService},
    transport::{
        auth::{AuthClient, AuthorizationRequest, OAuthState},
        streamable_http_client::StreamableHttpClientTransportConfig,
        ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess,
    },
    ClientLifecycleMode, ClientServiceExt,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tauri_plugin_opener::OpenerExt;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    process::Command,
    sync::Mutex,
};

type Client = RunningService<RoleClient, ClientConfig>;
pub type AuthCache = Arc<Mutex<HashMap<String, AuthClient<reqwest::Client>>>>;

/// The UI sends `package: ""` for URL servers; an empty string must mean
/// "no package", otherwise a URL server looks like it has both.
fn optional_nonempty<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.filter(|value| !value.trim().is_empty()))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteServer {
    pub name: String,
    #[serde(default)]
    pub url: String,
    #[serde(
        default,
        deserialize_with = "optional_nonempty",
        skip_serializing_if = "Option::is_none"
    )]
    pub package: Option<String>,
    #[serde(
        default,
        deserialize_with = "optional_nonempty",
        skip_serializing_if = "Option::is_none"
    )]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}
const MAX_SERVERS: usize = 32;

#[derive(Deserialize)]
struct LegacyFilesystem {
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default)]
    package: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<RemoteServer>,
}

/// A single tool advertised by an MCP server. Input schemas can be large; the
/// frontend decides how much of the schema to render.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct McpToolInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
}

/// Read-only catalog for one configured server. Disabled servers are always
/// represented so Settings can show them without ever connecting.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct McpServerTools {
    pub name: String,
    pub enabled: bool,
    /// One of "ok", "error" or "disabled".
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub tools: Vec<McpToolInfo>,
}

pub fn validate_config(config: &McpConfig) -> Result<(), String> {
    validate_remotes(&config.servers)
}

pub fn validate_remotes(remotes: &[RemoteServer]) -> Result<(), String> {
    if remotes.len() > MAX_SERVERS {
        return Err(format!("At most {MAX_SERVERS} MCP servers are supported"));
    }
    let mut names = std::collections::HashSet::new();
    for server in remotes {
        if server.name.is_empty()
            || server.name.len() > 32
            || !server
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
            || !names.insert(server.name.as_str())
        {
            return Err(format!(
                "Invalid or duplicate MCP server name: {}",
                server.name
            ));
        }
        let local = server.package.is_some() || server.command.is_some();
        let remote = !server.url.is_empty();
        if local == remote || (server.package.is_some() && server.command.is_some()) {
            return Err(format!(
                "{} needs exactly one of URL, command or legacy package",
                server.name
            ));
        }
        if server.package.is_some() && (!server.args.is_empty() || !server.env.is_empty()) {
            return Err(format!(
                "{} cannot combine a legacy package with args or env",
                server.name
            ));
        }
        if let Some(package) = &server.package {
            if package.starts_with('-')
                || package.starts_with('.')
                || package.contains("://")
                || package.contains("..")
                || package.len() > 160
                || !package
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "@/._-".contains(c))
            {
                return Err(format!("Invalid legacy npm package for {}", server.name));
            }
        }
        if !local {
            if !server.url.contains("://") {
                return Err(format!("{} needs a full URL with ://", server.name));
            }
            let parsed = reqwest::Url::parse(&server.url)
                .map_err(|_| format!("Invalid URL for {}", server.name))?;
            if (parsed.scheme() != "https"
                && !(parsed.scheme() == "http"
                    && matches!(parsed.host_str(), Some("localhost" | "127.0.0.1"))))
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.fragment().is_some()
            {
                return Err(format!(
                    "{} needs an HTTPS URL (or localhost HTTP) without embedded credentials",
                    server.name
                ));
            }
            if !server.args.is_empty() || !server.env.is_empty() {
                return Err(format!(
                    "{} cannot use arguments or environment with a URL",
                    server.name
                ));
            }
        } else {
            let command = server
                .command
                .as_deref()
                .or(server.package.as_deref())
                .unwrap_or("");
            if command.trim().is_empty()
                || command.len() > 1024
                || command.contains('\0')
                || command.contains('\n')
            {
                return Err(format!("Invalid command for {}", server.name));
            }
            if server.args.len() > 64
                || server
                    .args
                    .iter()
                    .any(|arg| arg.len() > 4096 || arg.contains('\0'))
            {
                return Err(format!("Invalid arguments for {}", server.name));
            }
            if server.env.len() > 64
                || server.env.iter().any(|(key, value)| {
                    key.is_empty()
                        || key.len() > 128
                        || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                        || key.as_bytes()[0].is_ascii_digit()
                        || value.len() > 8192
                        || value.contains('\0')
                })
            {
                return Err(format!("Invalid environment variables for {}", server.name));
            }
        }
    }
    Ok(())
}

// Missing config means an empty server list. Existing configurations are never reset.
pub fn read_config(path: &Path) -> Result<McpConfig, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(McpConfig::default())
        }
        Err(error) => return Err(format!("Cannot read MCP configuration: {error}")),
    };
    if bytes.len() > 64 * 1024 {
        return Err("MCP configuration is too large".into());
    }
    let config: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("Invalid mcp.json: {e}"))?;
    let empty = serde_json::Map::new();
    let map = match config.get("mcpServers") {
        Some(Value::Object(map)) => map,
        None if config.get("filesystem").is_some() => &empty,
        _ => return Err("mcp.json must contain an mcpServers object".into()),
    };
    let mut servers = Vec::with_capacity(map.len() + 1);
    // Files written before servers became a flat list stored the folder server
    // under a top-level key. Migrate it so it stays an ordinary, editable entry.
    if let Some(legacy) = config
        .get("filesystem")
        .filter(|_| !map.contains_key("filesystem"))
    {
        let legacy: LegacyFilesystem = serde_json::from_value(legacy.clone())
            .map_err(|e| format!("Invalid filesystem configuration: {e}"))?;
        servers.push(RemoteServer {
            name: "filesystem".into(),
            url: String::new(),
            package: Some(legacy.package),
            command: None,
            args: vec![],
            env: HashMap::new(),
            enabled: legacy.enabled,
        });
    }
    for (name, value) in map {
        servers.push(RemoteServer {
            name: name.clone(),
            url: value
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            package: value
                .get("package")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|package| !package.is_empty())
                .map(str::to_string),
            command: value
                .get("command")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|command| !command.is_empty())
                .map(str::to_string),
            args: value
                .get("args")
                .map(|v| serde_json::from_value(v.clone()))
                .transpose()
                .map_err(|e| format!("Invalid args for {name}: {e}"))?
                .unwrap_or_default(),
            env: value
                .get("env")
                .map(|v| serde_json::from_value(v.clone()))
                .transpose()
                .map_err(|e| format!("Invalid env for {name}: {e}"))?
                .unwrap_or_default(),
            enabled: value
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        });
    }
    let mut config = McpConfig { servers };
    // Allow a disabled legacy entry to be repaired or deleted in Settings.
    // Enabled servers must still be valid before any agent turn starts.
    validate_remotes(
        &config
            .servers
            .iter()
            .filter(|server| server.enabled)
            .cloned()
            .collect::<Vec<_>>(),
    )?;
    // Present old package-only entries to the UI as ordinary editable commands.
    // The adapter is solely for existing configuration files; new servers do
    // not require any particular package manager or executable.
    for server in &mut config.servers {
        if let Some(package) = server.package.take() {
            server.command = Some("npx".into());
            server.args = vec!["--yes".into(), package, "${workspaceFolder}".into()];
        }
    }
    Ok(config)
}

fn lifecycle() -> ClientLifecycleMode {
    ClientLifecycleMode::Auto {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        legacy_version: Some(ProtocolVersion::V_2025_11_25),
    }
}

struct Server {
    name: String,
    config: RemoteServer,
    client: Client,
}

pub struct Tools {
    servers: Vec<Server>,
    routes: HashMap<String, (usize, String)>,
    pub definitions: Vec<Value>,
    folder: PathBuf,
    app: AppHandle,
    cache: AuthCache,
}

// Arguments are an array, never a shell string. Only this explicit token is
// expanded; $PWD is shell-specific, may differ from the selected workspace,
// and does not work consistently on Windows.
fn workspace_arg(value: &str, folder: &Path) -> String {
    value.replace("${workspaceFolder}", &folder.to_string_lossy())
}

async fn connect_server(
    server: &RemoteServer,
    folder: &Path,
    app: &AppHandle,
    cache: &AuthCache,
) -> Result<Client, String> {
    if !server.url.is_empty() {
        return connect_remote(&server.url, app, cache).await;
    }
    let (command, args) = if let Some(package) = &server.package {
        (
            "npx",
            vec![
                "--yes".to_string(),
                package.clone(),
                "${workspaceFolder}".to_string(),
            ],
        )
    } else {
        (
            server.command.as_deref().ok_or("Missing command")?,
            server.args.clone(),
        )
    };
    let executable = if cfg!(windows) && matches!(command, "npx" | "npm") {
        format!("{command}.cmd")
    } else {
        workspace_arg(command, folder)
    };
    let args: Vec<_> = args.iter().map(|arg| workspace_arg(arg, folder)).collect();
    let transport = TokioChildProcess::new(Command::new(executable).configure(|cmd| {
        cmd.args(&args);
        cmd.current_dir(folder);
        cmd.envs(
            server
                .env
                .iter()
                .map(|(key, value)| (key, workspace_arg(value, folder))),
        );
        cmd.kill_on_drop(true);
    }))
    .map_err(|e| format!("Cannot launch {}: {e}", server.name))?;
    tokio::time::timeout(
        Duration::from_secs(90),
        ClientConfig::default().serve_with_lifecycle(transport, lifecycle()),
    )
    .await
    .map_err(|_| format!("{} did not initialize within 90 seconds", server.name))?
    .map_err(|e| format!("{} could not initialize: {e}", server.name))
}

// --- read-only tool catalog -------------------------------------------------

fn tool_info(raw: &Value) -> Option<McpToolInfo> {
    let name = raw.get("name").and_then(Value::as_str)?;
    Some(McpToolInfo {
        name: name.to_string(),
        description: raw
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        input_schema: raw
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({"type":"object","properties":{}})),
    })
}

/// Never echo configured environment values or unbounded provider output to the
/// UI: connection errors can otherwise leak secrets into the frontend or logs.
fn redact_error(error: &str, server: &RemoteServer) -> String {
    let mut redacted = error.to_string();
    for value in server
        .env
        .values()
        .chain(server.args.iter())
        .chain(std::iter::once(&server.url))
        .filter(|value| value.len() >= 4)
    {
        if redacted.contains(value.as_str()) {
            redacted = redacted.replace(value.as_str(), "[redacted]");
        }
    }
    redacted.chars().take(2000).collect()
}

fn disabled_report(server: &RemoteServer) -> McpServerTools {
    McpServerTools {
        name: server.name.clone(),
        enabled: false,
        status: "disabled".into(),
        error: None,
        tools: Vec::new(),
    }
}

fn enabled_report(server: &RemoteServer, result: Result<Vec<Value>, String>) -> McpServerTools {
    match result {
        Ok(raw) => McpServerTools {
            name: server.name.clone(),
            enabled: true,
            status: "ok".into(),
            error: None,
            tools: raw.iter().filter_map(tool_info).collect(),
        },
        Err(error) => McpServerTools {
            name: server.name.clone(),
            enabled: true,
            status: "error".into(),
            error: Some(redact_error(&error, server)),
            tools: Vec::new(),
        },
    }
}

// Connects only long enough to list tools and then releases the server. No
// tool is ever invoked and the configuration is never written.
async fn list_server_tools(
    server: &RemoteServer,
    folder: &Path,
    app: &AppHandle,
    cache: &AuthCache,
) -> Result<Vec<Value>, String> {
    let client = connect_server(server, folder, app, cache).await?;
    let listed = tokio::time::timeout(Duration::from_secs(60), client.list_all_tools())
        .await
        .map_err(|_| "MCP tools/list timed out".to_string())?
        .map_err(|error| error.to_string());
    let _ = tokio::time::timeout(Duration::from_secs(2), client.cancel()).await;
    listed?
        .into_iter()
        .map(|tool| serde_json::to_value(tool).map_err(|error| error.to_string()))
        .collect()
}

/// Snapshot of every configured server: disabled entries are reported as such,
/// and one failing server never hides the others. Reads mcp.json only.
pub async fn list_tools_report(
    folder: &Path,
    config_path: &Path,
    app: &AppHandle,
    cache: &AuthCache,
) -> Result<Vec<McpServerTools>, String> {
    let config = read_config(config_path)?;
    let mut reports = Vec::with_capacity(config.servers.len());
    for server in &config.servers {
        if !server.enabled {
            reports.push(disabled_report(server));
            continue;
        }
        // Discovery is an explicit inspection action, not an agent turn. Do
        // not let one unresponsive server hold the entire catalog indefinitely.
        let result = tokio::time::timeout(
            Duration::from_secs(35),
            list_server_tools(server, folder, app, cache),
        )
        .await
        .unwrap_or_else(|_| Err("MCP tool inspection timed out".into()));
        reports.push(enabled_report(server, result));
    }
    Ok(reports)
}

impl Tools {
    pub async fn connect(
        folder: &Path,
        config_path: &Path,
        app: &AppHandle,
        cache: &AuthCache,
    ) -> Result<Self, String> {
        let mut servers = Vec::new();
        let config = read_config(config_path)?;
        let enabled_count = config
            .servers
            .iter()
            .filter(|server| server.enabled)
            .count();
        for server in config.servers.into_iter().filter(|server| server.enabled) {
            match connect_server(&server, folder, app, cache).await {
                Ok(client) => servers.push(Server {
                    name: server.name.clone(),
                    config: server,
                    client,
                }),
                Err(error) => {
                    let _ = app.emit("agent-message", json!({"kind":"error","message":format!("MCP: {error}. Other servers remain available; try again to reconnect.")}));
                }
            }
        }
        if enabled_count > 0 && servers.is_empty() {
            return Err("No enabled MCP server could connect. Check the server commands or URLs in Settings, then retry.".into());
        }

        let mut routes = HashMap::new();
        let mut definitions = Vec::new();
        let mut discovered = 0;
        let mut skipped = 0;
        for (index, server) in servers.iter().enumerate() {
            let tools = match tokio::time::timeout(
                Duration::from_secs(60),
                server.client.list_all_tools(),
            )
            .await
            {
                Ok(Ok(tools)) => tools,
                failure => {
                    let detail = match failure {
                        Ok(Err(error)) => error.to_string(),
                        Err(_) => "timed out".into(),
                        _ => unreachable!(),
                    };
                    let _ = app.emit("agent-message", json!({"kind":"error","message":format!("MCP tools/list ({}): {detail}", server.name)}));
                    continue;
                }
            };
            discovered += 1;
            for tool in tools {
                if definitions.len() >= 64 {
                    skipped += 1;
                    continue;
                }
                let raw = serde_json::to_value(&tool).map_err(|e| e.to_string())?;
                let Some(name) = raw.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let external = format!("mcp__{}__{}", server.name, name);
                if external.len() > 64 || routes.contains_key(&external) {
                    skipped += 1;
                    continue;
                }
                routes.insert(external.clone(), (index, name.to_string()));
                definitions.push(json!({"type":"function","function":{
                    "name":external,
                    "description":raw.get("description").and_then(Value::as_str).unwrap_or("MCP tool"),
                    "parameters":raw.get("inputSchema").cloned().unwrap_or_else(|| json!({"type":"object","properties":{}}))
                }}));
            }
        }
        if skipped > 0 {
            let _ = app.emit("agent-message", json!({"kind":"info","message":format!("{skipped} MCP tools were unavailable because of name collisions or model tool limits.")}));
        }
        if enabled_count > 0 && discovered == 0 {
            return Err("No enabled MCP server could list tools. Check the connection in Settings, then retry.".into());
        }
        Ok(Self {
            servers,
            routes,
            definitions,
            folder: folder.to_path_buf(),
            app: app.clone(),
            cache: cache.clone(),
        })
    }

    pub async fn call(&mut self, name: &str, arguments: Value) -> Value {
        let Some((index, original)) = self.routes.get(name).cloned() else {
            return json!({"error":"Unknown MCP tool"});
        };
        let Some(args) = arguments.as_object().cloned() else {
            return json!({"error":"Tool arguments must be an object"});
        };
        let request = CallToolRequestParams::new(original).with_arguments(args);
        // A tool may legitimately run for many minutes. Timeout is an upper
        // safety bound, not an idle timeout. Do not replay ambiguous calls:
        // they may have completed on the server before the connection failed.
        let result = tokio::time::timeout(
            Duration::from_secs(1800),
            self.servers[index].client.call_tool(request),
        )
        .await;
        match result {
            Ok(Ok(value)) => {
                serde_json::to_value(value).unwrap_or_else(|e| json!({"error":e.to_string()}))
            }
            failure => {
                let reason = match failure {
                    Ok(Err(error)) => error.to_string(),
                    Err(_) => "MCP tool timed out after 30 minutes".into(),
                    _ => unreachable!(),
                };
                let server = &mut self.servers[index];
                let reconnect =
                    connect_server(&server.config, &self.folder, &self.app, &self.cache).await;
                match reconnect {
                    Ok(client) => {
                        server.client = client;
                        json!({"error":format!("MCP call failed ({reason}); reconnected. The call was not retried because it may have already completed. Check its result before retrying.")})
                    }
                    Err(error) => {
                        json!({"error":format!("MCP call failed ({reason}); reconnection failed: {error}. Try again later.")})
                    }
                }
            }
        }
    }
}

async fn connect_remote(url: &str, app: &AppHandle, cache: &AuthCache) -> Result<Client, String> {
    let cached = { cache.lock().await.get(url).cloned() };
    if let Some(auth) = cached {
        let transport = StreamableHttpClientTransport::with_client(
            auth,
            StreamableHttpClientTransportConfig::with_uri(url),
        );
        if let Ok(Ok(client)) = tokio::time::timeout(
            Duration::from_secs(60),
            ClientConfig::default().serve_with_lifecycle(transport, lifecycle()),
        )
        .await
        {
            return Ok(client);
        }
        cache.lock().await.remove(url);
    }
    let transport = StreamableHttpClientTransport::from_uri(url);
    match tokio::time::timeout(
        Duration::from_secs(60),
        ClientConfig::default().serve_with_lifecycle(transport, lifecycle()),
    )
    .await
    .map_err(|_| format!("MCP connection to {url} timed out"))?
    {
        Ok(client) => Ok(client),
        Err(error) => {
            let challenge = error
                .auth_challenge()
                .ok_or_else(|| format!("MCP connection to {url}: {error}"))?
                .to_string();
            let auth = authorize(url, &challenge, app).await?;
            cache.lock().await.insert(url.to_string(), auth.clone());
            let transport = StreamableHttpClientTransport::with_client(
                auth,
                StreamableHttpClientTransportConfig::with_uri(url),
            );
            tokio::time::timeout(
                Duration::from_secs(60),
                ClientConfig::default().serve_with_lifecycle(transport, lifecycle()),
            )
            .await
            .map_err(|_| "MCP connection after authorization timed out".to_string())?
            .map_err(|e| format!("MCP connection after authorization: {e}"))
        }
    }
}

async fn authorize(
    url: &str,
    challenge: &str,
    app: &AppHandle,
) -> Result<AuthClient<reqwest::Client>, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let redirect = format!(
        "http://127.0.0.1:{}/callback",
        listener.local_addr().map_err(|e| e.to_string())?.port()
    );
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let mut state = OAuthState::new(url, Some(http.clone()))
        .await
        .map_err(|e| e.to_string())?;
    state
        .start_authorization(
            AuthorizationRequest::new(&redirect)
                .with_client_name("datasetop")
                .with_challenge(challenge.to_string()),
        )
        .await
        .map_err(|e| format!("MCP authorization: {e}"))?;
    let authorization_url = state
        .get_authorization_url()
        .await
        .map_err(|e| e.to_string())?;
    let _ = app.emit("agent-message", json!({"kind":"info","message":"Authorize the MCP server in your browser, then return to datasetop."}));
    app.opener()
        .open_url(authorization_url, None::<&str>)
        .map_err(|e| format!("Open authorization page: {e}"))?;
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(180), listener.accept())
        .await
        .map_err(|_| "MCP authorization timed out".to_string())?
        .map_err(|e| e.to_string())?;
    let mut request_bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let mut chunk = [0u8; 2048];
            let count = socket.read(&mut chunk).await.map_err(|e| e.to_string())?;
            if count == 0 {
                return Err("Incomplete OAuth callback".to_string());
            }
            request_bytes.extend_from_slice(&chunk[..count]);
            if request_bytes.len() > 16 * 1024 {
                return Err("OAuth callback too large".to_string());
            }
            if request_bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                break;
            }
        }
        Ok::<(), String>(())
    })
    .await
    .map_err(|_| "OAuth callback timed out".to_string())??;
    let request = std::str::from_utf8(&request_bytes).map_err(|e| e.to_string())?;
    let mut parts = request.split_whitespace();
    if parts.next() != Some("GET") {
        return Err("Invalid OAuth callback method".into());
    }
    let path = parts.next().ok_or("Invalid OAuth callback")?;
    let callback =
        reqwest::Url::parse(&format!("http://127.0.0.1{path}")).map_err(|e| e.to_string())?;
    if callback.path() != "/callback" {
        return Err("Invalid OAuth callback path".into());
    }
    let query: HashMap<_, _> = callback.query_pairs().into_owned().collect();
    let code = query.get("code").ok_or("Authorization was declined")?;
    let csrf = query.get("state").ok_or("OAuth callback missing state")?;
    let outcome = state
        .handle_callback_with_issuer(code, csrf, query.get("iss").map(String::as_str))
        .await;
    let body = if outcome.is_ok() {
        "Authorization complete. Return to datasetop."
    } else {
        "Authorization failed. Return to datasetop."
    };
    let _ = socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
    outcome.map_err(|e| format!("MCP authorization: {e}"))?;
    let manager = state
        .into_authorization_manager()
        .ok_or("MCP authorization state unavailable")?;
    Ok(AuthClient::new(http, manager))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "starts the real npm filesystem MCP server; run explicitly when Node.js is installed"]
    async fn filesystem_server_negotiates_legacy_and_reads_only_selected_folder() {
        let dir = std::env::temp_dir().join(format!("datasetop-mcp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hello.txt"), "MCP integration works").unwrap();
        let path = dir.to_string_lossy().into_owned();
        let transport = TokioChildProcess::new(Command::new("npx").configure(|cmd| {
            cmd.args(["--yes", "@modelcontextprotocol/server-filesystem", &path]);
            cmd.kill_on_drop(true);
        }))
        .unwrap();
        let client = ClientConfig::default()
            .serve_with_lifecycle(transport, lifecycle())
            .await
            .unwrap();
        let tools = client.list_all_tools().await.unwrap();
        assert!(tools.iter().any(|tool| tool.name == "read_text_file"));
        let result = client
            .call_tool(
                CallToolRequestParams::new("read_text_file").with_arguments(
                    json!({"path":dir.join("hello.txt").to_string_lossy()})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        assert!(serde_json::to_string(&result)
            .unwrap()
            .contains("MCP integration works"));
        let denied = client
            .call_tool(
                CallToolRequestParams::new("read_text_file").with_arguments(
                    json!({"path":dir.parent().unwrap().join("outside.txt").to_string_lossy()})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        assert!(
            denied.is_error.unwrap_or(false),
            "filesystem server must deny paths outside the selected root"
        );
        client.cancel().await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    #[ignore = "contacts the hosted OpenRouter MCP server without credentials"]
    async fn openrouter_mcp_exposes_an_oauth_challenge() {
        let transport = StreamableHttpClientTransport::from_uri("https://mcp.openrouter.ai/mcp");
        let error = ClientConfig::default()
            .serve_with_lifecycle(transport, lifecycle())
            .await
            .expect_err("unauthenticated requests must be refused");
        assert!(
            error.auth_challenge().is_some(),
            "expected a usable OAuth challenge: {error}"
        );
    }

    fn remote(name: &str, url: &str) -> RemoteServer {
        RemoteServer {
            name: name.into(),
            url: url.into(),
            package: None,
            command: None,
            args: vec![],
            env: HashMap::new(),
            enabled: true,
        }
    }

    fn write_temp_config(tag: &str, contents: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("datasetop-mcp-{}-{tag}.json", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn validate_accepts_openrouter_https_and_localhost_http() {
        let remotes = vec![
            remote("openrouter", "https://mcp.openrouter.ai/mcp"),
            remote("local", "http://localhost:3000/mcp"),
            remote("loopback", "http://127.0.0.1:8080/mcp"),
        ];
        assert!(validate_remotes(&remotes).is_ok());
    }

    #[test]
    fn validate_rejects_duplicate_names() {
        assert!(validate_remotes(&[
            remote("dup", "https://a.example.com/mcp"),
            remote("dup", "https://b.example.com/mcp"),
        ])
        .is_err());
        assert!(validate_remotes(&[RemoteServer {
            name: "local".into(),
            url: "https://a.example.com/mcp".into(),
            package: Some("@example/server".into()),
            command: None,
            args: vec![],
            env: HashMap::new(),
            enabled: true
        }])
        .is_err());
    }

    #[test]
    fn empty_package_from_the_ui_is_treated_as_url_only() {
        let config: McpConfig = serde_json::from_value(json!({"servers":[
            {"name":"filesystem","url":"","package":"@example/folder","enabled":true},
            {"name":"openrouter","url":"https://mcp.openrouter.ai/mcp","package":"","enabled":true}
        ]}))
        .unwrap();
        assert_eq!(
            config.servers[0].package.as_deref(),
            Some("@example/folder")
        );
        assert_eq!(
            config.servers[1].package, None,
            "a blank package must not be stored"
        );
        assert!(
            validate_config(&config).is_ok(),
            "enabling the URL-only OpenRouter row must validate"
        );
    }

    #[test]
    fn config_file_with_blank_package_is_a_url_server() {
        let path = write_temp_config(
            "blank-package",
            r#"{"mcpServers":{"openrouter":{"url":"https://mcp.openrouter.ai/mcp","package":"  "}}}"#,
        );
        let config = read_config(&path).unwrap();
        assert_eq!(config.servers[0].package, None);
        assert_eq!(config.servers[0].url, "https://mcp.openrouter.ai/mcp");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn validate_rejects_malformed_urls() {
        assert!(validate_remotes(&[remote("empty", "")]).is_err());
        assert!(validate_remotes(&[remote("bad", "not a url")]).is_err());
        assert!(validate_remotes(&[remote("schemeless", "mcp.openrouter.ai/mcp")]).is_err());
    }

    #[test]
    fn validate_rejects_embedded_credentials() {
        assert!(
            validate_remotes(&[remote("creds", "https://user:pass@mcp.openrouter.ai/mcp")])
                .is_err()
        );
        assert!(
            validate_remotes(&[remote("useronly", "https://user@mcp.openrouter.ai/mcp")]).is_err()
        );
    }

    #[test]
    fn validate_rejects_nonlocalhost_http() {
        assert!(validate_remotes(&[remote("plain", "http://mcp.openrouter.ai/mcp")]).is_err());
    }

    #[test]
    fn validate_rejects_more_than_max_servers() {
        let accepted: Vec<_> = (0..MAX_SERVERS)
            .map(|i| remote(&format!("s{i}"), "https://example.com/mcp"))
            .collect();
        assert!(validate_remotes(&accepted).is_ok());
        let rejected: Vec<_> = (0..MAX_SERVERS + 1)
            .map(|i| remote(&format!("s{i}"), "https://example.com/mcp"))
            .collect();
        assert!(validate_remotes(&rejected).is_err());
    }

    #[test]
    fn read_missing_file_exposes_no_servers() {
        let path =
            std::env::temp_dir().join(format!("datasetop-mcp-{}-absent.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert!(read_config(&path).unwrap().servers.is_empty());
    }

    #[test]
    fn read_remotes_rejects_corrupt_json() {
        let path = write_temp_config("corrupt", "{ not valid json");
        assert!(read_config(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_remotes_rejects_missing_mcp_servers() {
        let path = write_temp_config("nokey", "{\"other\":{}}");
        assert!(read_config(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_remotes_parses_valid_config() {
        let path = write_temp_config(
            "valid",
            r#"{"mcpServers":{"openrouter":{"url":"https://mcp.openrouter.ai/mcp"}}}"#,
        );
        let config = read_config(&path).unwrap();
        assert_eq!(
            config.servers,
            vec![remote("openrouter", "https://mcp.openrouter.ai/mcp")]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn config_can_disable_filesystem_and_openrouter_and_change_package() {
        let path = write_temp_config(
            "toggles",
            r#"{"filesystem":{"enabled":false,"package":"@example/folder-server@1.2.3"},"mcpServers":{"openrouter":{"url":"https://mcp.openrouter.ai/mcp","enabled":false}}}"#,
        );
        let config = read_config(&path).unwrap();
        let filesystem = config
            .servers
            .iter()
            .find(|server| server.name == "filesystem")
            .unwrap();
        assert!(!filesystem.enabled);
        assert_eq!(filesystem.command.as_deref(), Some("npx"));
        assert_eq!(
            filesystem.args,
            [
                "--yes",
                "@example/folder-server@1.2.3",
                "${workspaceFolder}"
            ]
        );
        let openrouter = config
            .servers
            .iter()
            .find(|server| server.name == "openrouter")
            .unwrap();
        assert!(!openrouter.enabled);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_empty_saved_list_stays_empty_and_unpinned_servers_are_kept() {
        let path = write_temp_config("empty", r#"{"mcpServers":{}}"#);
        assert!(
            read_config(&path).unwrap().servers.is_empty(),
            "the file, not the code, decides what exists"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn unpinned_local_server_can_be_named_and_disabled() {
        let path = write_temp_config(
            "local",
            r#"{"mcpServers":{"images":{"package":"@example/image-tools","enabled":false}}}"#,
        );
        let config = read_config(&path).unwrap();
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].name, "images");
        assert_eq!(config.servers[0].command.as_deref(), Some("npx"));
        assert_eq!(
            config.servers[0].args,
            ["--yes", "@example/image-tools", "${workspaceFolder}"]
        );
        assert!(!config.servers[0].enabled);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn command_config_accepts_workspace_arguments_and_environment() {
        let path = write_temp_config(
            "command",
            r#"{"mcpServers":{"files":{"command":"npx","args":["--yes","@modelcontextprotocol/server-filesystem","${workspaceFolder}"],"env":{"ROOT":"${workspaceFolder}"}}}}"#,
        );
        let config = read_config(&path).unwrap();
        assert_eq!(config.servers[0].command.as_deref(), Some("npx"));
        assert_eq!(
            workspace_arg(&config.servers[0].args[2], Path::new("/tmp/my folder")),
            "/tmp/my folder"
        );
        assert_eq!(config.servers[0].env["ROOT"], "${workspaceFolder}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn invalid_local_config_is_rejected() {
        let path = write_temp_config(
            "bad-env",
            r#"{"mcpServers":{"bad":{"command":"uvx","args":["server"],"env":{"NOT-VALID":"x"}}}}"#,
        );
        assert!(read_config(&path).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn commands_and_urls_are_mutually_exclusive() {
        let mut entry = remote("mixed", "https://example.com/mcp");
        entry.command = Some("uvx".into());
        assert!(validate_config(&McpConfig {
            servers: vec![entry]
        })
        .is_err());
    }

    #[test]
    fn legacy_only_config_is_migrated() {
        let path = write_temp_config(
            "legacy-only",
            r#"{"filesystem":{"package":"@example/folder","enabled":false}}"#,
        );
        let config = read_config(&path).unwrap();
        assert_eq!(config.servers[0].name, "filesystem");
        assert!(!config.servers[0].enabled);
        assert_eq!(config.servers[0].command.as_deref(), Some("npx"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn disabled_server_is_reported_without_connecting() {
        let mut server = remote("off", "https://example.com/mcp");
        server.enabled = false;
        let report = disabled_report(&server);
        assert_eq!(report.name, "off");
        assert!(!report.enabled);
        assert_eq!(report.status, "disabled");
        assert!(report.tools.is_empty());
        assert!(report.error.is_none());
    }

    #[test]
    fn enabled_report_includes_tool_name_description_and_schema() {
        let server = remote("tools", "https://example.com/mcp");
        let raw = vec![
            json!({
                "name": "read_text_file",
                "description": "Read a file",
                "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}}
            }),
            json!({"name": "no_schema"}),
        ];
        let report = enabled_report(&server, Ok(raw));
        assert_eq!(report.status, "ok");
        assert!(report.error.is_none());
        assert_eq!(report.tools.len(), 2);
        assert_eq!(report.tools[0].name, "read_text_file");
        assert_eq!(report.tools[0].description.as_deref(), Some("Read a file"));
        assert_eq!(
            report.tools[0].input_schema["properties"]["path"]["type"],
            "string"
        );
        assert_eq!(
            report.tools[1].input_schema,
            json!({"type":"object","properties":{}}),
            "a tool without a schema gets an empty object schema"
        );
    }

    #[test]
    fn connection_errors_are_redacted_and_expose_no_environment() {
        let mut server = remote("secure", "https://example.com/mcp");
        server
            .env
            .insert("API_KEY".into(), "super-secret-token".into());
        let report = enabled_report(
            &server,
            Err("handshake failed while using super-secret-token".into()),
        );
        assert_eq!(report.status, "error");
        assert!(report.tools.is_empty());
        // The report must never serialize configured environment or commands.
        let serialized = serde_json::to_string(&report).unwrap();
        let error = report.error.unwrap();
        assert!(!error.contains("super-secret-token"));
        assert!(error.contains("[redacted]"));
        assert!(!serialized.contains("super-secret-token"));
        assert!(!serialized.contains("API_KEY"));
    }

    #[test]
    fn report_never_serializes_server_credentials_from_config() {
        let mut server = remote("secure", "https://example.com/mcp");
        server
            .env
            .insert("TOKEN".into(), "another-secret-value".into());
        let report = enabled_report(&server, Ok(vec![]));
        let serialized = serde_json::to_string(&report).unwrap();
        assert!(!serialized.contains("another-secret-value"));
        assert_eq!(report.status, "ok");
        assert!(report.tools.is_empty());
    }
}
