//! A chat window over a real OS folder. The model can interact with that folder
//! only through MCP; there is no filesystem watcher or built-in file tool.
//! MCP image results are persisted by the host, not by model-invoked tools.
mod mcp;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::Utc;
use mcp::{AuthCache, McpConfig, Tools};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
    time::Duration,
};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;

const MAX_STEPS: usize = 64;
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 12 * 1024 * 1024;
static MEDIA_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Default, Serialize, Deserialize)]
struct Settings {
    #[serde(default)]
    openai_base_url: String,
    #[serde(default)]
    openai_model: String,
    #[serde(default)]
    openai_api_key: String,
    // A blank override means: load AGENTS.md from the selected folder.
    #[serde(default)]
    prompts: HashMap<String, String>,
}

#[derive(Deserialize)]
struct SettingsInput {
    openai_base_url: String,
    openai_model: String,
    openai_api_key: String,
    #[serde(default)]
    clear_api_key: bool,
    system_prompt: String,
}

#[derive(Serialize)]
struct PublicSettings {
    openai_base_url: String,
    openai_model: String,
    openai_api_key_set: bool,
    saved_api_key_set: bool,
    env_fields: Vec<String>,
}

#[derive(Serialize)]
struct Snapshot {
    folder: String,
    platform: &'static str,
    running: bool,
    busy: bool,
    configured: bool,
    system_prompt: String,
    settings: PublicSettings,
}

#[derive(Clone, Serialize)]
struct ChatItem {
    role: String,
    kind: String,
    text: String,
    ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

#[derive(Clone)]
struct Shared {
    folder: Arc<RwLock<PathBuf>>,
    settings: Arc<RwLock<Settings>>,
    settings_path: PathBuf,
    log_path: PathBuf,
    running: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    epoch: Arc<AtomicU64>,
    task: Arc<Mutex<Option<tauri::async_runtime::JoinHandle<()>>>>,
    auth: AuthCache,
}

impl Shared {
    fn folder(&self) -> PathBuf {
        self.folder.read().unwrap().clone()
    }
    fn effective_settings(&self, folder: &Path) -> (Settings, Vec<String>) {
        let mut settings = self.settings.read().unwrap().clone();
        let env = read_folder_env(folder);
        // Never send a saved secret to a URL supplied by a newly selected
        // workspace. An env-sourced endpoint may use only its own env key.
        if settings.openai_base_url.trim().is_empty()
            && env
                .get("OPENAI_BASE_URL")
                .is_some_and(|value| !value.trim().is_empty())
        {
            settings.openai_api_key.clear();
        }
        let mut fields = Vec::new();
        for (name, field) in [
            ("OPENAI_BASE_URL", &mut settings.openai_base_url),
            ("OPENAI_MODEL", &mut settings.openai_model),
            ("OPENAI_API_KEY", &mut settings.openai_api_key),
        ] {
            if field.trim().is_empty() {
                if let Some(value) = env.get(name).filter(|value| !value.trim().is_empty()) {
                    *field = value.clone();
                    fields.push(name.to_string());
                }
            }
        }
        (settings, fields)
    }
    fn snapshot(&self) -> Snapshot {
        let folder = self.folder();
        let (settings, env_fields) = self.effective_settings(&folder);
        Snapshot {
            folder: folder.to_string_lossy().into_owned(),
            platform: match std::env::consts::OS {
                "macos" => "macos",
                "windows" => "windows",
                _ => "linux",
            },
            running: self.running.load(Ordering::SeqCst),
            busy: self.busy.load(Ordering::SeqCst),
            configured: !settings.openai_base_url.trim().is_empty()
                && !settings.openai_model.trim().is_empty(),
            system_prompt: prompt_for(&folder, &settings),
            settings: PublicSettings {
                openai_base_url: settings.openai_base_url.clone(),
                openai_model: settings.openai_model.clone(),
                openai_api_key_set: !settings.openai_api_key.is_empty(),
                saved_api_key_set: !self.settings.read().unwrap().openai_api_key.is_empty(),
                env_fields,
            },
        }
    }
}

// The selected folder is the only .env source. Never execute its contents or
// copy its key into the UI; explicit saved settings take precedence per field.
fn read_folder_env(folder: &Path) -> HashMap<String, String> {
    let Ok(bytes) = fs::read(folder.join(".env")) else {
        return HashMap::new();
    };
    if bytes.len() > 64 * 1024 {
        return HashMap::new();
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return HashMap::new();
    };
    let mut result = HashMap::new();
    for line in text.lines() {
        let line = line.trim().strip_prefix("export ").unwrap_or(line.trim());
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if !matches!(key, "OPENAI_BASE_URL" | "OPENAI_API_KEY" | "OPENAI_MODEL") {
            continue;
        }
        let value = value.trim();
        let value = if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            &value[1..value.len() - 1]
        } else {
            value
                .split_once(" #")
                .map(|(head, _)| head)
                .unwrap_or(value)
                .trim_end()
        };
        result.insert(key.to_string(), value.to_string());
    }
    result
}

fn prompt_for(folder: &Path, settings: &Settings) -> String {
    let key = folder.to_string_lossy();
    if let Some(prompt) = settings
        .prompts
        .get(key.as_ref())
        .filter(|s| !s.trim().is_empty())
    {
        return prompt.clone();
    }
    fs::read_to_string(folder.join("AGENTS.md")).unwrap_or_default()
}

fn save_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    let mut file = fs::File::create(&tmp).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    }
    file.write_all(&bytes).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    fs::rename(tmp, path).map_err(|e| e.to_string())
}

fn initialize(app: &AppHandle) -> Result<Shared, String> {
    let config_dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&config_dir).map_err(|e| e.to_string())?;
    fs::create_dir_all(&data_dir).map_err(|e| e.to_string())?;
    let settings_path = config_dir.join("settings.json");
    let settings = fs::read(&settings_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let folder_path = config_dir.join("folder.json");
    // Documents is discoverable in Finder/Explorer/XDG file managers. Never
    // plant TASK.md or provider secrets in a user's working folder.
    let default_folder = app
        .path()
        .document_dir()
        .or_else(|_| app.path().home_dir())
        .unwrap_or(data_dir.clone())
        .join("datasetop");
    let folder = fs::read(&folder_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<PathBuf>(&bytes).ok())
        .filter(|path| path.is_dir())
        .unwrap_or(default_folder);
    fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    let folder = fs::canonicalize(folder).map_err(|e| e.to_string())?;
    Ok(Shared {
        folder: Arc::new(RwLock::new(folder)),
        settings: Arc::new(RwLock::new(settings)),
        settings_path,
        log_path: data_dir.join("chat.jsonl"),
        running: Arc::new(AtomicBool::new(false)),
        busy: Arc::new(AtomicBool::new(false)),
        epoch: Arc::new(AtomicU64::new(0)),
        task: Arc::new(Mutex::new(None)),
        auth: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
    })
}

fn append_log(state: &Shared, kind: &str, message: &str) -> Result<(), String> {
    append_log_data(state, kind, message, Value::Null)
}

