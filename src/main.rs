use base64::Engine;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use hyper_util::server::conn::auto::Builder;
use std::convert::Infallible;
use std::env;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Instant, SystemTime};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::signal;
use zip::write::SimpleFileOptions;

// ── ANSI codes ────────────────────────────────────────────────────────

const RST: &str = "\x1b[0m";
const B: &str = "\x1b[1m";
const D: &str = "\x1b[2m";
const CY: &str = "\x1b[36m";
const GR: &str = "\x1b[32m";
const YL: &str = "\x1b[33m";
const RD: &str = "\x1b[31m";
const MG: &str = "\x1b[35m";
const BL: &str = "\x1b[34m";
const BGG: &str = "\x1b[42m";
const BGR: &str = "\x1b[41m";
const BGY: &str = "\x1b[43m";
const BK: &str = "\x1b[30m";
const WH: &str = "\x1b[37m";

fn status_style(code: u16) -> String {
    match code {
        200..=299 => format!("{BGG}{BK}{B} {code} {RST}"),
        300..=399 => format!("{BGY}{BK}{B} {code} {RST}"),
        400..=599 => format!("{BGR}{WH}{B} {code} {RST}"),
        _ => format!("{D}{code}{RST}"),
    }
}

fn method_style(m: &str) -> String {
    match m {
        "GET" => format!("{GR}{B}{m}{RST}"),
        "POST" => format!("{BL}{B}{m}{RST}"),
        "PUT" => format!("{YL}{B}{m}{RST}"),
        "DELETE" => format!("{RD}{B}{m}{RST}"),
        _ => format!("{D}{m}{RST}"),
    }
}

fn ts() -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{D}{:02}:{:02}:{:02}{RST}", (now / 3600) % 24, (now / 60) % 60, now % 60)
}

// ── Shared server config ──────────────────────────────────────────────

struct ServerConfig {
    root: PathBuf,
    auth: Option<String>, // base64 encoded "user:pass"
}

// ── Content types ─────────────────────────────────────────────────────

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "application/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("pdf") => "application/pdf",
        Some("wasm") => "application/wasm",
        Some("xml") => "application/xml; charset=utf-8",
        Some("txt" | "md") => "text/plain; charset=utf-8",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mp3") => "audio/mpeg",
        Some("ogg") => "audio/ogg",
        Some("webp") => "image/webp",
        _ => "application/octet-stream",
    }
}

fn file_icon(path: &Path, is_dir: bool) -> &'static str {
    if is_dir { return "📁"; }
    match path.extension().and_then(|e| e.to_str()) {
        Some("html" | "htm") => "🌐",
        Some("css") => "🎨",
        Some("js" | "mjs" | "ts") => "⚡",
        Some("json" | "toml" | "yaml" | "yml") => "⚙️",
        Some("png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "ico") => "🖼️",
        Some("mp4" | "webm" | "mov" | "avi") => "🎬",
        Some("mp3" | "ogg" | "wav" | "flac") => "🎵",
        Some("pdf") => "📕",
        Some("zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar") => "📦",
        Some("rs") => "🦀",
        Some("py") => "🐍",
        Some("go") => "🐹",
        Some("rb") => "💎",
        Some("sh" | "bash" | "zsh") => "🐚",
        Some("md" | "txt" | "log") => "📄",
        Some("lock") => "🔒",
        Some("doc" | "docx") => "📝",
        Some("xls" | "xlsx" | "csv") => "📊",
        Some("c" | "cpp" | "h" | "hpp") => "⚙️",
        Some("java" | "jar") => "☕",
        Some("sql" | "db" | "sqlite") => "🗄️",
        _ => "📄",
    }
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB { format!("{:.1} GB", bytes as f64 / GB as f64) }
    else if bytes >= MB { format!("{:.1} MB", bytes as f64 / MB as f64) }
    else if bytes >= KB { format!("{:.1} KB", bytes as f64 / KB as f64) }
    else { format!("{bytes} B") }
}

fn format_speed(bytes: u64, elapsed_ms: u64) -> String {
    if elapsed_ms == 0 { return format_size(bytes) + "/s"; }
    let bps = (bytes as f64 / elapsed_ms as f64) * 1000.0;
    format!("{}/s", format_size(bps as u64))
}

fn format_time(secs: u64) -> String {
    let d = secs / 86400;
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    if d / 365 > 0 { format!("{}y ago", d / 365) }
    else if d / 30 > 0 { format!("{}mo ago", d / 30) }
    else if d > 0 { format!("{d}d ago") }
    else if h > 0 { format!("{h}h ago") }
    else if m > 0 { format!("{m}m ago") }
    else { format!("{s}s ago") }
}

fn percent_encode(input: &str) -> String {
    let mut r = String::new();
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => r.push(b as char),
            _ => r.push_str(&format!("%{:02X}", b)),
        }
    }
    r
}

fn percent_decode(input: &str) -> String {
    let mut result = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16,
            ) { result.push(byte); i += 3; continue; }
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
}

fn html_escape(input: &str) -> String {
    input.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

// ── QR code generation (terminal) ─────────────────────────────────────

fn render_qr_terminal(url: &str) {
    use qrcode::QrCode;
    let code = match QrCode::new(url.as_bytes()) {
        Ok(c) => c,
        Err(_) => return,
    };
    let modules = code.to_colors();
    let width = code.width();
    // Use Unicode block characters: upper half / lower half / full / empty
    // Each printed row covers 2 QR rows
    eprintln!();
    let rows: Vec<&[qrcode::Color]> = modules.chunks(width).collect();
    for y in (0..rows.len()).step_by(2) {
        eprint!("    ");
        for x in 0..width {
            let top = rows[y][x] == qrcode::Color::Dark;
            let bot = if y + 1 < rows.len() { rows[y + 1][x] == qrcode::Color::Dark } else { false };
            match (top, bot) {
                (true, true) => eprint!("█"),
                (true, false) => eprint!("▀"),
                (false, true) => eprint!("▄"),
                (false, false) => eprint!(" "),
            }
        }
        eprintln!();
    }
    eprintln!();
}

// ── IP geolocation ────────────────────────────────────────────────────

async fn geolocate_ip(ip: &str) -> Option<String> {
    // Skip private/local IPs
    if ip.starts_with("127.") || ip.starts_with("10.") || ip.starts_with("192.168.")
        || ip.starts_with("172.") || ip == "::1" || ip.starts_with("fe80") {
        return None;
    }
    // Use ip-api.com (free, no key needed, 45 req/min)
    let url = format!("http://ip-api.com/json/{}?fields=city,country,query", ip);
    let output = Command::new("curl")
        .arg("-s").arg("-m").arg("2") // 2 second timeout
        .arg(&url)
        .output()
        .await
        .ok()?;
    let body = String::from_utf8_lossy(&output.stdout);
    // Simple JSON parsing without serde
    let city = extract_json_string(&body, "city")?;
    let country = extract_json_string(&body, "country")?;
    if city.is_empty() && country.is_empty() { return None; }
    if city.is_empty() { Some(country) }
    else { Some(format!("{city}, {country}")) }
}

fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{key}\":\"");
    let start = json.find(&pattern)? + pattern.len();
    let end = json[start..].find('"')? + start;
    Some(json[start..end].to_string())
}

