#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder};
use url::Url;

const TARGET_LABEL: &str = "target-browser";
const DEFAULT_TARGET_URL: &str = "https://www.google.com/";
const BRIDGE_SCHEME: &str = "button-automation";
const MAX_LOGS: usize = 500;
const HTML2CANVAS_SOURCE: &str =
    include_str!("../../node_modules/html2canvas/dist/html2canvas.min.js");

#[derive(Clone)]
struct SharedState {
    inner: Arc<Mutex<RuntimeState>>,
}

impl SharedState {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RuntimeState::default())),
        }
    }

    fn with_runtime<T>(&self, handler: impl FnOnce(&mut RuntimeState) -> T) -> T {
        let mut runtime = self.inner.lock().expect("runtime state mutex poisoned");
        handler(&mut runtime)
    }

    fn client_state(&self) -> ClientState {
        let runtime = self.inner.lock().expect("runtime state mutex poisoned");
        runtime.client_state()
    }
}

#[derive(Default)]
struct RuntimeState {
    bridge_port: u16,
    target_url: Option<String>,
    inspector_enabled: bool,
    running: bool,
    interval_ms: u64,
    selected: Option<SelectedElement>,
    snapshot: Option<PageSnapshot>,
    logs: VecDeque<LogEntry>,
    bridge_chunks: HashMap<String, BridgeChunkBuffer>,
    next_log_id: u64,
}

impl RuntimeState {
    fn client_state(&self) -> ClientState {
        ClientState {
            target_url: self.target_url.clone(),
            inspector_enabled: self.inspector_enabled,
            running: self.running,
            interval_ms: self.interval_ms.max(500),
            selected: self.selected.clone(),
            snapshot: self.snapshot.clone(),
            logs: self.logs.iter().cloned().collect(),
        }
    }

    fn push_log(&mut self, level: impl Into<String>, message: impl Into<String>) {
        self.next_log_id += 1;
        self.logs.push_back(LogEntry {
            id: self.next_log_id,
            ts: now_ms(),
            level: level.into(),
            message: message.into(),
        });
        while self.logs.len() > MAX_LOGS {
            self.logs.pop_front();
        }
    }
}