fn append_log_data(state: &Shared, kind: &str, message: &str, data: Value) -> Result<(), String> {
    let path = &state.log_path;
    if fs::metadata(path).map(|m| m.len()).unwrap_or(0) > MAX_LOG_BYTES {
        let old = path.with_extension("previous.jsonl");
        let _ = fs::remove_file(&old);
        fs::rename(path, old).map_err(|e| e.to_string())?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    writeln!(file, "{}", json!({"folder":state.folder().to_string_lossy(),"ts":Utc::now().to_rfc3339(),"kind":kind,"message":message,"data":data})).map_err(|e| e.to_string())
}

fn history(state: &Shared) -> Vec<ChatItem> {
    let mut items = Vec::new();
    let folder = state.folder().to_string_lossy().into_owned();
    for path in [
        state.log_path.with_extension("previous.jsonl"),
        state.log_path.clone(),
    ] {
        if let Ok(text) = fs::read_to_string(path) {
            for line in text.lines() {
                let Ok(row) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if row.get("folder").and_then(Value::as_str) != Some(&folder) {
                    continue;
                }
                let kind = row.get("kind").and_then(Value::as_str).unwrap_or("");
                if kind == "chat_cleared" {
                    items.clear();
                    continue;
                }
                let role = match kind {
                    "user_message" => "user",
                    "agent_message" | "error" | "tool" | "thinking" => "assistant",
                    _ => continue,
                };
                let text = row.get("message").and_then(Value::as_str).unwrap_or("");
                items.push(ChatItem {
                    role: role.into(),
                    kind: kind.into(),
                    text: text.into(),
                    ts: row.get("ts").and_then(Value::as_str).unwrap_or("").into(),
                    data: row.get("data").filter(|v| !v.is_null()).cloned(),
                });
                if items.len() > 300 {
                    items.remove(0);
                }
            }
        }
    }
    items
}

fn emit_chat(app: &AppHandle, kind: &str, text: &str, data: Value) {
    let _ = app.emit("agent-chat", json!({"kind":kind,"text":text,"data":data}));
}

fn image_format(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("image/png", "png"))
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some(("image/jpeg", "jpg"))
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(("image/gif", "gif"))
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(("image/webp", "webp"))
    } else {
        None
    }
}

// Media is stored in the selected workspace: an ordinary MCP file tool can
// subsequently operate on it, without the model ever copying base64 tokens.
fn store_tool_image(folder: &Path, encoded: &str, claimed_mime: &str) -> Result<Value, String> {
    if encoded.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 + 4 {
        return Err("Image exceeds the 12 MB limit".into());
    }
    let bytes = BASE64
        .decode(encoded)
        .map_err(|_| "Invalid MCP image encoding")?;
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return Err("Image exceeds the 12 MB limit".into());
    }
    let (mime, extension) = image_format(&bytes).ok_or("Unsupported MCP image format")?;
    if claimed_mime != mime && !(claimed_mime == "image/jpg" && mime == "image/jpeg") {
        return Err("MCP image format does not match its MIME type".into());
    }
    let directory = folder.join(".datasetop").join("media");
    fs::create_dir_all(&directory).map_err(|e| format!("Could not create image directory: {e}"))?;
    let root = fs::canonicalize(folder).map_err(|e| e.to_string())?;
    let safe_directory = fs::canonicalize(&directory).map_err(|e| e.to_string())?;
    if !safe_directory.starts_with(&root) {
        return Err("Image directory escapes the selected folder".into());
    }
    for _ in 0..4 {
        let sequence = MEDIA_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = safe_directory.join(format!(
            "{}-{sequence}.{extension}",
            Utc::now().timestamp_micros()
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                if let Err(error) = file.write_all(&bytes) {
                    drop(file);
                    let _ = fs::remove_file(&path);
                    return Err(format!("Could not save MCP image: {error}"));
                }
                return Ok(json!({"path":path.to_string_lossy(), "mime_type":mime}));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Could not save MCP image: {e}")),
        }
    }
    Err("Could not allocate an image filename".into())
}

fn prepare_tool_result(folder: &Path, result: &mut Value) -> Vec<Value> {
    let mut images = Vec::new();
    let mut encoded_images = Vec::new();
    if let Some(blocks) = result.get_mut("content").and_then(Value::as_array_mut) {
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("image") {
                let mime = block
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let image = block
                    .get("data")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "MCP image has no data".to_string())
                    .and_then(|data| store_tool_image(folder, data, &mime));
                match image {
                    Ok(info) => {
                        if let Some(encoded) = block.get("data").and_then(Value::as_str) {
                            encoded_images.push(encoded.to_string());
                        }
                        *block = json!({"type":"text", "text": format!("Image saved to {} ({})", info["path"].as_str().unwrap_or(""), mime)});
                        images.push(info);
                    }
                    Err(error) => {
                        *block = json!({"type":"text", "text":format!("Image could not be saved: {error}")})
                    }
                }
            }
        }
    }
    // Structured content may repeat typed binary blocks. Do not erase ordinary
    // objects named `data`: they can contain important tool results.
    fn strip_media(value: &mut Value, encoded_images: &[String]) {
        match value {
            Value::Array(items) => {
                for item in items {
                    strip_media(item, encoded_images);
                }
            }
            Value::Object(items) => {
                let binary = matches!(
                    items.get("type").and_then(Value::as_str),
                    Some("image" | "audio")
                ) || items
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .is_some_and(|mime| mime.starts_with("image/") || mime.starts_with("audio/"));
                if binary {
                    if let Some(data) = items.get_mut("data") {
                        *data = json!("[binary media omitted]");
                    }
                }
                for (key, item) in items.iter_mut() {
                    if key == "data"
                        && item
                            .as_str()
                            .is_some_and(|data| encoded_images.iter().any(|saved| saved == data))
                    {
                        *item = json!("[binary media omitted]");
                    } else {
                        strip_media(item, encoded_images);
                    }
                }
            }
            _ => {}
        }
    }
    strip_media(result, &encoded_images);
    images
}

// Only files that this app saved during a tool call are readable: anything
// else (other folders, arbitrary paths, symlinks out of the media directory)
// is rejected before the bytes are read.
fn resolve_tool_image(folder: &Path, path: &str) -> Result<PathBuf, String> {
    let media = fs::canonicalize(folder.join(".datasetop").join("media"))
        .map_err(|_| "Image unavailable".to_string())?;
    if !media.starts_with(folder) {
        return Err("Image unavailable".into());
    }
    let file = fs::canonicalize(path).map_err(|_| "Image unavailable".to_string())?;
    if file.parent() != Some(media.as_path()) {
        return Err("Image is outside this folder's saved media".into());
    }
    let metadata = fs::metadata(&file).map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_IMAGE_BYTES as u64 {
        return Err("Image exceeds the 12 MB limit".into());
    }
    Ok(file)
}

#[tauri::command]
fn read_tool_image(path: String, state: State<Shared>) -> Result<String, String> {
    let file = resolve_tool_image(&state.folder(), &path)?;
    let bytes = fs::read(file).map_err(|e| e.to_string())?;
    let (mime, _) = image_format(&bytes).ok_or("Unsupported image format")?;
    Ok(format!("data:{mime};base64,{}", BASE64.encode(bytes)))
}