// ── Simple JSON array extraction ──────────────────────────────────────

fn extract_json_string_array(json: &str, key: &str) -> Vec<String> {
    let pattern = format!("\"{}\"", key);
    let key_pos = match json.find(&pattern) { Some(p) => p, None => return vec![] };
    let after_key = &json[key_pos + pattern.len()..];
    // Find the opening bracket
    let bracket_pos = match after_key.find('[') { Some(p) => p, None => return vec![] };
    let after_bracket = &after_key[bracket_pos + 1..];
    let close_pos = match after_bracket.find(']') { Some(p) => p, None => return vec![] };
    let array_content = &after_bracket[..close_pos];
    array_content.split(',')
        .filter_map(|s| {
            let trimmed = s.trim().trim_matches('"');
            if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
        })
        .collect()
}

// ── CSS ───────────────────────────────────────────────────────────────

const PAGE_CSS: &str = r##"
:root {
    --bg: #0a0a0f; --surface: #12121a; --border: #1e1e2e;
    --text: #c8c8d8; --text-dim: #5a5a72;
    --accent: #7c6cf0; --accent-light: #9d8ff5;
    --hover: #1a1a28; --green: #4ade80; --red: #f87171;
}
[data-theme="light"] {
    --bg: #f5f5f7; --surface: #ffffff; --border: #e0e0e6;
    --text: #1a1a2e; --text-dim: #8888a0;
    --accent: #6c5ce7; --accent-light: #5b4cdb;
    --hover: #eeeef2; --green: #22c55e; --red: #ef4444;
}
* { margin: 0; padding: 0; box-sizing: border-box; }
body {
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', system-ui, sans-serif;
    background: var(--bg); color: var(--text); min-height: 100vh;
    transition: background 0.2s, color 0.2s;
}
.header {
    background: var(--surface); border-bottom: 1px solid var(--border);
    padding: 14px 32px; transition: background 0.2s;
}
.header-inner {
    max-width: 960px; margin: 0 auto;
    display: flex; align-items: center; justify-content: space-between;
}
.header-left { display: flex; align-items: center; gap: 16px; }
.logo { display: flex; align-items: center; gap: 8px; }
.logo-dot {
    width: 7px; height: 7px; background: var(--green); border-radius: 50%;
    box-shadow: 0 0 6px var(--green); animation: pulse 2s ease-in-out infinite;
}
@keyframes pulse { 0%,100%{opacity:1} 50%{opacity:0.4} }
.logo-text {
    font-size: 15px; font-weight: 700; color: var(--accent-light);
    letter-spacing: 1.5px; text-transform: uppercase;
    font-family: 'SF Mono','Cascadia Code','JetBrains Mono',monospace;
}
.breadcrumbs {
    font-size: 13px; color: var(--text-dim);
    font-family: 'SF Mono','Cascadia Code','JetBrains Mono',monospace;
}
.breadcrumbs a { color: var(--accent-light); text-decoration: none; }
.breadcrumbs a:hover { text-decoration: underline; }
.breadcrumbs .sep { margin: 0 5px; color: var(--border); }
.header-right { display: flex; align-items: center; gap: 12px; }
.theme-btn {
    background: none; border: 1px solid var(--border); border-radius: 6px;
    padding: 6px 8px; cursor: pointer; color: var(--text-dim); font-size: 14px;
    transition: all 0.15s;
}
.theme-btn:hover { border-color: var(--accent); color: var(--text); }
.container { max-width: 960px; margin: 0 auto; padding: 16px 32px 48px; }
.search-bar {
    width: 100%; padding: 10px 14px; margin-bottom: 14px;
    background: var(--surface); border: 1px solid var(--border); border-radius: 8px;
    color: var(--text); font-size: 14px; outline: none; transition: border-color 0.15s;
    font-family: inherit;
}
.search-bar:focus { border-color: var(--accent); }
.search-bar::placeholder { color: var(--text-dim); }
.stats {
    display: flex; gap: 20px; padding: 10px 0 14px;
    font-size: 12px; color: var(--text-dim);
}
table { width: 100%; border-collapse: collapse; }
thead th {
    text-align: left; font-size: 11px; font-weight: 600;
    text-transform: uppercase; letter-spacing: 0.5px;
    color: var(--text-dim); padding: 8px 12px;
    border-bottom: 1px solid var(--border);
}
.entry { cursor: pointer; transition: background 0.1s; }
.entry:hover { background: var(--hover); }
.entry td {
    padding: 9px 12px; border-bottom: 1px solid var(--border); font-size: 14px;
}
.entry.hidden-by-search { display: none; }
.icon { width: 32px; text-align: center; }
.name a { color: var(--text); text-decoration: none; }
.name a:hover { color: var(--accent-light); }
.dir a { color: var(--accent-light); font-weight: 500; }
.size,.modified { text-align:right; color:var(--text-dim); width:100px; font-size:13px; }
.dim { color: var(--text-dim); }
.empty { padding:48px; text-align:center; color:var(--text-dim); font-size:14px; }
.upload-zone {
    border: 2px dashed var(--border); border-radius: 8px; padding: 24px;
    text-align: center; margin-bottom: 14px; transition: all 0.2s; cursor: pointer;
}
.upload-zone:hover,.upload-zone.dragover { border-color:var(--accent); background:rgba(124,108,240,0.05); }
.upload-zone.dragover { background:rgba(124,108,240,0.1); }
.upload-icon { font-size: 24px; margin-bottom: 6px; }
.upload-text { font-size: 13px; color: var(--text-dim); }
.upload-text strong { color: var(--accent-light); }
.upload-input { display: none; }
.upload-progress { margin-top: 10px; display: none; }
.upload-bar-bg { height: 4px; background: var(--border); border-radius: 2px; overflow: hidden; }
.upload-bar { height:100%; background:var(--accent); border-radius:2px; width:0%; transition:width 0.2s; }
.upload-status { font-size:12px; color:var(--text-dim); margin-top:6px; }
.upload-status.error { color:var(--red); }
.upload-status.success { color:var(--green); }
.no-results { display:none; padding:32px; text-align:center; color:var(--text-dim); font-size:14px; }
.cb { width:32px; text-align:center; padding-right:0 !important; }
.cb input[type="checkbox"] {
    width:15px; height:15px; cursor:pointer; accent-color:var(--accent);
    vertical-align:middle; margin:0;
}
thead .cb { vertical-align:middle; }
.sel-bar {
    position:fixed; bottom:0; left:0; right:0;
    background:var(--surface); border-top:1px solid var(--border);
    padding:12px 32px; display:none; align-items:center; justify-content:center; gap:16px;
    z-index:100; box-shadow:0 -4px 12px rgba(0,0,0,0.2);
    animation:slideUp 0.2s ease-out;
}
@keyframes slideUp { from{transform:translateY(100%)} to{transform:translateY(0)} }
.sel-bar.visible { display:flex; }
.sel-count { font-size:14px; color:var(--text); font-weight:500; }
.sel-btn {
    background:var(--accent); color:#fff; border:none; border-radius:6px;
    padding:8px 20px; font-size:14px; font-weight:600; cursor:pointer;
    transition:opacity 0.15s; font-family:inherit;
}
.sel-btn:hover { opacity:0.85; }
.sel-btn.sel-clear {
    background:none; border:1px solid var(--border); color:var(--text-dim);
}
.sel-btn.sel-clear:hover { border-color:var(--accent); color:var(--text); }
@media (max-width:640px) {
    .header{padding:12px 16px} .container{padding:12px 16px 32px}
    .modified{display:none} .header-inner{flex-direction:column;align-items:flex-start;gap:8px}
    .header-right{align-self:flex-end;margin-top:-28px}
}
"##;