struct BridgeChunkBuffer {
    total: usize,
    parts: Vec<Option<String>>,
    created_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SelectedElement {
    tag: String,
    selector: String,
    text: String,
    role: Option<String>,
    name: String,
    fingerprint: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ElementPreview {
    rect: Rect,
    label: String,
    selector: String,
    selected: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageSnapshot {
    url: String,
    title: String,
    image: Option<String>,
    width: f64,
    height: f64,
    scroll_x: f64,
    scroll_y: f64,
    selected_rect: Option<Rect>,
    candidates: Vec<ElementPreview>,
    captured_at: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LogEntry {
    id: u64,
    ts: u64,
    level: String,
    message: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClientState {
    target_url: Option<String>,
    inspector_enabled: bool,
    running: bool,
    interval_ms: u64,
    selected: Option<SelectedElement>,
    snapshot: Option<PageSnapshot>,
    logs: Vec<LogEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeEvent {
    #[serde(rename = "type")]
    event_type: String,
    level: Option<String>,
    message: Option<String>,
    selected: Option<SelectedElement>,
    snapshot: Option<PageSnapshot>,
}

#[tauri::command]
fn get_state(state: State<'_, SharedState>) -> ClientState {
    state.client_state()
}

#[tauri::command]
fn open_target(
    app: AppHandle,
    state: State<'_, SharedState>,
    url: String,
) -> Result<ClientState, String> {
    open_target_url(&app, &state, Some(&url))?;
    Ok(state.client_state())
}

#[tauri::command]
fn set_inspector(
    app: AppHandle,
    state: State<'_, SharedState>,
    enabled: bool,
) -> Result<ClientState, String> {
    if ensure_target(&app).is_err() {
        open_target_url(&app, &state, None)?;
    }
    state.with_runtime(|runtime| {
        runtime.inspector_enabled = enabled;
        runtime.running = false;
        runtime.push_log(
            "info",
            if enabled {
                "인스펙터 시작: 대상 웹뷰에서 버튼 위로 마우스를 올리고 클릭하세요."
            } else {
                "인스펙터 정지"
            },
        );
    });
    inject_controller(&app)?;
    Ok(state.client_state())
}

#[tauri::command]
fn start_automation(
    app: AppHandle,
    state: State<'_, SharedState>,
    interval_ms: u64,
) -> Result<ClientState, String> {
    ensure_target(&app)?;
    let interval_ms = interval_ms.max(500);
    state.with_runtime(|runtime| {
        if runtime.selected.is_none() {
            return;
        }
        runtime.interval_ms = interval_ms;
        runtime.running = true;
        runtime.inspector_enabled = false;
        runtime.push_log("success", format!("자동 클릭 시작: {interval_ms}ms 주기"));
    });

    if state.client_state().selected.is_none() {
        return Err("먼저 인스펙터로 버튼을 선택하세요.".into());
    }

    inject_controller(&app)?;
    Ok(state.client_state())
}

#[tauri::command]
fn stop_automation(app: AppHandle, state: State<'_, SharedState>) -> Result<ClientState, String> {
    state.with_runtime(|runtime| {
        runtime.running = false;
        runtime.inspector_enabled = false;
        runtime.push_log("info", "자동화 정지");
    });
    inject_controller(&app)?;
    Ok(state.client_state())
}

#[tauri::command]
fn set_interval(
    app: AppHandle,
    state: State<'_, SharedState>,
    interval_ms: u64,
) -> Result<ClientState, String> {
    let interval_ms = interval_ms.max(500);
    state.with_runtime(|runtime| {
        runtime.interval_ms = interval_ms;
        runtime.push_log("info", format!("클릭 주기 변경: {interval_ms}ms"));
    });
    inject_controller(&app)?;
    Ok(state.client_state())
}

#[tauri::command]
fn click_once(app: AppHandle, state: State<'_, SharedState>) -> Result<ClientState, String> {
    ensure_target(&app)?;
    if state.client_state().selected.is_none() {
        return Err("먼저 인스펙터로 버튼을 선택하세요.".into());
    }

    inject_controller(&app)?;
    let Some(window) = app.get_webview_window(TARGET_LABEL) else {
        return Err("대상 웹뷰를 찾을 수 없습니다.".into());
    };
    window
        .eval("window.__buttonAutomation && window.__buttonAutomation.clickOnce();")
        .map_err(|error| error.to_string())?;

    state.with_runtime(|runtime| runtime.push_log("info", "1회 클릭 요청"));
    Ok(state.client_state())
}

#[tauri::command]
fn clear_logs(state: State<'_, SharedState>) -> ClientState {
    state.with_runtime(|runtime| runtime.logs.clear());
    state.client_state()
}

fn main() {
    let shared_state = SharedState::new();

    tauri::Builder::default()
        .setup({
            let shared_state = shared_state.clone();
            move |app| {
                let port = start_bridge(shared_state.clone())?;
                app.manage(shared_state.clone());
                shared_state.with_runtime(|runtime| {
                    runtime.bridge_port = port;
                    runtime.interval_ms = 5_000;
                    runtime.push_log("info", format!("로컬 브리지 시작: 127.0.0.1:{port}"));
                });
                open_target_url(app.handle(), &shared_state, None)?;
                Ok(())
            }
        })
        .on_page_load(|webview, _payload| {
            if webview.label() != TARGET_LABEL {
                return;
            }

            let app = webview.app_handle().clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(450));
                let _ = inject_controller(&app);
            });
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            open_target,
            set_inspector,
            start_automation,
            stop_automation,
            set_interval,
            click_once,
            clear_logs
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn ensure_target(app: &AppHandle) -> Result<(), String> {
    app.get_webview_window(TARGET_LABEL)
        .map(|_| ())
        .ok_or_else(|| "먼저 대상 URL을 열어주세요.".into())
}

fn normalize_url(input: Option<&str>) -> Result<String, String> {
    let input = input.unwrap_or(DEFAULT_TARGET_URL);
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(DEFAULT_TARGET_URL.into());
    }

    let candidate = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };

    Url::parse(&candidate)
        .map(|url| url.to_string())
        .map_err(|error| format!("URL 형식이 올바르지 않습니다: {error}"))
}

fn open_target_url(app: &AppHandle, state: &SharedState, url: Option<&str>) -> Result<(), String> {
    let normalized_url = normalize_url(url)?;

    state.with_runtime(|runtime| {
        runtime.target_url = Some(normalized_url.clone());
        runtime.running = false;
        runtime.inspector_enabled = false;
        runtime.selected = None;
        runtime.snapshot = None;
        runtime.push_log("info", format!("대상 웹뷰 열기: {normalized_url}"));
    });

    if let Some(window) = app.get_webview_window(TARGET_LABEL) {
        let script = format!(
            "window.location.href = {};",
            serde_json::to_string(&normalized_url).map_err(|error| error.to_string())?
        );
        window.eval(&script).map_err(|error| error.to_string())?;
        window.set_focus().map_err(|error| error.to_string())?;
    } else {
        let parsed = Url::parse(&normalized_url).map_err(|error| error.to_string())?;
        let nav_state = state.clone();
        WebviewWindowBuilder::new(app, TARGET_LABEL, WebviewUrl::External(parsed))
            .title("Automation Target")
            .inner_size(1200.0, 820.0)
            .min_inner_size(640.0, 480.0)
            .on_navigation(move |url| !handle_navigation_bridge(&nav_state, url))
            .build()
            .map_err(|error| error.to_string())?;
    }

    inject_controller(app)
}

fn inject_controller(app: &AppHandle) -> Result<(), String> {
    let Some(window) = app.get_webview_window(TARGET_LABEL) else {
        return Ok(());
    };

    let state = app.state::<SharedState>();
    let runtime = state.client_state();
    let bridge_port = state.with_runtime(|runtime| runtime.bridge_port);
    if bridge_port == 0 {
        return Ok(());
    }

    let config = json!({
        "endpoint": format!("http://127.0.0.1:{bridge_port}"),
        "bridgeScheme": BRIDGE_SCHEME,
        "inspector": runtime.inspector_enabled,
        "running": runtime.running,
        "intervalMs": runtime.interval_ms,
        "selected": runtime.selected,
    });

    let script = build_injection_script(&config)?;
    window.eval(&script).map_err(|error| error.to_string())
}

fn build_injection_script(config: &serde_json::Value) -> Result<String, String> {
    let config = serde_json::to_string(config).map_err(|error| error.to_string())?;
    let runtime_script = format!(
        r##"
(() => {{
  const CONFIG = {config};

  if (window.__buttonAutomation && window.__buttonAutomation.version === 1) {{
    window.__buttonAutomation.configure(CONFIG);
    return;
  }}

  const runtime = {{
    version: 1,
    endpoint: CONFIG.endpoint,
    inspector: false,
    running: false,
    selected: null,
    intervalMs: 5000,
    hoverTarget: null,
    timer: null,
    snapshotTimer: null,
    snapshotBusy: false,
    html2canvasLoaded: false,
    html2canvasFailed: false,
    lastSnapshotError: "",
    bridgeQueue: Promise.resolve(),
  }};

  const overlayId = "__buttonAutomationOverlay";
  const selectedOverlayId = "__buttonAutomationSelectedOverlay";

  function base64UrlEncode(value) {{
    const utf8 = encodeURIComponent(value).replace(/%([0-9A-F]{{2}})/g, (_match, code) =>
      String.fromCharCode(Number.parseInt(code, 16))
    );
    return btoa(utf8).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/g, "");
  }}

  function navigateBridge(url) {{
    return new Promise((resolve) => {{
      window.location.href = url;
      window.setTimeout(resolve, 12);
    }});
  }}

  async function sendBridgePayload(payload) {{
    const data = base64UrlEncode(JSON.stringify(payload));
    const maxChunkSize = 1800;
    if (data.length <= maxChunkSize) {{
      await navigateBridge(`${{CONFIG.bridgeScheme}}://event?data=${{data}}`);
      return;
    }}

    const id = `${{Date.now().toString(36)}}-${{Math.random().toString(36).slice(2)}}`;
    const total = Math.ceil(data.length / maxChunkSize);
    for (let index = 0; index < total; index += 1) {{
      const chunk = data.slice(index * maxChunkSize, (index + 1) * maxChunkSize);
      await navigateBridge(
        `${{CONFIG.bridgeScheme}}://chunk?id=${{id}}&index=${{index}}&total=${{total}}&data=${{chunk}}`
      );
    }}
  }}

  function post(payload) {{
    payload.sentAt = Date.now();
    runtime.bridgeQueue = runtime.bridgeQueue
      .then(() => sendBridgePayload(payload))
      .catch(() => undefined);
  }}

  function log(level, message) {{
    post({{ type: "log", level, message }});
  }}

  function compactText(value) {{
    return String(value || "").replace(/\s+/g, " ").trim().slice(0, 140);
  }}

  function cssEscape(value) {{
    if (window.CSS && typeof window.CSS.escape === "function") {{
      return window.CSS.escape(value);
    }}
    return String(value).replace(/[^a-zA-Z0-9_-]/g, "\\$&");
  }}

  function labelFor(element) {{
    if (!element) return "";
    const inputValue = element instanceof HTMLInputElement ? element.value : "";
    return compactText(
      element.getAttribute("aria-label") ||
        element.getAttribute("title") ||
        inputValue ||
        element.innerText ||
        element.textContent ||
        element.tagName.toLowerCase()
    );
  }}

  function selectorFor(element) {{
    if (!element || element.nodeType !== Node.ELEMENT_NODE) return "";
    const parts = [];
    let node = element;
    while (node && node.nodeType === Node.ELEMENT_NODE && node !== document.documentElement) {{
      const tag = node.tagName.toLowerCase();
      const id = node.getAttribute("id");
      if (id) {{
        parts.unshift(`${{tag}}#${{cssEscape(id)}}`);
        break;
      }}

      let part = tag;
      const testAttr = node.hasAttribute("data-testid") ? "data-testid" : "data-test";
      const testId = node.getAttribute(testAttr);
      if (testId) {{
        part += `[${{testAttr}}="${{String(testId).replace(/"/g, '\\"')}}"]`;
        parts.unshift(part);
        break;
      }}

      const classNames = Array.from(node.classList || []).filter(Boolean).slice(0, 2);
      if (classNames.length) {{
        part += "." + classNames.map(cssEscape).join(".");
      }}

      const parent = node.parentElement;
      if (parent) {{
        const siblings = Array.from(parent.children).filter(
          (child) => child.tagName === node.tagName
        );
        if (siblings.length > 1) {{
          part += `:nth-of-type(${{siblings.indexOf(node) + 1}})`;
        }}
      }}

      parts.unshift(part);
      node = parent;
    }}
    return parts.join(" > ");
  }}

  function isVisible(element) {{
    if (!element || !(element instanceof Element)) return false;
    const rect = element.getBoundingClientRect();
    const style = window.getComputedStyle(element);
    return (
      rect.width > 0 &&
      rect.height > 0 &&
      style.display !== "none" &&
      style.visibility !== "hidden" &&
      Number(style.opacity || "1") > 0.02
    );
  }}

  function isEnabled(element) {{
    if (!element || !(element instanceof Element)) return false;
    if ("disabled" in element && element.disabled) return false;
    if (element.getAttribute("aria-disabled") === "true") return false;
    const style = window.getComputedStyle(element);
    if (style.pointerEvents === "none") return false;
    return true;
  }}

  function isClickable(element) {{
    if (!element || !(element instanceof Element)) return false;
    if (element.id === overlayId || element.id === selectedOverlayId) return false;
    const tag = element.tagName.toLowerCase();
    const role = element.getAttribute("role");
    const type = (element.getAttribute("type") || "").toLowerCase();
    const style = window.getComputedStyle(element);
    return (
      tag === "button" ||
      tag === "summary" ||
      tag === "a" ||
      role === "button" ||
      element.hasAttribute("onclick") ||
      element.hasAttribute("data-testid") ||
      (tag === "input" && ["button", "submit", "reset"].includes(type)) ||
      style.cursor === "pointer"
    );
  }}

  function nearestClickable(element) {{
    let node = element;
    let depth = 0;
    while (node && node !== document.body && depth < 8) {{
      if (isClickable(node) && isVisible(node)) return node;
      node = node.parentElement;
      depth += 1;
    }}
    return null;
  }}

  function serializeElement(element) {{
    const tag = element.tagName.toLowerCase();
    const role = element.getAttribute("role");
    const text = labelFor(element);
    const selector = selectorFor(element);
    return {{
      tag,
      selector,
      text,
      role,
      name: text || selector || tag,
      fingerprint: [tag, role || "", text].join("|"),
    }};
  }}

  function sameByFingerprint(element, selected) {{
    if (!selected) return false;
    const tag = element.tagName.toLowerCase();
    const role = element.getAttribute("role");
    const text = labelFor(element);
    if (selected.tag && selected.tag !== tag) return false;
    if (selected.role && selected.role !== role) return false;
    if (selected.text && selected.text === text) return true;
    return selected.fingerprint === [tag, role || "", text].join("|");
  }}

  function findTarget() {{
    const selected = runtime.selected;
    if (!selected) return null;
    if (selected.selector) {{
      try {{
        const direct = document.querySelector(selected.selector);
        if (direct && isVisible(direct)) return direct;
      }} catch (_error) {{
        // Ignore broken selectors from dynamic pages and fall through to fingerprint matching.
      }}
    }}

    const candidates = clickableCandidates(250);
    return candidates.find((element) => sameByFingerprint(element, selected)) || null;
  }}

  function ensureOverlay(id, selected) {{
    let overlay = document.getElementById(id);
    if (!overlay) {{
      overlay = document.createElement("div");
      overlay.id = id;
      overlay.style.position = "fixed";
      overlay.style.zIndex = "2147483647";
      overlay.style.pointerEvents = "none";
      overlay.style.border = selected ? "2px solid #d25d2f" : "2px solid #1e6f5c";
      overlay.style.background = selected ? "rgba(210, 93, 47, 0.18)" : "rgba(30, 111, 92, 0.18)";
      overlay.style.boxShadow = selected
        ? "0 0 0 9999px rgba(210, 93, 47, 0.05)"
        : "0 0 0 9999px rgba(30, 111, 92, 0.05)";
      overlay.style.borderRadius = "6px";
      overlay.style.display = "none";
      document.documentElement.appendChild(overlay);
    }}
    return overlay;
  }}

  function drawOverlay(element, id, selected) {{
    const overlay = ensureOverlay(id, selected);
    if (!element || !isVisible(element)) {{
      overlay.style.display = "none";
      return;
    }}
    const rect = element.getBoundingClientRect();
    overlay.style.display = "block";
    overlay.style.left = `${{Math.max(rect.left, 0)}}px`;
    overlay.style.top = `${{Math.max(rect.top, 0)}}px`;
    overlay.style.width = `${{Math.max(rect.width, 1)}}px`;
    overlay.style.height = `${{Math.max(rect.height, 1)}}px`;
  }}

  function clearHoverOverlay() {{
    const overlay = document.getElementById(overlayId);
    if (overlay) overlay.style.display = "none";
  }}

  function handleMouseMove(event) {{
    if (!runtime.inspector) return;
    const element = nearestClickable(document.elementFromPoint(event.clientX, event.clientY));
    runtime.hoverTarget = element;
    drawOverlay(element, overlayId, false);
  }}

  function handleClick(event) {{
    if (!runtime.inspector || !runtime.hoverTarget) return;
    event.preventDefault();
    event.stopPropagation();
    event.stopImmediatePropagation();

    const selected = serializeElement(runtime.hoverTarget);
    runtime.selected = selected;
    runtime.inspector = false;
    clearHoverOverlay();
    drawOverlay(runtime.hoverTarget, selectedOverlayId, true);
    post({{ type: "selected", selected }});
    log("success", `선택했다: ${{selected.name}}`);
  }}

  function clickableCandidates(limit) {{
    const selector = [
      "button",
      "summary",
      "a[href]",
      "[role='button']",
      "[onclick]",
      "[data-testid]",
      "[data-test]",
      "input[type='button']",
      "input[type='submit']",
      "input[type='reset']",
    ].join(",");
    const elements = Array.from(document.querySelectorAll(selector)).slice(0, limit);
    const known = new Set(elements);
    const pointerElements = [];
    for (const element of document.querySelectorAll("body *")) {{
      if (pointerElements.length >= limit) break;
      if (known.has(element)) continue;
      if (!isVisible(element)) continue;
      if (window.getComputedStyle(element).cursor === "pointer") pointerElements.push(element);
    }}
    return elements
      .concat(pointerElements)
      .filter((element) => isClickable(element) && isVisible(element))
      .slice(0, limit);
  }}

  function rectFor(element) {{
    const rect = element.getBoundingClientRect();
    return {{
      x: Math.max(0, rect.left),
      y: Math.max(0, rect.top),
      width: Math.max(0, Math.min(rect.width, window.innerWidth - rect.left)),
      height: Math.max(0, Math.min(rect.height, window.innerHeight - rect.top)),
    }};
  }}

  function candidatePreview(element) {{
    const selected = runtime.selected ? sameByFingerprint(element, runtime.selected) : false;
    return {{
      rect: rectFor(element),
      label: labelFor(element) || element.tagName.toLowerCase(),
      selector: selectorFor(element),
      selected,
    }};
  }}

  async function ensureHtml2Canvas() {{
    if (window.html2canvas) return true;
    runtime.html2canvasFailed = true;
    return false;
  }}

  async function takeSnapshot() {{
    if (runtime.snapshotBusy || !document.body) return;
    runtime.snapshotBusy = true;

    let image = null;
    const target = findTarget();
    const selectedRect = target && isVisible(target) ? rectFor(target) : null;
    const candidates = clickableCandidates(120)
      .map(candidatePreview)
      .filter((candidate) => candidate.rect.width > 0 && candidate.rect.height > 0);

    try {{
      if (await ensureHtml2Canvas()) {{
        const canvas = await window.html2canvas(document.body, {{
          logging: false,
          useCORS: true,
          allowTaint: false,
          imageTimeout: 900,
          backgroundColor: window.getComputedStyle(document.body).backgroundColor || "#ffffff",
          scale: 0.45,
          width: window.innerWidth,
          height: window.innerHeight,
          x: window.scrollX,
          y: window.scrollY,
          windowWidth: window.innerWidth,
          windowHeight: window.innerHeight,
          ignoreElements: (element) => element.id === overlayId || element.id === selectedOverlayId,
        }});
        image = canvas.toDataURL("image/jpeg", 0.42);
        if (image.length > 42000) image = null;
      }}
    }} catch (error) {{
      const message = String(error && error.message ? error.message : error);
      if (runtime.lastSnapshotError !== message) {{
        runtime.lastSnapshotError = message;
        log("warn", `화면 캡처 대신 DOM 미러를 사용한다: ${{message}}`);
      }}
    }}

    post({{
      type: "snapshot",
      snapshot: {{
        url: window.location.href,
        title: document.title,
        image,
        width: window.innerWidth,
        height: window.innerHeight,
        scrollX: window.scrollX,
        scrollY: window.scrollY,
        selectedRect,
        candidates: candidates.slice(0, 48),
        capturedAt: Date.now(),
      }},
    }});
    runtime.snapshotBusy = false;
  }}

  function clickElement(element) {{
    if (!element) return;
    element.scrollIntoView({{ block: "center", inline: "center", behavior: "smooth" }});
    window.setTimeout(() => {{
      drawOverlay(element, selectedOverlayId, true);
      element.click();
      log("success", `눌렀다: ${{labelFor(element) || runtime.selected?.name || "button"}}`);
      takeSnapshot();
    }}, 80);
  }}

  function clickOnce() {{
    if (!runtime.selected) {{
      log("warn", "선택된 버튼이 없다.");
      return;
    }}
    log("info", `찾는 중: ${{runtime.selected.name}}`);
    const target = findTarget();
    if (!target) {{
      log("warn", `못 찾았다: ${{runtime.selected.name}}`);
      return;
    }}
    drawOverlay(target, selectedOverlayId, true);
    log("success", `찾았다: ${{labelFor(target) || runtime.selected.name}}`);
    if (!isEnabled(target)) {{
      log("warn", `찾았지만 아직 비활성화 상태다: ${{labelFor(target) || runtime.selected.name}}`);
      return;
    }}
    clickElement(target);
  }}

  function scheduleAutomation() {{
    if (runtime.timer) {{
      window.clearInterval(runtime.timer);
      runtime.timer = null;
    }}
    if (!runtime.running || !runtime.selected) return;
    runtime.timer = window.setInterval(clickOnce, Math.max(runtime.intervalMs, 500));
    clickOnce();
  }}

  function scheduleSnapshot() {{
    if (runtime.snapshotTimer) {{
      window.clearInterval(runtime.snapshotTimer);
    }}
    runtime.snapshotTimer = window.setInterval(takeSnapshot, 1600);
    takeSnapshot();
  }}

  function configure(config) {{
    runtime.endpoint = config.endpoint;
    runtime.inspector = Boolean(config.inspector);
    runtime.running = Boolean(config.running);
    runtime.intervalMs = Number(config.intervalMs || 5000);
    runtime.selected = config.selected || runtime.selected || null;

    if (!runtime.inspector) {{
      clearHoverOverlay();
    }}

    if (runtime.selected) {{
      drawOverlay(findTarget(), selectedOverlayId, true);
    }}

    scheduleAutomation();
    scheduleSnapshot();
  }}

  document.addEventListener("mousemove", handleMouseMove, true);
  document.addEventListener("click", handleClick, true);
  window.addEventListener("scroll", () => {{
    if (runtime.selected) drawOverlay(findTarget(), selectedOverlayId, true);
  }}, true);
  window.addEventListener("resize", takeSnapshot);

  runtime.configure = configure;
  runtime.clickOnce = clickOnce;
  window.__buttonAutomation = runtime;

  configure(CONFIG);
  log("info", "대상 웹뷰 런타임 연결됨");
}})();
"##
    );

    Ok(format!("{HTML2CANVAS_SOURCE}\n;{runtime_script}"))
}

fn start_bridge(state: SharedState) -> Result<u16, Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();

    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => handle_bridge_stream(stream, &state),
                Err(error) => state.with_runtime(|runtime| {
                    runtime.push_log("error", format!("브리지 연결 오류: {error}"))
                }),
            }
        }
    });

    Ok(port)
}