fn model_error(status: reqwest::StatusCode, bytes: &[u8]) -> String {
    let payload = serde_json::from_slice::<Value>(bytes).unwrap_or(Value::Null);
    let provider = payload
        .pointer("/error/metadata/raw")
        .and_then(Value::as_str)
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
    let message = provider
        .as_ref()
        .and_then(|value| value.pointer("/error/message"))
        .and_then(Value::as_str)
        .or_else(|| payload.pointer("/error/message").and_then(Value::as_str));
    let detail = message.unwrap_or("The model provider rejected the request");
    let detail: String = detail.chars().take(350).collect();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        format!("Model authentication failed ({status}). Check your API key in Settings. {detail}")
    } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        format!("Model rate limit reached. Please try again shortly. {detail}")
    } else {
        format!("Model request failed ({status}): {detail}")
    }
}

/// Reasoning and visible text produced by a single streamed chunk.
type StreamDelta = (Option<String>, Option<String>);

fn apply_stream_event(
    data: &[u8],
    text: &mut String,
    reasoning: &mut String,
    calls: &mut std::collections::BTreeMap<u64, Value>,
) -> Result<Option<StreamDelta>, String> {
    if data == b"[DONE]" {
        return Ok(None);
    }
    let value: Value = serde_json::from_slice(data)
        .map_err(|_| "Invalid streaming response from the model".to_string())?;
    if let Some(error) = value.pointer("/error/message").and_then(Value::as_str) {
        return Err(format!(
            "Model stream failed: {}",
            error.chars().take(350).collect::<String>()
        ));
    }
    let delta = value.pointer("/choices/0/delta").unwrap_or(&Value::Null);
    // OpenAI-compatible providers commonly expose reasoning as
    // `reasoning_content` (or `reasoning`). Never infer thoughts from content.
    let thought = delta
        .get("reasoning_content")
        .or_else(|| delta.get("reasoning"))
        .and_then(Value::as_str)
        .filter(|piece| !piece.is_empty());
    if let Some(piece) = thought {
        reasoning.push_str(piece);
        if reasoning.len() > 2_000_000 {
            return Err("Model reasoning was too long".into());
        }
    }
    if let Some(piece) = delta
        .get("content")
        .and_then(Value::as_str)
        .filter(|piece| !piece.is_empty())
    {
        text.push_str(piece);
        if text.len() > 2_000_000 {
            return Err("Model response was too long".into());
        }
    }
    if let Some(parts) = delta.get("tool_calls").and_then(Value::as_array) {
        for part in parts {
            let index = part
                .get("index")
                .and_then(Value::as_u64)
                .ok_or("Model tool call omitted its index")?;
            if index >= 8 {
                return Err("Model requested too many tools at once".into());
            }
            let call = calls.entry(index).or_insert_with(
                || json!({"id":"","type":"function","function":{"name":"","arguments":""}}),
            );
            for (path, key) in [
                ("/id", "id"),
                ("/function/name", "name"),
                ("/function/arguments", "arguments"),
            ] {
                if let Some(piece) = part.pointer(path).and_then(Value::as_str) {
                    let target = if key == "id" {
                        &mut call["id"]
                    } else {
                        &mut call["function"][key]
                    };
                    let mut current = target.as_str().unwrap_or_default().to_string();
                    current.push_str(piece);
                    if current.len() > 200_000 {
                        return Err("Model tool arguments were too long".into());
                    }
                    *target = json!(current);
                }
            }
        }
    }
    let content = delta
        .get("content")
        .and_then(Value::as_str)
        .filter(|piece| !piece.is_empty())
        .map(str::to_string);
    Ok(if content.is_some() || thought.is_some() {
        Some((content, thought.map(str::to_string)))
    } else {
        None
    })
}

async fn read_model_reply(
    mut response: reqwest::Response,
    app: &AppHandle,
) -> Result<Value, String> {
    if !response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .contains("text/event-stream")
    {
        let bytes = response
            .bytes()
            .await
            .map_err(|e| format!("Could not read model response: {e}"))?;
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| "Invalid model response".to_string())?;
        let reply = value
            .pointer("/choices/0/message")
            .ok_or("Model response contained no message")?
            .clone();
        if let Some(thought) = reply
            .get("reasoning_content")
            .or_else(|| reply.get("reasoning"))
            .and_then(Value::as_str)
        {
            if !thought.is_empty() {
                emit_chat(app, "thinking_delta", thought, json!({}));
            }
        }
        if let Some(text) = reply.get("content").and_then(Value::as_str) {
            emit_chat(app, "assistant_delta", text, json!({}));
        }
        return Ok(reply);
    }
    let mut pending = Vec::new();
    let mut data = Vec::new();
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls = std::collections::BTreeMap::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("Model stream interrupted: {e}"))?
    {
        pending.extend_from_slice(&chunk);
        if pending.len() > 2_000_000 {
            return Err("Model response was too long".into());
        }
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = pending.drain(..=end).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                if !data.is_empty() {
                    if let Some((content, thought)) =
                        apply_stream_event(&data, &mut text, &mut reasoning, &mut calls)?
                    {
                        if let Some(thought) = thought {
                            emit_chat(app, "thinking_delta", &thought, json!({}));
                        }
                        if let Some(content) = content {
                            emit_chat(app, "assistant_delta", &content, json!({}));
                        }
                    }
                    data.clear();
                }
            } else if let Some(payload) = line
                .strip_prefix(b"data: ")
                .or_else(|| line.strip_prefix(b"data:"))
            {
                if !data.is_empty() {
                    data.push(b'\n');
                }
                data.extend_from_slice(payload);
            }
        }
    }
    if !data.is_empty() {
        if let Some((content, thought)) =
            apply_stream_event(&data, &mut text, &mut reasoning, &mut calls)?
        {
            if let Some(thought) = thought {
                emit_chat(app, "thinking_delta", &thought, json!({}));
            }
            if let Some(content) = content {
                emit_chat(app, "assistant_delta", &content, json!({}));
            }
        }
    }
    if text.is_empty() && calls.is_empty() {
        return Err("The model returned an empty response".into());
    }
    Ok(
        json!({"role":"assistant","content":text,"reasoning_content":reasoning,"tool_calls":calls.into_values().collect::<Vec<_>>()}),
    )
}

#[tauri::command]
fn get_snapshot(state: State<Shared>) -> Snapshot {
    state.snapshot()
}

#[tauri::command]
fn get_chat_history(state: State<Shared>) -> Vec<ChatItem> {
    history(&state)
}