// ── Directory listing HTML ────────────────────────────────────────────

async fn render_directory(dir_path: &Path, uri_path: &str, root: &Path) -> String {
    let mut entries: Vec<(String, bool, u64, u64)> = Vec::new();
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();

    if let Ok(mut rd) = fs::read_dir(dir_path).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') { continue; }
            let meta = entry.metadata().await.ok();
            let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let mod_ago = meta.as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| now.saturating_sub(d.as_secs())).unwrap_or(0);
            entries.push((name, is_dir, size, mod_ago));
        }
    }

    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));

    let display_path = if uri_path == "/" { "/" } else { uri_path.trim_end_matches('/') };
    let breadcrumbs = build_breadcrumbs(uri_path);
    let mut rows = String::new();

    if dir_path != root {
        let parent = if uri_path.len() > 1 {
            let t = uri_path.trim_end_matches('/');
            match t.rfind('/') { Some(0) => "/".into(), Some(p) => t[..p].into(), None => "/".into() }
        } else { "/".to_string() };
        rows.push_str(&format!(
            r#"<tr class="entry" data-name=".." onclick="window.location='{parent}'"><td class="cb"></td><td class="icon">📁</td><td class="name"><a href="{parent}">..</a></td><td class="size dim">&mdash;</td><td class="modified dim">&mdash;</td></tr>"#,
        ));
    }

    for (name, is_dir, size, mod_ago) in &entries {
        let href = if uri_path.ends_with('/') { format!("{uri_path}{}", percent_encode(name)) }
                   else { format!("{uri_path}/{}", percent_encode(name)) };
        let href_s = if *is_dir { format!("{href}/") } else { href.clone() };
        let icon = file_icon(Path::new(name), *is_dir);
        let sz = if *is_dir { "&mdash;".into() } else { format_size(*size) };
        let mt = format_time(*mod_ago);
        let nc = if *is_dir { "name dir" } else { "name" };
        let esc = html_escape(name);
        let suf = if *is_dir { "/" } else { "" };
        rows.push_str(&format!(
            r#"<tr class="entry" data-name="{}" data-href="{href_s}" onclick="rowClick(event,this)"><td class="cb"><input type="checkbox" class="sel-cb" data-path="{href_s}" onclick="event.stopPropagation();updateSelection()"></td><td class="icon">{icon}</td><td class="{nc}"><a href="{href_s}">{esc}{suf}</a></td><td class="size">{sz}</td><td class="modified">{mt}</td></tr>"#,
            html_escape(&name.to_lowercase()),
        ));
    }

    let fc = entries.iter().filter(|e| !e.1).count();
    let dc = entries.iter().filter(|e| e.1).count();
    let ts: u64 = entries.iter().filter(|e| !e.1).map(|e| e.2).sum();

    let upload_target = if uri_path.ends_with('/') { format!("{uri_path}__upload") }
                        else { format!("{uri_path}/__upload") };
    let download_target = if uri_path.ends_with('/') { format!("{uri_path}__download") }
                          else { format!("{uri_path}/__download") };

    format!(
        r##"<!DOCTYPE html><html lang="en"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>leak {display_path}</title>
<style>{PAGE_CSS}</style>
</head><body>
<div class="header"><div class="header-inner">
  <div class="header-left">
    <div class="logo"><div class="logo-dot"></div><div class="logo-text">leak</div></div>
    <div class="breadcrumbs">{breadcrumbs}</div>
  </div>
  <div class="header-right">
    <button class="theme-btn" id="themeToggle" title="Toggle theme">◑</button>
  </div>
</div></div>
<div class="container">
  <div class="upload-zone" id="dropzone">
    <input type="file" class="upload-input" id="fileInput" multiple>
    <div class="upload-icon">↑</div>
    <div class="upload-text">Drop files here or <strong>click to browse</strong></div>
    <div class="upload-progress" id="uploadProgress">
      <div class="upload-bar-bg"><div class="upload-bar" id="uploadBar"></div></div>
      <div class="upload-status" id="uploadStatus"></div>
    </div>
  </div>
  <input type="text" class="search-bar" id="searchBar" placeholder="Search files..." autocomplete="off">
  <div class="stats">
    <span>{dc} folder{}</span>
    <span>{fc} file{}</span>
    <span>{}</span>
  </div>
  <table><thead><tr><th class="cb"><input type="checkbox" id="selectAll" title="Select all"></th><th></th><th>Name</th><th style="text-align:right">Size</th><th style="text-align:right">Modified</th></tr></thead>
  <tbody id="fileList">{rows}</tbody></table>
  <div class="no-results" id="noResults">No files match your search</div>
  {}
</div>
<div class="sel-bar" id="selBar">
  <span class="sel-count" id="selCount">0 selected</span>
  <button class="sel-btn" id="selDownload">Download</button>
  <button class="sel-btn sel-clear" id="selClear">Clear</button>
</div>
<script>
// Theme toggle
const html = document.documentElement;
const saved = localStorage.getItem('leak-theme');
if (saved) html.setAttribute('data-theme', saved);
document.getElementById('themeToggle').addEventListener('click', () => {{
  const next = html.getAttribute('data-theme') === 'light' ? '' : 'light';
  if (next) html.setAttribute('data-theme', next); else html.removeAttribute('data-theme');
  localStorage.setItem('leak-theme', next);
}});

// Search
const searchBar = document.getElementById('searchBar');
const fileList = document.getElementById('fileList');
const noResults = document.getElementById('noResults');
searchBar.addEventListener('input', () => {{
  const q = searchBar.value.toLowerCase();
  const rows = fileList.querySelectorAll('.entry');
  let visible = 0;
  rows.forEach(row => {{
    const name = row.getAttribute('data-name') || '';
    if (!q || name.includes(q) || name === '..') {{
      row.classList.remove('hidden-by-search');
      visible++;
    }} else {{
      row.classList.add('hidden-by-search');
    }}
  }});
  noResults.style.display = (visible === 0 && q) ? 'block' : 'none';
}});
// Focus search on / key
document.addEventListener('keydown', (e) => {{
  if (e.key === '/' && document.activeElement !== searchBar) {{
    e.preventDefault();
    searchBar.focus();
  }}
}});

// Selection
const selBar = document.getElementById('selBar');
const selCount = document.getElementById('selCount');
const selDownload = document.getElementById('selDownload');
const selClear = document.getElementById('selClear');
const selectAll = document.getElementById('selectAll');

function getCheckboxes() {{ return document.querySelectorAll('.sel-cb'); }}

function updateSelection() {{
  const cbs = getCheckboxes();
  const checked = document.querySelectorAll('.sel-cb:checked');
  const n = checked.length;
  if (n > 0) {{
    selBar.classList.add('visible');
    selCount.textContent = n + ' selected';
  }} else {{
    selBar.classList.remove('visible');
  }}
  selectAll.checked = cbs.length > 0 && checked.length === cbs.length;
  selectAll.indeterminate = checked.length > 0 && checked.length < cbs.length;
}}

function rowClick(e, row) {{
  if (e.target.tagName === 'A' || e.target.tagName === 'INPUT') return;
  const cb = row.querySelector('.sel-cb');
  if (cb) {{ cb.checked = !cb.checked; updateSelection(); }}
}}

selectAll.addEventListener('change', () => {{
  const state = selectAll.checked;
  getCheckboxes().forEach(cb => {{ if (!cb.closest('.hidden-by-search')) cb.checked = state; }});
  updateSelection();
}});

selClear.addEventListener('click', () => {{
  getCheckboxes().forEach(cb => cb.checked = false);
  selectAll.checked = false;
  updateSelection();
}});

selDownload.addEventListener('click', async () => {{
  const paths = Array.from(document.querySelectorAll('.sel-cb:checked')).map(cb => cb.dataset.path);
  if (!paths.length) return;
  selDownload.disabled = true;
  selDownload.textContent = 'Zipping...';
  try {{
    const r = await fetch('{download_target}', {{
      method: 'POST',
      headers: {{'Content-Type': 'application/json'}},
      body: JSON.stringify({{files:paths}})
    }});
    if (!r.ok) {{ alert('Download failed: ' + await r.text()); return; }}
    const blob = await r.blob();
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = 'leak-download.zip';
    a.click();
    URL.revokeObjectURL(a.href);
  }} catch(e) {{ alert('Download error: ' + e.message); }}
  finally {{ selDownload.disabled = false; selDownload.textContent = 'Download'; }}
}});

// Upload
const dropzone = document.getElementById('dropzone');
const fileInput = document.getElementById('fileInput');
const progress = document.getElementById('uploadProgress');
const bar = document.getElementById('uploadBar');
const status = document.getElementById('uploadStatus');

dropzone.addEventListener('click', () => fileInput.click());
dropzone.addEventListener('dragover', (e) => {{ e.preventDefault(); dropzone.classList.add('dragover'); }});
dropzone.addEventListener('dragleave', () => dropzone.classList.remove('dragover'));
dropzone.addEventListener('drop', (e) => {{ e.preventDefault(); dropzone.classList.remove('dragover'); if(e.dataTransfer.files.length) uploadFiles(e.dataTransfer.files); }});
fileInput.addEventListener('change', () => {{ if(fileInput.files.length) uploadFiles(fileInput.files); }});

async function uploadFiles(files) {{
  progress.style.display = 'block';
  status.className = 'upload-status';
  const total = files.length;
  let done = 0;
  for (const file of files) {{
    status.textContent = `Uploading ${{file.name}} (${{done+1}}/${{total}})...`;
    bar.style.width = `${{(done/total)*100}}%`;
    const fd = new FormData();
    fd.append('file', file);
    try {{
      const r = await fetch('{upload_target}', {{ method:'POST', body:fd }});
      if (!r.ok) {{ status.textContent = `Failed: ${{await r.text()}}`; status.className='upload-status error'; return; }}
      done++;
      bar.style.width = `${{(done/total)*100}}%`;
    }} catch(e) {{ status.textContent=`Error: ${{e.message}}`; status.className='upload-status error'; return; }}
  }}
  status.textContent = `${{done}} file${{done!==1?'s':''}} uploaded`;
  status.className = 'upload-status success';
  setTimeout(() => window.location.reload(), 600);
}}
</script></body></html>"##,
        if dc != 1 { "s" } else { "" },
        if fc != 1 { "s" } else { "" },
        format_size(ts),
        if entries.is_empty() { r#"<div class="empty">This directory is empty</div>"# } else { "" },
    )
}