fn handle_bridge_stream(mut stream: TcpStream, state: &SharedState) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));

    let Ok((method, path, body)) = read_http_request(&mut stream) else {
        let _ = write_response(&mut stream, 400, "{}");
        return;
    };

    if method == "OPTIONS" {
        let _ = write_response(&mut stream, 204, "");
        return;
    }

    if method != "POST" || path != "/event" {
        let _ = write_response(&mut stream, 404, "{}");
        return;
    }

    match serde_json::from_slice::<BridgeEvent>(&body) {
        Ok(event) => {
            apply_bridge_event(state, event);
            let _ = write_response(&mut stream, 200, r#"{"ok":true}"#);
        }
        Err(error) => {
            state.with_runtime(|runtime| {
                runtime.push_log("error", format!("브리지 이벤트 파싱 실패: {error}"))
            });
            let _ = write_response(&mut stream, 400, "{}");
        }
    }
}

fn handle_navigation_bridge(state: &SharedState, url: &Url) -> bool {
    if url.scheme() != BRIDGE_SCHEME {
        return false;
    }

    let Some(host) = url.host_str() else {
        return true;
    };

    let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
    match host {
        "event" => {
            if let Some(data) = query.get("data") {
                apply_encoded_bridge_event(state, data);
            }
        }
        "chunk" => {
            let id = query.get("id").cloned().unwrap_or_default();
            let index = query
                .get("index")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(usize::MAX);
            let total = query
                .get("total")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            let data = query.get("data").cloned().unwrap_or_default();

            if !id.is_empty() && total > 0 && total <= 256 && index < total && !data.is_empty() {
                let assembled = state.with_runtime(|runtime| {
                    let now = now_ms();
                    runtime
                        .bridge_chunks
                        .retain(|_, buffer| now.saturating_sub(buffer.created_at) < 15_000);
                    if runtime
                        .bridge_chunks
                        .get(&id)
                        .is_some_and(|buffer| buffer.total != total)
                    {
                        runtime.bridge_chunks.remove(&id);
                    }

                    let buffer = runtime.bridge_chunks.entry(id.clone()).or_insert_with(|| {
                        BridgeChunkBuffer {
                            total,
                            parts: vec![None; total],
                            created_at: now,
                        }
                    });

                    buffer.parts[index] = Some(data);
                    if buffer.parts.iter().all(Option::is_some) {
                        let payload = buffer
                            .parts
                            .iter()
                            .map(|part| part.as_ref().expect("all parts are present").clone())
                            .collect::<String>();
                        runtime.bridge_chunks.remove(&id);
                        Some(payload)
                    } else {
                        None
                    }
                });

                if let Some(data) = assembled {
                    apply_encoded_bridge_event(state, &data);
                }
            }
        }
        _ => {
            state.with_runtime(|runtime| {
                runtime.push_log(
                    "warn",
                    format!("알 수 없는 네비게이션 브리지 이벤트: {host}"),
                )
            });
        }
    }

    true
}