#[tauri::command]
async fn export_chat(state: State<'_, Shared>, app: AppHandle) -> Result<bool, String> {
    if state.busy.load(Ordering::SeqCst) {
        return Err("Wait for the current response before exporting".into());
    }
    let items = history(&state);
    if items.is_empty() {
        return Err("There are no messages to export yet".into());
    }
    let chosen = tokio::task::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_file_name("datasetop-chat.md")
            .blocking_save_file()
    })
    .await
    .map_err(|e| e.to_string())?;
    let Some(path) = chosen else { return Ok(false) };
    let path = path.as_path().ok_or("Choose a local file")?;
    let mut output = String::from("# datasetop conversation\n\n");
    for item in items {
        let label = match item.kind.as_str() {
            "user_message" => "You",
            "agent_message" => "Assistant",
            "error" => "Error",
            "tool" => "Tool",
            "thinking" => "Thinking",
            _ => continue,
        };
        output.push_str(&format!("## {label} · {}\n\n{}\n\n", item.ts, item.text));
    }
    fs::write(path, output).map_err(|e| format!("Could not export conversation: {e}"))?;
    Ok(true)
}

#[tauri::command]
fn get_mcp_servers(state: State<Shared>) -> Result<McpConfig, String> {
    mcp::read_config(&state.settings_path.with_file_name("mcp.json"))
}

// Read-only inspection of the configured MCP servers and the tools each one
// advertises. Disabled servers are represented without connecting.
#[tauri::command]
async fn list_mcp_tools(
    state: State<'_, Shared>,
    app: AppHandle,
) -> Result<Vec<mcp::McpServerTools>, String> {
    mcp::list_tools_report(
        &state.folder(),
        &state.settings_path.with_file_name("mcp.json"),
        &app,
        &state.auth,
    )
    .await
}

#[tauri::command]
fn save_mcp_servers(config: McpConfig, state: State<Shared>) -> Result<McpConfig, String> {
    if state.busy.load(Ordering::SeqCst) {
        return Err("Wait for the current response before changing MCP servers".into());
    }
    mcp::validate_config(&config)?;
    let mut map = serde_json::Map::new();
    for server in &config.servers {
        map.insert(
            server.name.clone(),
            serde_json::to_value(server).map_err(|e| e.to_string())?,
        );
    }
    save_json(
        &state.settings_path.with_file_name("mcp.json"),
        &json!({"mcpServers":map}),
    )?;
    Ok(config)
}

#[tauri::command]
fn save_settings(
    settings: SettingsInput,
    state: State<Shared>,
    app: AppHandle,
) -> Result<Snapshot, String> {
    if state.busy.load(Ordering::SeqCst) {
        return Err("Wait for the current response before changing settings".into());
    }
    let base = settings.openai_base_url.trim().trim_end_matches('/');
    if !base.is_empty() {
        let url =
            reqwest::Url::parse(base).map_err(|_| "Enter a valid OPENAI_BASE_URL".to_string())?;
        if (url.scheme() != "https"
            && !(url.scheme() == "http"
                && matches!(url.host_str(), Some("localhost" | "127.0.0.1"))))
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err("OPENAI_BASE_URL must use HTTPS (or localhost HTTP), without embedded credentials or a fragment".into());
        }
    }
    let mut next = state.settings.read().unwrap().clone();
    next.openai_base_url = base.into();
    next.openai_model = settings.openai_model.trim().into();
    if settings.clear_api_key {
        next.openai_api_key.clear();
    } else if !settings.openai_api_key.is_empty() {
        next.openai_api_key = settings.openai_api_key;
    }
    let folder = state.folder().to_string_lossy().into_owned();
    let agents_prompt =
        fs::read_to_string(Path::new(&folder).join("AGENTS.md")).unwrap_or_default();
    if settings.system_prompt.trim().is_empty() || settings.system_prompt == agents_prompt {
        next.prompts.remove(&folder);
    } else {
        next.prompts.insert(folder, settings.system_prompt);
    }
    save_json(&state.settings_path, &next)?;
    *state.settings.write().unwrap() = next;
    let _ = app.emit("state-changed", json!({}));
    Ok(state.snapshot())
}

#[tauri::command]
async fn choose_folder(state: State<'_, Shared>, app: AppHandle) -> Result<Snapshot, String> {
    if state.busy.load(Ordering::SeqCst) {
        return Err("Wait for the current response before changing folders".into());
    }
    let picked = tokio::task::spawn_blocking({
        let app = app.clone();
        move || app.dialog().file().blocking_pick_folder()
    })
    .await
    .map_err(|e| e.to_string())?;
    let Some(picked) = picked else {
        return Ok(state.snapshot());
    };
    let path = picked.as_path().ok_or("Choose a local folder")?;
    let folder = fs::canonicalize(path).map_err(|e| e.to_string())?;
    if !folder.is_dir() {
        return Err("Choose a directory".into());
    }
    save_json(&state.settings_path.with_file_name("folder.json"), &folder)?;
    *state.folder.write().unwrap() = folder;
    state.epoch.fetch_add(1, Ordering::SeqCst);
    let _ = app.emit("folder-changed", json!({}));
    Ok(state.snapshot())
}

#[tauri::command]
fn stop_response(state: State<Shared>, app: AppHandle) -> Snapshot {
    state.epoch.fetch_add(1, Ordering::SeqCst);
    state.running.store(false, Ordering::SeqCst);
    if let Some(task) = state.task.lock().unwrap().take() {
        task.abort();
    }
    state.busy.store(false, Ordering::SeqCst);
    let _ = app.emit("state-changed", json!({}));
    state.snapshot()
}

#[tauri::command]
fn clear_chat(state: State<Shared>, app: AppHandle) -> Result<(), String> {
    if state.busy.load(Ordering::SeqCst) {
        return Err("Wait for the agent to finish before clearing chat".into());
    }
    append_log(&state, "chat_cleared", "")?;
    let _ = app.emit("chat-cleared", json!({}));
    Ok(())
}

#[tauri::command]
fn submit_user_message(
    message: String,
    state: State<Shared>,
    app: AppHandle,
) -> Result<(), String> {
    let message = message.trim();
    if message.is_empty() {
        return Err("Message cannot be empty".into());
    }
    if message.len() > 40_000 {
        return Err("Message is too long".into());
    }
    if state.busy.swap(true, Ordering::SeqCst) {
        return Err("Wait for the current response".into());
    }
    if !state.snapshot().configured {
        state.busy.store(false, Ordering::SeqCst);
        return Err("Add your model URL and model in Settings or this folder’s .env first".into());
    }
    if let Err(error) = append_log(&state, "user_message", message) {
        state.busy.store(false, Ordering::SeqCst);
        return Err(error);
    }
    let epoch = state.epoch.load(Ordering::SeqCst);
    state.running.store(true, Ordering::SeqCst);
    let message = message.to_string();
    let state = state.inner().clone();
    let _ = app.emit("state-changed", json!({}));
    let task_state = state.clone();
    let task_app = app.clone();
    let task = tauri::async_runtime::spawn(async move {
        let result = agent_turn(&task_app, &task_state, epoch, &message).await;
        if let Err(error) = result {
            if task_state.running.load(Ordering::SeqCst)
                && task_state.epoch.load(Ordering::SeqCst) == epoch
            {
                let _ = append_log(&task_state, "error", &error);
                let _ = task_app.emit("agent-message", json!({"kind":"error","message":error}));
            }
        }
        if task_state.epoch.load(Ordering::SeqCst) == epoch {
            task_state.running.store(false, Ordering::SeqCst);
            task_state.busy.store(false, Ordering::SeqCst);
            let _ = task_app.emit("state-changed", json!({}));
        }
    });
    *state.task.lock().unwrap() = Some(task);
    Ok(())
}