fn build_breadcrumbs(uri_path: &str) -> String {
    let mut r = String::from(r#"<a href="/">~</a>"#);
    let parts: Vec<&str> = uri_path.split('/').filter(|p| !p.is_empty()).collect();
    let mut href = String::new();
    for (i, part) in parts.iter().enumerate() {
        href.push('/'); href.push_str(part);
        r.push_str(r#"<span class="sep">/</span>"#);
        if i == parts.len() - 1 { r.push_str(&format!("<span>{}</span>", html_escape(part))); }
        else { r.push_str(&format!(r#"<a href="{href}">{}</a>"#, html_escape(part))); }
    }
    r
}

// ── ZIP building ──────────────────────────────────────────────────────

fn add_path_to_zip(
    zip: &mut zip::ZipWriter<std::io::Cursor<Vec<u8>>>,
    fs_path: &Path,
    archive_name: &str,
    root: &Path,
    opts: SimpleFileOptions,
) -> std::io::Result<()> {
    if fs_path.is_file() {
        zip.start_file(archive_name, opts)?;
        let data = std::fs::read(fs_path)?;
        std::io::Write::write_all(zip, &data)?;
    } else if fs_path.is_dir() {
        let mut stack: Vec<(PathBuf, String)> = vec![(fs_path.to_path_buf(), archive_name.to_string())];
        while let Some((dir, prefix)) = stack.pop() {
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for entry in rd.flatten() {
                    let path = entry.path();
                    if !path.starts_with(root) { continue; }
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with('.') { continue; }
                    let arc_name = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
                    if path.is_file() {
                        zip.start_file(&arc_name, opts)?;
                        let data = std::fs::read(&path)?;
                        std::io::Write::write_all(zip, &data)?;
                    } else if path.is_dir() {
                        stack.push((path, arc_name));
                    }
                }
            }
        }
    }
    Ok(())
}

// ── Multipart parsing ─────────────────────────────────────────────────

struct UploadedFile { filename: String, data: Vec<u8> }

fn parse_multipart(body: &[u8], boundary: &str) -> Vec<UploadedFile> {
    let mut files = Vec::new();
    let delim = format!("--{boundary}").into_bytes();
    let mut starts: Vec<usize> = Vec::new();
    let mut i = 0;
    while i + delim.len() <= body.len() {
        if &body[i..i + delim.len()] == delim.as_slice() {
            starts.push(i + delim.len()); i += delim.len();
        } else { i += 1; }
    }
    for (idx, &start) in starts.iter().enumerate() {
        let end = if idx + 1 < starts.len() { starts[idx + 1] - delim.len() } else { body.len() };
        if start >= end { continue; }
        let part = &body[start..end];
        let part = if part.starts_with(b"\r\n") { &part[2..] } else { part };
        if part.starts_with(b"--") { continue; }
        let sep = match part.windows(4).position(|w| w == b"\r\n\r\n") { Some(p) => p, None => continue };
        let headers = String::from_utf8_lossy(&part[..sep]);
        let data = &part[sep + 4..];
        let data = if data.ends_with(b"\r\n") { &data[..data.len() - 2] } else { data };
        if let Some(filename) = extract_filename(&headers) {
            if !filename.is_empty() { files.push(UploadedFile { filename, data: data.to_vec() }); }
        }
    }
    files
}

fn extract_filename(headers: &str) -> Option<String> {
    for line in headers.lines() {
        if line.to_lowercase().contains("content-disposition") {
            if let Some(pos) = line.find("filename=\"") {
                let start = pos + 10;
                if let Some(end) = line[start..].find('"') {
                    let name = &line[start..start + end];
                    return Some(name.rsplit(['/', '\\']).next().unwrap_or(name).to_string());
                }
            }
        }
    }
    None
}

fn get_boundary(req: &Request<Incoming>) -> Option<String> {
    let ct = req.headers().get("content-type")?.to_str().ok()?;
    if !ct.contains("multipart/form-data") { return None; }
    Some(ct.split("boundary=").nth(1)?.trim().trim_matches('"').to_string())
}

// ── Auth ──────────────────────────────────────────────────────────────

fn check_auth(req: &Request<Incoming>, expected: &str) -> bool {
    if let Some(auth) = req.headers().get("authorization") {
        if let Ok(val) = auth.to_str() {
            if val.starts_with("Basic ") {
                return &val[6..] == expected;
            }
        }
    }
    false
}

fn auth_required_response() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("WWW-Authenticate", "Basic realm=\"leak\"")
        .header("Content-Type", "text/plain")
        .body(Full::new(Bytes::from("Authentication required")))
        .unwrap()
}

// ── HTTP core ─────────────────────────────────────────────────────────

fn http_response(status: StatusCode, body: impl Into<Bytes>, ctype: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("Content-Type", ctype)
        .header("Access-Control-Allow-Origin", "*")
        .body(Full::new(body.into()))
        .unwrap()
}

async fn serve(cfg: Arc<ServerConfig>, req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    // Auth check
    if let Some(ref expected) = cfg.auth {
        if !check_auth(&req, expected) {
            return Ok(auth_required_response());
        }
    }

    let uri_path = req.uri().path().to_string();
    let method = req.method().clone();
    let root = &cfg.root;

    // Upload handler
    if method == Method::POST && uri_path.ends_with("/__upload") {
        let dir_uri = uri_path.trim_end_matches("/__upload");
        let dir_uri = if dir_uri.is_empty() { "/" } else { dir_uri };
        let clean = dir_uri.trim_start_matches('/');
        let decoded = percent_decode(clean);
        let dir_path = root.join(&decoded);

        let canonical = match dir_path.canonicalize() {
            Ok(c) if c.starts_with(root) => c,
            _ if decoded.is_empty() => root.clone(),
            _ => return Ok(http_response(StatusCode::BAD_REQUEST, "Invalid path", "text/plain")),
        };
        if !canonical.is_dir() {
            return Ok(http_response(StatusCode::BAD_REQUEST, "Not a directory", "text/plain"));
        }

        let boundary = match get_boundary(&req) {
            Some(b) => b,
            None => return Ok(http_response(StatusCode::BAD_REQUEST, "Missing boundary", "text/plain")),
        };

        let upload_start = Instant::now();
        let body_bytes = match req.collect().await {
            Ok(c) => c.to_bytes(),
            Err(_) => return Ok(http_response(StatusCode::BAD_REQUEST, "Read failed", "text/plain")),
        };
        if body_bytes.len() > 500 * 1024 * 1024 {
            return Ok(http_response(StatusCode::PAYLOAD_TOO_LARGE, "500MB max", "text/plain"));
        }

        let files = parse_multipart(&body_bytes, &boundary);
        if files.is_empty() {
            return Ok(http_response(StatusCode::BAD_REQUEST, "No file in upload", "text/plain"));
        }

        let elapsed_ms = upload_start.elapsed().as_millis() as u64;

        for file in &files {
            let safe: String = file.filename.chars()
                .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' || c == ' ' { c } else { '_' })
                .collect();
            if safe.is_empty() || safe == "." || safe == ".." { continue; }
            let dest = canonical.join(&safe);
            if let Ok(parent) = dest.parent().unwrap_or(&canonical).canonicalize() {
                if !parent.starts_with(root) { continue; }
            }
            if let Ok(mut f) = tokio::fs::File::create(&dest).await {
                let _ = f.write_all(&file.data).await;
                let speed = format_speed(file.data.len() as u64, elapsed_ms);
                eprintln!(
                    "  {} {BL}{B}UPLOAD{RST} {CY}{}{RST} {D}({} at {}){RST}",
                    ts(), safe, format_size(file.data.len() as u64), speed,
                );
            }
        }
        return Ok(http_response(StatusCode::OK, "OK", "text/plain"));
    }

    // Download handler (multi-file ZIP)
    if method == Method::POST && uri_path.ends_with("/__download") {
        let body_bytes = match req.collect().await {
            Ok(c) => c.to_bytes(),
            Err(_) => return Ok(http_response(StatusCode::BAD_REQUEST, "Read failed", "text/plain")),
        };
        let body_str = String::from_utf8_lossy(&body_bytes);
        let paths = extract_json_string_array(&body_str, "files");
        if paths.is_empty() {
            return Ok(http_response(StatusCode::BAD_REQUEST, "No files specified", "text/plain"));
        }

        let root_clone = root.clone();
        let zip_result = tokio::task::spawn_blocking(move || {
            let buf = std::io::Cursor::new(Vec::new());
            let mut zip = zip::ZipWriter::new(buf);
            let opts = SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);

            for path_str in &paths {
                let clean = path_str.trim_start_matches('/');
                let decoded = percent_decode(clean);
                let fs_path = root_clone.join(&decoded);
                let canonical = match fs_path.canonicalize() {
                    Ok(c) if c.starts_with(&root_clone) => c,
                    _ if decoded.is_empty() => continue,
                    _ => continue,
                };
                // Use the last path component as the archive entry name
                let arc_name = canonical.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| decoded.clone());
                let _ = add_path_to_zip(&mut zip, &canonical, &arc_name, &root_clone, opts);
            }

            zip.finish().map(|c| c.into_inner())
        }).await;

        match zip_result {
            Ok(Ok(data)) => {
                let size = data.len();
                eprintln!(
                    "  {} {GR}{B}DOWNLOAD{RST} {CY}ZIP{RST} {D}({}){RST}",
                    ts(), format_size(size as u64),
                );
                return Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "application/zip")
                    .header("Content-Disposition", "attachment; filename=\"leak-download.zip\"")
                    .header("Access-Control-Allow-Origin", "*")
                    .body(Full::new(Bytes::from(data)))
                    .unwrap());
            }
            _ => return Ok(http_response(StatusCode::INTERNAL_SERVER_ERROR, "ZIP creation failed", "text/plain")),
        }
    }

    // GET handler
    let clean = uri_path.trim_start_matches('/');
    let decoded = percent_decode(clean);
    let file_path = root.join(&decoded);

    let canonical = match file_path.canonicalize() {
        Ok(c) if c.starts_with(root) => c,
        _ if decoded.is_empty() => root.clone(),
        _ => return Ok(http_response(StatusCode::NOT_FOUND, format!("404 Not Found: {uri_path}"), "text/plain; charset=utf-8")),
    };

    if canonical.is_dir() {
        let index = canonical.join("index.html");
        if index.exists() {
            if let Ok(contents) = fs::read(&index).await {
                return Ok(http_response(StatusCode::OK, contents, "text/html; charset=utf-8"));
            }
        }
        let html = render_directory(&canonical, &uri_path, root).await;
        return Ok(http_response(StatusCode::OK, html, "text/html; charset=utf-8"));
    }

    match fs::read(&canonical).await {
        Ok(contents) => Ok(http_response(StatusCode::OK, contents, content_type(&canonical))),
        Err(_) => Ok(http_response(StatusCode::NOT_FOUND, format!("404 Not Found: {uri_path}"), "text/plain; charset=utf-8")),
    }
}