fn apply_encoded_bridge_event(state: &SharedState, data: &str) {
    let decoded = match URL_SAFE_NO_PAD.decode(data.as_bytes()) {
        Ok(decoded) => decoded,
        Err(error) => {
            state.with_runtime(|runtime| {
                runtime.push_log("error", format!("브리지 payload 디코딩 실패: {error}"))
            });
            return;
        }
    };

    match serde_json::from_slice::<BridgeEvent>(&decoded) {
        Ok(event) => apply_bridge_event(state, event),
        Err(error) => state.with_runtime(|runtime| {
            runtime.push_log("error", format!("브리지 이벤트 파싱 실패: {error}"))
        }),
    }
}

fn read_http_request(stream: &mut TcpStream) -> Result<(String, String, Vec<u8>), String> {
    let mut data = Vec::new();
    let mut buffer = [0_u8; 8192];
    let header_end = loop {
        let read = stream
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("connection closed".into());
        }
        data.extend_from_slice(&buffer[..read]);
        if let Some(index) = find_header_end(&data) {
            break index;
        }
        if data.len() > 64 * 1024 {
            return Err("request headers are too large".into());
        }
    };

    let headers = String::from_utf8_lossy(&data[..header_end]);
    let mut lines = headers.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| "missing request line".to_string())?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| "missing method".to_string())?
        .to_string();
    let path = request_parts
        .next()
        .ok_or_else(|| "missing path".to_string())?
        .to_string();

    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);

    if content_length > 12 * 1024 * 1024 {
        return Err("request body is too large".into());
    }

    let body_start = header_end + 4;
    let mut body = data[body_start..].to_vec();
    while body.len() < content_length {
        let read = stream
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&buffer[..read]);
    }
    body.truncate(content_length);

    Ok((method, path, body))
}

fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|window| window == b"\r\n\r\n")
}

fn write_response(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "OK",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: POST, OPTIONS\r\n\
         Access-Control-Allow-Headers: content-type\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes())
}

fn apply_bridge_event(state: &SharedState, event: BridgeEvent) {
    state.with_runtime(|runtime| match event.event_type.as_str() {
        "log" => {
            let level = event.level.unwrap_or_else(|| "info".into());
            let message = event.message.unwrap_or_else(|| "이벤트".into());
            runtime.push_log(level, message);
        }
        "selected" => {
            if let Some(selected) = event.selected {
                runtime.selected = Some(selected.clone());
                runtime.inspector_enabled = false;
                runtime.running = false;
                runtime.push_log("success", format!("선택 완료: {}", selected.name));
            }
        }
        "snapshot" => {
            if let Some(snapshot) = event.snapshot {
                runtime.snapshot = Some(snapshot);
            }
        }
        _ => runtime.push_log(
            "warn",
            format!("알 수 없는 브리지 이벤트: {}", event.event_type),
        ),
    });
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