async fn agent_turn(
    app: &AppHandle,
    state: &Shared,
    epoch: u64,
    _message: &str,
) -> Result<(), String> {
    let folder = state.folder();
    let (settings, _) = state.effective_settings(&folder);
    let mut tools = Tools::connect(
        &folder,
        &state.settings_path.with_file_name("mcp.json"),
        app,
        &state.auth,
    )
    .await?;
    if state.epoch.load(Ordering::SeqCst) != epoch {
        return Ok(());
    }
    let prompt = prompt_for(&folder, &settings);
    let mut messages = Vec::new();
    if !prompt.trim().is_empty() {
        messages.push(json!({"role":"system","content":prompt}));
    }
    for item in history(state)
        .into_iter()
        .rev()
        .filter(|i| i.kind == "user_message" || i.kind == "agent_message")
        .take(20)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        messages.push(json!({"role":item.role,"content":item.text}));
    }
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(1800))
        .build()
        .map_err(|e| e.to_string())?;
    let endpoint = if settings.openai_base_url.ends_with("/chat/completions") {
        settings.openai_base_url.clone()
    } else {
        format!(
            "{}/chat/completions",
            settings.openai_base_url.trim_end_matches('/')
        )
    };
    let mut total_calls = 0usize;
    for _ in 0..MAX_STEPS {
        if !state.running.load(Ordering::SeqCst) || state.epoch.load(Ordering::SeqCst) != epoch {
            return Ok(());
        }
        let mut body = json!({"model":settings.openai_model,"messages":messages,"stream":true});
        if !tools.definitions.is_empty() {
            body["tools"] = json!(tools.definitions);
            body["tool_choice"] = json!("auto");
        }
        let mut request = client.post(&endpoint).json(&body);
        if !settings.openai_api_key.is_empty() {
            request = request.bearer_auth(&settings.openai_api_key);
        }
        // Reconnect only while establishing the request, before any response
        // has been streamed or any MCP operation has been issued for this step.
        let mut response = None;
        for attempt in 0..3 {
            let next = request
                .try_clone()
                .ok_or("Could not replay model request")?;
            match next.send().await {
                Ok(value) => {
                    response = Some(value);
                    break;
                }
                Err(error) if error.is_connect() && attempt < 2 => {
                    let _ = app.emit(
                        "agent-message",
                        json!({"kind":"info","message":"Model connection lost; reconnecting…"}),
                    );
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                }
                Err(error) => return Err(format!("Could not reach the model provider: {error}")),
            }
        }
        let response = response.ok_or("Could not reach the model provider")?;
        let status = response.status();
        if !status.is_success() {
            let bytes = response.bytes().await.map_err(|e| e.to_string())?;
            return Err(model_error(status, &bytes));
        }
        let reply = read_model_reply(response, app).await?;
        if !state.running.load(Ordering::SeqCst) || state.epoch.load(Ordering::SeqCst) != epoch {
            return Ok(());
        }
        let text = reply.get("content").and_then(Value::as_str).unwrap_or("");
        if let Some(thought) = reply
            .get("reasoning_content")
            .or_else(|| reply.get("reasoning"))
            .and_then(Value::as_str)
            .filter(|thought| !thought.is_empty())
        {
            append_log(state, "thinking", thought)?;
        }
        let calls = reply
            .get("tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if calls.len() > 8 || total_calls + calls.len() > 256 {
            return Err("Model requested too many MCP calls in one turn".into());
        }
        total_calls += calls.len();
        if calls.is_empty() {
            if text.trim().is_empty() {
                return Err("Model returned an empty response".into());
            }
            append_log(state, "agent_message", text)?;
            emit_chat(app, "assistant_end", text, json!({}));
            return Ok(());
        }
        if !text.trim().is_empty() {
            append_log(state, "agent_message", text)?;
        }
        // Reasoning is display-only; do not send provider-specific fields back
        // in the next OpenAI-compatible request.
        let mut reply = reply;
        if let Some(object) = reply.as_object_mut() {
            object.remove("reasoning_content");
            object.remove("reasoning");
        }
        messages.push(reply);
        for call in calls {
            if !state.running.load(Ordering::SeqCst) || state.epoch.load(Ordering::SeqCst) != epoch
            {
                return Ok(());
            }
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .ok_or("Tool call missing id")?;
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .ok_or("Tool call missing name")?;
            let args: Value = serde_json::from_str(
                call.pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}"),
            )
            .map_err(|e| format!("Invalid tool arguments: {e}"))?;
            let arguments = args.to_string();
            let args_preview = arguments.chars().take(4000).collect::<String>();
            let args_truncated = arguments.chars().count() > 4000;
            emit_chat(
                app,
                "tool_call",
                name,
                json!({"id":id,"arguments":args_preview,"arguments_truncated":args_truncated}),
            );
            let mut result = tools.call(name, args).await;
            if state.epoch.load(Ordering::SeqCst) != epoch {
                return Ok(());
            }
            let images = prepare_tool_result(&folder, &mut result);
            let mut result_text: String = result.to_string().chars().take(20_000).collect();
            // Long text blocks must not hide saved image paths from the model.
            if !images.is_empty() {
                result_text.push_str("\nSaved image files: ");
                result_text.push_str(
                    &images
                        .iter()
                        .filter_map(|image| image["path"].as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                );
            }
            let is_error = result.get("error").is_some()
                || result.get("isError").and_then(Value::as_bool) == Some(true);
            let display_text = result.to_string();
            let preview = display_text.chars().take(6000).collect::<String>();
            let truncated = display_text.chars().count() > 6000;
            emit_chat(
                app,
                "tool_result",
                name,
                json!({"id":id,"result":preview,"images":images,"is_error":is_error,"truncated":truncated}),
            );
            let _ = append_log_data(
                state,
                "tool",
                name,
                json!({"arguments":args_preview,"arguments_truncated":args_truncated,"result":preview,"images":images,"is_error":is_error,"truncated":truncated}),
            );
            messages.push(json!({"role":"tool","tool_call_id":id,"content":result_text}));
        }
    }
    Err(format!(
        "Agent stopped after {MAX_STEPS} steps; send a new message to continue"
    ))
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .setup(|app| {
            let state = initialize(app.handle()).map_err(std::io::Error::other)?;
            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            get_chat_history,
            read_tool_image,
            export_chat,
            get_mcp_servers,
            save_mcp_servers,
            list_mcp_tools,
            choose_folder,
            save_settings,
            stop_response,
            clear_chat,
            submit_user_message
        ])
        .run(tauri::generate_context!())
        .expect("datasetop could not start");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_env_reads_only_supported_values_without_overwriting_settings() {
        let dir = std::env::temp_dir().join(format!("datasetop-env-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".env"), "# comment\nexport OPENAI_BASE_URL=https://openrouter.ai/api/v1\nOPENAI_API_KEY='sk-test-value'\nOPENAI_MODEL=\"openai/gpt-6-luna\"\nOTHER_SECRET=ignored\n").unwrap();
        let values = read_folder_env(&dir);
        assert_eq!(values.len(), 3);
        assert_eq!(values["OPENAI_BASE_URL"], "https://openrouter.ai/api/v1");
        assert_eq!(values["OPENAI_API_KEY"], "sk-test-value");
        assert_eq!(values["OPENAI_MODEL"], "openai/gpt-6-luna");
        let state = Shared {
            folder: Arc::new(RwLock::new(dir.clone())),
            settings: Arc::new(RwLock::new(Settings {
                openai_model: "saved-model".into(),
                ..Settings::default()
            })),
            settings_path: dir.join("settings.json"),
            log_path: dir.join("chat.jsonl"),
            running: Arc::new(AtomicBool::new(false)),
            busy: Arc::new(AtomicBool::new(false)),
            epoch: Arc::new(AtomicU64::new(0)),
            task: Arc::new(Mutex::new(None)),
            auth: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        };
        let snapshot = state.snapshot();
        assert!(snapshot.configured);
        assert_eq!(snapshot.settings.openai_model, "saved-model");
        assert_eq!(
            snapshot.settings.env_fields,
            ["OPENAI_BASE_URL", "OPENAI_API_KEY"]
        );
        let serialized = serde_json::to_string(&snapshot).unwrap();
        assert!(!serialized.contains("sk-test-value"));
        fs::remove_dir_all(dir).unwrap();
    }
    use rmcp::{
        model::{CallToolRequestParams, ClientConfig, ProtocolVersion, Tool},
        transport::{ConfigureCommandExt, TokioChildProcess},
        ClientLifecycleMode, ClientServiceExt,
    };
    use tokio::process::Command;

    #[test]
    fn empty_folder_has_no_assigned_task_or_prompt() {
        let folder = std::env::temp_dir().join(format!("datasetop-empty-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        assert_eq!(prompt_for(&folder, &Settings::default()), "");
        fs::write(folder.join("AGENTS.md"), "Follow these instructions").unwrap();
        assert_eq!(
            prompt_for(&folder, &Settings::default()),
            "Follow these instructions"
        );
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn mcp_image_is_saved_and_replaced_with_path_not_base64() {
        let folder =
            std::env::temp_dir().join(format!("datasetop-media-test-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        let encoded = BASE64.encode(b"\x89PNG\r\n\x1a\nplaceholder");
        let mut result = json!({"content":[{"type":"image","mimeType":"image/png","data":encoded}], "structuredContent":{"data":encoded}});
        let images = prepare_tool_result(&folder, &mut result);
        assert_eq!(images.len(), 1);
        let path = PathBuf::from(images[0]["path"].as_str().unwrap());
        assert!(path.starts_with(fs::canonicalize(&folder).unwrap().join(".datasetop/media")));
        assert_eq!(fs::read(&path).unwrap(), b"\x89PNG\r\n\x1a\nplaceholder");
        assert!(!result.to_string().contains(&encoded));
        // Compare parsed values: serialized JSON escapes Windows path separators.
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains(path.to_str().unwrap()));
        assert_eq!(
            result["structuredContent"]["data"],
            "[binary media omitted]"
        );
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn ordinary_tool_data_is_not_removed() {
        let mut result = json!({"content":[{"type":"text","text":"ok","data":{"count":3}}],"structuredContent":{"data":{"count":3}}});
        prepare_tool_result(&std::env::temp_dir(), &mut result);
        assert_eq!(result["content"][0]["data"]["count"], 3);
        assert_eq!(result["structuredContent"]["data"]["count"], 3);
    }

    #[test]
    fn workspace_url_does_not_receive_a_saved_api_key() {
        let folder =
            std::env::temp_dir().join(format!("datasetop-key-test-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join(".env"),
            "OPENAI_BASE_URL=https://workspace.example/v1\nOPENAI_API_KEY=workspace-key\n",
        )
        .unwrap();
        let state = Shared {
            folder: Arc::new(RwLock::new(folder.clone())),
            settings: Arc::new(RwLock::new(Settings {
                openai_api_key: "saved-key".into(),
                ..Settings::default()
            })),
            settings_path: folder.join("settings.json"),
            log_path: folder.join("chat.jsonl"),
            running: Arc::new(AtomicBool::new(false)),
            busy: Arc::new(AtomicBool::new(false)),
            epoch: Arc::new(AtomicU64::new(0)),
            task: Arc::new(Mutex::new(None)),
            auth: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        };
        let (settings, _) = state.effective_settings(&folder);
        assert_eq!(settings.openai_api_key, "workspace-key");
        fs::remove_file(folder.join(".env")).unwrap();
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn invalid_media_is_never_written() {
        let folder = std::env::temp_dir();
        assert!(store_tool_image(
            &folder,
            &BASE64.encode(b"<svg onload='alert(1)'>"),
            "image/svg+xml"
        )
        .is_err());
        assert!(store_tool_image(&folder, "!not base64", "image/png").is_err());
        assert!(
            store_tool_image(&folder, &BASE64.encode(b"\x89PNG\r\n\x1a\n"), "image/jpeg").is_err()
        );
    }

    #[test]
    fn only_saved_media_files_are_readable() {
        let root =
            std::env::temp_dir().join(format!("datasetop-read-image-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        // The app stores a canonical folder path; resolve symlinked temp roots too.
        let folder = fs::canonicalize(&root).unwrap();
        let media = folder.join(".datasetop").join("media");
        fs::create_dir_all(&media).unwrap();
        let saved = media.join("saved.png");
        fs::write(&saved, b"\x89PNG\r\n\x1a\nbody").unwrap();
        assert!(resolve_tool_image(&folder, saved.to_str().unwrap()).is_ok());
        let outside = folder.join("outside.png");
        fs::write(&outside, b"\x89PNG\r\n\x1a\nbody").unwrap();
        assert!(resolve_tool_image(&folder, outside.to_str().unwrap()).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, media.join("link.png")).unwrap();
            assert!(resolve_tool_image(&folder, media.join("link.png").to_str().unwrap()).is_err());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_mcp_config_is_empty_and_existing_edits_are_preserved() {
        let dir = std::env::temp_dir().join(format!("datasetop-mcp-seed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp.json");
        assert!(mcp::read_config(&path).unwrap().servers.is_empty());
        assert!(!path.exists(), "first launch does not install an MCP");
        let edited = r#"{"mcpServers":{}}"#;
        fs::write(&path, edited).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            edited,
            "a user-emptied server list is never repopulated"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn chat_clear_marker_hides_previous_messages() {
        let dir = std::env::temp_dir().join(format!("datasetop-log-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let state = Shared {
            folder: Arc::new(RwLock::new(dir.clone())),
            settings: Arc::new(RwLock::new(Settings::default())),
            settings_path: dir.join("config.json"),
            log_path: dir.join("chat.jsonl"),
            running: Arc::new(AtomicBool::new(false)),
            busy: Arc::new(AtomicBool::new(false)),
            epoch: Arc::new(AtomicU64::new(0)),
            task: Arc::new(Mutex::new(None)),
            auth: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        };
        append_log(&state, "user_message", "old").unwrap();
        append_log(&state, "chat_cleared", "").unwrap();
        assert!(history(&state).is_empty());
        append_log(&state, "user_message", "new").unwrap();
        assert_eq!(history(&state)[0].text, "new");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn replayed_assistant_messages_use_provider_role() {
        let dir = std::env::temp_dir().join(format!("datasetop-role-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let state = Shared {
            folder: Arc::new(RwLock::new(dir.clone())),
            settings: Arc::new(RwLock::new(Settings::default())),
            settings_path: dir.join("config.json"),
            log_path: dir.join("chat.jsonl"),
            running: Arc::new(AtomicBool::new(false)),
            busy: Arc::new(AtomicBool::new(false)),
            epoch: Arc::new(AtomicU64::new(0)),
            task: Arc::new(Mutex::new(None)),
            auth: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        };
        append_log(&state, "user_message", "hello").unwrap();
        append_log(&state, "agent_message", "hi").unwrap();
        assert_eq!(
            history(&state)
                .iter()
                .map(|item| item.role.as_str())
                .collect::<Vec<_>>(),
            ["user", "assistant"]
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn provider_errors_show_useful_detail_not_nested_json() {
        let payload = json!({"error":{"message":"Provider returned error","metadata":{"raw":json!({"error":{"message":"Invalid value: 'agent'. Supported values are assistant and user."}}).to_string()}}});
        let message = model_error(
            reqwest::StatusCode::BAD_REQUEST,
            &serde_json::to_vec(&payload).unwrap(),
        );
        assert!(message.contains("Invalid value: 'agent'"));
        assert!(!message.contains("metadata"));
    }

    #[test]
    fn streamed_text_and_tool_arguments_are_reassembled_in_order() {
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut calls = std::collections::BTreeMap::new();
        let chunks = [
            json!({"choices":[{"delta":{"content":"Hello "}}]}),
            json!({"choices":[{"delta":{"content":"world","tool_calls":[{"index":0,"id":"call_1","function":{"name":"mcp__filesystem__read_text_file","arguments":"{\"path\":\""}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"a.txt\"}"}}]}}]}),
        ];
        for chunk in chunks {
            apply_stream_event(
                chunk.to_string().as_bytes(),
                &mut text,
                &mut reasoning,
                &mut calls,
            )
            .unwrap();
        }
        assert_eq!(text, "Hello world");
        assert_eq!(calls[&0]["id"], "call_1");
        assert_eq!(calls[&0]["function"]["arguments"], "{\"path\":\"a.txt\"}");
        assert_eq!(
            apply_stream_event(b"[DONE]", &mut text, &mut reasoning, &mut calls).unwrap(),
            None
        );
    }

    #[test]
    fn reasoning_is_separate_from_visible_content() {
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut calls = std::collections::BTreeMap::new();
        let chunk =
            json!({"choices":[{"delta":{"reasoning_content":"considering","content":"answer"}}]});
        let emitted = apply_stream_event(
            chunk.to_string().as_bytes(),
            &mut text,
            &mut reasoning,
            &mut calls,
        )
        .unwrap();
        assert_eq!(
            emitted,
            Some((Some("answer".into()), Some("considering".into())))
        );
        assert_eq!(text, "answer");
        assert_eq!(reasoning, "considering");
    }

    #[test]
    fn chat_history_is_scoped_to_the_selected_folder() {
        let dir = std::env::temp_dir().join(format!("datasetop-folders-{}", std::process::id()));
        let first = dir.join("one");
        let second = dir.join("two");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let state = Shared {
            folder: Arc::new(RwLock::new(first.clone())),
            settings: Arc::new(RwLock::new(Settings::default())),
            settings_path: dir.join("settings.json"),
            log_path: dir.join("chat.jsonl"),
            running: Arc::new(AtomicBool::new(false)),
            busy: Arc::new(AtomicBool::new(false)),
            epoch: Arc::new(AtomicU64::new(0)),
            task: Arc::new(Mutex::new(None)),
            auth: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        };
        append_log(&state, "user_message", "first message").unwrap();
        *state.folder.write().unwrap() = second;
        assert!(history(&state).is_empty());
        append_log(&state, "user_message", "second message").unwrap();
        assert_eq!(history(&state)[0].text, "second message");
        *state.folder.write().unwrap() = first;
        assert_eq!(history(&state)[0].text, "first message");
        fs::remove_dir_all(dir).unwrap();
    }

    // --- Live .env + model + MCP smoke test ---------------------------------
    //
    // This test is ignored by default because it reaches the network and spawns a
    // real npm MCP server. It reads the repository-root .env at runtime and never
    // prints the API key or a full provider response.

    struct LiveEnv {
        base_url: String,
        endpoint: Option<String>,
        model: String,
        api_key: String,
    }

    // Returns the value for `key` without ever echoing other lines.
    fn live_env_value(contents: &str, key: &str) -> Option<String> {
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            let Some((name, value)) = line.split_once('=') else {
                continue;
            };
            if name.trim() != key {
                continue;
            }
            let value = value.trim().trim_matches('"').trim_matches('\'').trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
        None
    }

    // Walks up from the crate directory to find the repository-root .env.
    fn find_repo_env() -> Option<PathBuf> {
        let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        loop {
            let candidate = dir.join(".env");
            if candidate.is_file() {
                return Some(candidate);
            }
            if !dir.pop() {
                return None;
            }
        }
    }

    fn load_live_env() -> Option<LiveEnv> {
        let contents = fs::read_to_string(find_repo_env()?).ok()?;
        Some(LiveEnv {
            base_url: live_env_value(&contents, "DATASETOP_LLM_BASE_URL")?,
            endpoint: live_env_value(&contents, "DATASETOP_LLM_ENDPOINT"),
            model: live_env_value(&contents, "DATASETOP_LLM_MODEL")?,
            api_key: live_env_value(&contents, "DATASETOP_LLM_API_KEY")?,
        })
    }

    fn chat_completion_url(base_url: &str, endpoint: Option<&str>) -> String {
        let base = base_url.trim().trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            return base.to_string();
        }
        match endpoint.map(str::trim).filter(|path| !path.is_empty()) {
            Some(path) => format!("{base}/{}", path.trim_start_matches('/')),
            None => format!("{base}/chat/completions"),
        }
    }

    fn openai_tool_definition(tool: &Tool) -> Value {
        let raw = serde_json::to_value(tool).unwrap_or(Value::Null);
        json!({"type":"function","function":{
            "name": raw.get("name").and_then(Value::as_str).unwrap_or("read_text_file"),
            "description": raw.get("description").and_then(Value::as_str).unwrap_or("Read a text file from the folder"),
            "parameters": raw.get("inputSchema").cloned().unwrap_or_else(|| json!({"type":"object","properties":{}})),
        }})
    }

    // Removes the sample folder even when an assertion panics.
    struct SampleFolder {
        dir: PathBuf,
    }

    impl SampleFolder {
        fn create(invoice: &str) -> std::io::Result<Self> {
            let dir = std::env::temp_dir().join(format!(
                "datasetop-live-smoke-{}-{}",
                std::process::id(),
                Utc::now().timestamp_micros()
            ));
            fs::create_dir_all(&dir)?;
            fs::write(dir.join("invoice.txt"), invoice)?;
            Ok(Self { dir })
        }
    }

    impl Drop for SampleFolder {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    const SAMPLE_INVOICE: &str = "\
ACME INDUSTRIAL SUPPLY LLC
88 Foundry Road, Springfield

INVOICE #INV-2026-0417
Date: 2026-04-17

Bill To:
Northwind Analytics
1200 Harbor Street

Description            Qty   Unit      Amount
Copper fittings 12mm    40   $12.50    $500.00
Sealing gasket kit       6   $45.25    $271.50
Pressure gauge p-200    12   $30.00    $360.00
Freight and handling     1   $103.06   $103.06

Subtotal:  $1,234.56
Tax (0%):  $0.00
Total Due: $1,234.56

Payment terms: Net 30. Thank you for your business.
";

    const SAMPLE_TOTAL: &str = "1234.56";

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "live test: starts the npm filesystem MCP server and calls an OpenAI-compatible model using the repository .env"]
    async fn live_env_model_mcp_folder_smoke() {
        match tokio::time::timeout(
            Duration::from_secs(110),
            live_env_model_mcp_folder_smoke_inner(),
        )
        .await
        {
            Ok(()) => {}
            Err(_) => panic!("live smoke test exceeded its 120s budget"),
        }
    }

    async fn live_env_model_mcp_folder_smoke_inner() {
        let Some(env) = load_live_env() else {
            eprintln!("live_env_model_mcp_folder_smoke: DATASETOP_LLM_BASE_URL/MODEL/API_KEY not found in .env; skipping");
            return;
        };
        let sample =
            SampleFolder::create(SAMPLE_INVOICE).expect("create the sample invoice folder");
        let invoice = sample.dir.join("invoice.txt");

        // Launch the real filesystem MCP server with the SDK's 2026 -> 2025 fallback.
        let root_arg = sample.dir.to_string_lossy().into_owned();
        let executable = if cfg!(windows) { "npx.cmd" } else { "npx" };
        let transport = TokioChildProcess::new(Command::new(executable).configure(|cmd| {
            cmd.args([
                "--yes",
                "@modelcontextprotocol/server-filesystem",
                &root_arg,
            ]);
            cmd.kill_on_drop(true);
        }))
        .expect("launch the filesystem MCP server (is Node.js/npm installed?)");
        let lifecycle = ClientLifecycleMode::Auto {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            legacy_version: Some(ProtocolVersion::V_2025_11_25),
        };
        let client = ClientConfig::default()
            .serve_with_lifecycle(transport, lifecycle)
            .await
            .expect("filesystem MCP server handshake failed");

        let tools = client
            .list_all_tools()
            .await
            .expect("MCP tools/list failed");
        let read_tool = tools
            .iter()
            .find(|tool| tool.name == "read_text_file")
            .expect("filesystem MCP server does not expose read_text_file");
        let tool_definition = openai_tool_definition(read_tool);

        let endpoint = chat_completion_url(&env.base_url, env.endpoint.as_deref());
        let http = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("build HTTP client");
        let system = "You are a careful assistant. Use the available tools to read files from the workspace. Never guess file contents.";
        let user = format!(
            "Read the file {} using the read_text_file tool. Then tell me the total amount due.",
            invoice.display()
        );
        let request = json!({
            "model": env.model.clone(),
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "tools": [tool_definition.clone()],
            "tool_choice": "auto",
            "temperature": 0,
        });
        let response = http
            .post(&endpoint)
            .bearer_auth(&env.api_key)
            .json(&request)
            .send()
            .await
            .expect("model request failed");
        let status = response.status();
        let body = response.bytes().await.expect("read model response");
        assert!(
            status.is_success(),
            "model endpoint returned HTTP {} (body redacted)",
            status.as_u16()
        );
        let reply: Value = serde_json::from_slice(&body).expect("model returned invalid JSON");
        let message = reply
            .pointer("/choices/0/message")
            .expect("model response had no choices[0].message")
            .clone();
        let calls = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let call = calls
            .iter()
            .find(|call| {
                call.pointer("/function/name").and_then(Value::as_str) == Some("read_text_file")
            })
            .expect("model did not request the read_text_file tool");
        let arguments: Value = serde_json::from_str(
            call.pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}"),
        )
        .expect("model returned invalid tool arguments");
        let requested = arguments
            .get("path")
            .and_then(Value::as_str)
            .expect("tool call had no path argument");
        let canonical_root = fs::canonicalize(&sample.dir).unwrap();
        let canonical_file =
            fs::canonicalize(requested).expect("model requested a path that does not exist");
        assert!(
            canonical_file.starts_with(&canonical_root),
            "model requested a path outside the sample folder"
        );
        assert_eq!(
            canonical_file.file_name().and_then(|name| name.to_str()),
            Some("invoice.txt")
        );

        // Execute the real MCP tool for the path the model chose.
        let mcp_result = client
            .call_tool(
                CallToolRequestParams::new("read_text_file")
                    .with_arguments(json!({"path": requested}).as_object().unwrap().clone()),
            )
            .await
            .expect("MCP read_text_file call failed");
        assert!(
            mcp_result.is_error != Some(true),
            "MCP read_text_file reported an error"
        );
        let mcp_text = serde_json::to_string(&mcp_result).unwrap();
        assert!(
            mcp_text.contains("1,234.56") || mcp_text.contains("1234.56"),
            "MCP result did not contain the invoice total"
        );

        // Feed the real tool output back and check the model's answer.
        let follow_up = vec![
            json!({"role": "system", "content": system}),
            json!({"role": "user", "content": user}),
            message,
            json!({"role": "tool", "tool_call_id": call.get("id").and_then(Value::as_str).unwrap_or("call_read_text_file"), "content": mcp_text}),
            json!({"role": "user", "content": "Reply with only the total amount due as a plain number such as 1234.56 and nothing else."}),
        ];
        let follow_up_request = json!({"model": env.model.clone(), "messages": follow_up, "tools": [tool_definition], "temperature": 0});
        let follow_up_response = http
            .post(&endpoint)
            .bearer_auth(&env.api_key)
            .json(&follow_up_request)
            .send()
            .await
            .expect("follow-up model request failed");
        let follow_up_status = follow_up_response.status();
        let follow_up_body = follow_up_response
            .bytes()
            .await
            .expect("read follow-up model response");
        assert!(
            follow_up_status.is_success(),
            "model endpoint returned HTTP {} on the follow-up (body redacted)",
            follow_up_status.as_u16()
        );
        let follow_up_reply: Value =
            serde_json::from_slice(&follow_up_body).expect("follow-up response was not valid JSON");
        let answer = follow_up_reply
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or("");
        let digits: String = answer
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        assert!(
            digits.contains(SAMPLE_TOTAL),
            "model follow-up did not contain the expected invoice total (reply redacted)"
        );

        client.cancel().await.ok();
    }
}