// ── Tunnel ────────────────────────────────────────────────────────────

enum TunnelProvider { Localtunnel, Cloudflared, Serveo }
impl TunnelProvider {
    fn name(&self) -> &'static str {
        match self { Self::Localtunnel=>"localtunnel", Self::Cloudflared=>"cloudflared", Self::Serveo=>"serveo" }
    }
}

async fn which(cmd: &str) -> bool {
    Command::new("which").arg(cmd).stdout(Stdio::null()).stderr(Stdio::null())
        .status().await.map(|s| s.success()).unwrap_or(false)
}

async fn detect_tunnel() -> Option<TunnelProvider> {
    if which("lt").await { Some(TunnelProvider::Localtunnel) }
    else if which("cloudflared").await { Some(TunnelProvider::Cloudflared) }
    else if which("ssh").await { Some(TunnelProvider::Serveo) }
    else { None }
}

async fn start_tunnel(prov: &TunnelProvider, port: u16) -> Option<(tokio::process::Child, tokio::sync::oneshot::Receiver<String>)> {
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    let tx = std::sync::Mutex::new(Some(tx));

    macro_rules! drain {
        ($stream:expr) => {
            if let Some(s) = $stream {
                tokio::spawn(async move { let mut l = BufReader::new(s).lines(); while let Ok(Some(_)) = l.next_line().await {} });
            }
        }
    }
    macro_rules! scan_url {
        ($stream:expr, $pattern:expr) => {{
            let s = $stream?;
            let tx = tx;
            tokio::spawn(async move {
                let mut lines = BufReader::new(s).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some(url) = line.split_whitespace().find(|w| w.contains($pattern)) {
                        if let Some(sender) = tx.lock().unwrap().take() { let _ = sender.send(url.to_string()); }
                    }
                }
            });
        }}
    }

    match prov {
        TunnelProvider::Localtunnel => {
            let mut c = Command::new("lt").arg("--port").arg(port.to_string())
                .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().ok()?;
            let stdout = c.stdout.take(); let stderr = c.stderr.take();
            scan_url!(stdout, "https://");
            drain!(stderr);
            Some((c, rx))
        }
        TunnelProvider::Cloudflared => {
            let mut c = Command::new("cloudflared").arg("tunnel").arg("--url").arg(format!("http://127.0.0.1:{port}"))
                .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().ok()?;
            let stderr = c.stderr.take(); let stdout = c.stdout.take();
            scan_url!(stderr, ".trycloudflare.com");
            drain!(stdout);
            Some((c, rx))
        }
        TunnelProvider::Serveo => {
            let mut c = Command::new("ssh").arg("-o").arg("StrictHostKeyChecking=no")
                .arg("-o").arg("ServerAliveInterval=60")
                .arg("-R").arg(format!("80:localhost:{port}")).arg("serveo.net")
                .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().ok()?;
            let stdout = c.stdout.take(); let stderr = c.stderr.take();
            scan_url!(stdout, "https://");
            drain!(stderr);
            Some((c, rx))
        }
    }
}

// ── TLS ───────────────────────────────────────────────────────────────

fn generate_self_signed_tls() -> Result<tokio_rustls::TlsAcceptor, Box<dyn std::error::Error>> {
    let cert_params = rcgen::CertificateParams::new(vec!["localhost".to_string()])?;
    let key_pair = rcgen::KeyPair::generate()?;
    let cert = cert_params.self_signed(&key_pair)?;

    let cert_der = cert.der().clone();
    let key_der = key_pair.serialize_der();

    let certs = vec![rustls::pki_types::CertificateDer::from(cert_der.to_vec())];
    let key = rustls::pki_types::PrivateKeyDer::try_from(key_der)
        .map_err(|e| format!("key error: {e}"))?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

// ── Arg parsing ───────────────────────────────────────────────────────

struct Args {
    port: u16,
    dir: PathBuf,
    public: bool,
    auth: Option<(String, String)>, // (user, pass)
    tls: bool,
}

fn parse_args() -> Args {
    let raw: Vec<String> = env::args().skip(1).collect();

    if raw.iter().any(|a| a == "--help" || a == "-h") || raw.is_empty() {
        eprintln!();
        eprintln!("  {B}{CY}leak{RST} {D}v0.5.0{RST}  {D}file server with uploads, tunnels, and TLS{RST}");
        eprintln!();
        eprintln!("  {B}Usage:{RST}  leak {GR}<port>{RST} {D}[directory]{RST} {YL}[options]{RST}");
        eprintln!();
        eprintln!("  {B}Options:{RST}");
        eprintln!("    {YL}--public, -p{RST}          {D}expose via tunnel{RST}");
        eprintln!("    {YL}--auth user:pass{RST}       {D}require basic auth{RST}");
        eprintln!("    {YL}--tls{RST}                  {D}enable HTTPS (self-signed){RST}");
        eprintln!();
        eprintln!("  {B}Examples:{RST}");
        eprintln!("    {D}${RST} leak {GR}8080{RST}");
        eprintln!("    {D}${RST} leak {GR}8080{RST} {YL}--public{RST}");
        eprintln!("    {D}${RST} leak {GR}443{RST} ./dist {YL}--tls --auth admin:secret{RST}");
        eprintln!();
        std::process::exit(0);
    }

    let public = raw.iter().any(|a| a == "--public" || a == "-p");
    let tls = raw.iter().any(|a| a == "--tls");

    let auth = raw.iter().position(|a| a == "--auth")
        .and_then(|i| raw.get(i + 1))
        .and_then(|val| {
            let parts: Vec<&str> = val.splitn(2, ':').collect();
            if parts.len() == 2 { Some((parts[0].to_string(), parts[1].to_string())) } else { None }
        });

    let skip_flags: std::collections::HashSet<usize> = {
        let mut s = std::collections::HashSet::new();
        for (i, a) in raw.iter().enumerate() {
            if a.starts_with('-') { s.insert(i); if a == "--auth" { s.insert(i + 1); } }
        }
        s
    };
    let positional: Vec<&String> = raw.iter().enumerate()
        .filter(|(i, _)| !skip_flags.contains(i))
        .map(|(_, v)| v).collect();

    let port: u16 = match positional.first() {
        Some(p) => p.parse().unwrap_or_else(|_| { eprintln!("{RD}{B}Error:{RST} invalid port: {p}"); std::process::exit(1); }),
        None => { eprintln!("{RD}{B}Error:{RST} port required. Run {B}leak --help{RST}"); std::process::exit(1); }
    };

    let dir = positional.get(1).map(|d| PathBuf::from(d.as_str()))
        .unwrap_or_else(|| env::current_dir().expect("cannot read current directory"));

    Args { port, dir, public, auth, tls }
}

// ── Local IP detection ────────────────────────────────────────────────

fn get_local_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}

// ── Main ──────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let args = parse_args();

    let root = fs::canonicalize(&args.dir).await.unwrap_or_else(|_| {
        eprintln!("{RD}{B}Error:{RST} directory not found: {}", args.dir.display());
        std::process::exit(1);
    });

    let auth_b64 = args.auth.as_ref().map(|(u, p)| {
        base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"))
    });

    let cfg = Arc::new(ServerConfig { root: root.clone(), auth: auth_b64 });

    let scheme = if args.tls { "https" } else { "http" };
    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    let listener = TcpListener::bind(addr).await.unwrap_or_else(|e| {
        eprintln!("{RD}{B}Error:{RST} failed to bind to port {}: {e}", args.port);
        std::process::exit(1);
    });

    // TLS setup
    let tls_acceptor = if args.tls {
        match generate_self_signed_tls() {
            Ok(a) => Some(a),
            Err(e) => { eprintln!("{RD}{B}Error:{RST} TLS setup failed: {e}"); std::process::exit(1); }
        }
    } else { None };

    // Banner
    eprintln!();
    eprintln!("  {B}{CY}leak{RST} {D}v0.5.0{RST}");
    eprintln!();

    let local_url = format!("{scheme}://127.0.0.1:{}", args.port);
    eprintln!("  {B}{GR}●{RST} {B}Local:{RST}   {CY}{local_url}{RST}");

    if let Some(ip) = get_local_ip() {
        let net_url = format!("{scheme}://{ip}:{}", args.port);
        eprintln!("  {B}{GR}●{RST} {B}Network:{RST} {CY}{net_url}{RST}");
        // QR code for network URL
        render_qr_terminal(&net_url);
    }

    eprintln!("  {D}  Root:    {}{RST}", root.display());
    if args.auth.is_some() { eprintln!("  {D}  Auth:    enabled{RST}"); }
    if args.tls { eprintln!("  {D}  TLS:     self-signed{RST}"); }

    // Tunnel
    let mut _tunnel_child: Option<tokio::process::Child> = None;
    if args.public {
        match detect_tunnel().await {
            Some(provider) => {
                eprintln!("  {D}  Tunnel:  connecting via {}...{RST}", provider.name());
                match start_tunnel(&provider, args.port).await {
                    Some((child, rx)) => {
                        _tunnel_child = Some(child);
                        match tokio::time::timeout(std::time::Duration::from_secs(15), rx).await {
                            Ok(Ok(url)) => eprintln!("\x1b[1A\x1b[2K  {B}{MG}●{RST} {B}Public:{RST}  {CY}{B}{url}{RST}"),
                            _ => eprintln!("\x1b[1A\x1b[2K  {YL}●{RST} {B}Public:{RST}  {D}timed out{RST}"),
                        }
                    }
                    None => eprintln!("  {RD}●{RST} {B}Public:{RST}  {D}failed to start {}{RST}", provider.name()),
                }
            }
            None => {
                eprintln!("  {RD}●{RST} {B}Public:{RST}  {D}no tunnel tool found{RST}");
                eprintln!("  {D}  Install: npm i -g localtunnel | brew install cloudflared{RST}");
            }
        }
    }

    eprintln!();
    eprintln!("  {D}Ctrl+C to stop{RST}");
    eprintln!();

    // Track seen IPs for geolocation
    let seen_ips: Arc<tokio::sync::Mutex<std::collections::HashSet<String>>> =
        Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, remote) = match result {
                    Ok(conn) => conn,
                    Err(e) => { eprintln!("  {RD}accept error:{RST} {e}"); continue; }
                };

                let cfg = cfg.clone();
                let tls_acceptor = tls_acceptor.clone();
                let seen_ips = seen_ips.clone();

                tokio::spawn(async move {
                    // Geo-lookup for new IPs
                    let ip_str = remote.ip().to_string();
                    {
                        let mut seen = seen_ips.lock().await;
                        if !seen.contains(&ip_str) {
                            seen.insert(ip_str.clone());
                            let ip_clone = ip_str.clone();
                            tokio::spawn(async move {
                                if let Some(loc) = geolocate_ip(&ip_clone).await {
                                    eprintln!("  {} {MG}{B}CONNECT{RST} {CY}{}{RST} {D}({}){RST}", ts(), ip_clone, loc);
                                } else {
                                    eprintln!("  {} {MG}{B}CONNECT{RST} {CY}{}{RST}", ts(), ip_clone);
                                }
                            });
                        }
                    }

                    let svc = service_fn(move |req: Request<Incoming>| {
                        let cfg = cfg.clone();
                        let method = req.method().to_string();
                        let path = req.uri().path().to_string();
                        let remote = remote;
                        async move {
                            let resp = serve(cfg, req).await;
                            let code = resp.as_ref().map(|r| r.status().as_u16()).unwrap_or(500);
                            if method != "POST" || !path.ends_with("/__upload") {
                                println!("  {} {} {} {CY}{}{RST} {D}{}{RST}", ts(), status_style(code), method_style(&method), path, remote.ip());
                            }
                            resp
                        }
                    });

                    if let Some(acceptor) = tls_acceptor {
                        match acceptor.accept(stream).await {
                            Ok(tls_stream) => {
                                let io = TokioIo::new(tls_stream);
                                let _ = Builder::new(hyper_util::rt::TokioExecutor::new())
                                    .http1().serve_connection(io, svc).await;
                            }
                            Err(_) => {} // TLS handshake failed, ignore
                        }
                    } else {
                        let io = TokioIo::new(stream);
                        let _ = Builder::new(hyper_util::rt::TokioExecutor::new())
                            .http1().serve_connection(io, svc).await;
                    }
                });
            }
            _ = signal::ctrl_c() => {
                eprintln!("\n  {D}Shutting down...{RST}");
                if let Some(ref mut child) = _tunnel_child { let _ = child.kill().await; }
                break;
            }
        }
    }
}
