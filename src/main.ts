import { invoke } from "@tauri-apps/api/core";
import "./styles.css";

const DEFAULT_TARGET_URL = "https://www.google.com";

type Level = "info" | "success" | "warn" | "error";

type LogEntry = {
  id: number;
  ts: number;
  level: Level;
  message: string;
};

type Rect = {
  x: number;
  y: number;
  width: number;
  height: number;
};

type SelectedElement = {
  tag: string;
  selector: string;
  text: string;
  role?: string | null;
  name: string;
  fingerprint: string;
};

type ElementPreview = {
  rect: Rect;
  label: string;
  selector: string;
  selected: boolean;
};

type PageSnapshot = {
  url: string;
  title: string;
  image?: string | null;
  width: number;
  height: number;
  scrollX: number;
  scrollY: number;
  selectedRect?: Rect | null;
  candidates: ElementPreview[];
  capturedAt: number;
};

type ClientState = {
  targetUrl?: string | null;
  inspectorEnabled: boolean;
  running: boolean;
  intervalMs: number;
  selected?: SelectedElement | null;
  logs: LogEntry[];
  snapshot?: PageSnapshot | null;
};

const els = {
  form: document.querySelector<HTMLFormElement>("#target-form")!,
  targetUrl: document.querySelector<HTMLInputElement>("#target-url")!,
  targetStatus: document.querySelector("#target-status")!,
  runState: document.querySelector("#run-state")!,
  inspectBtn: document.querySelector<HTMLButtonElement>("#inspect-btn")!,
  startBtn: document.querySelector<HTMLButtonElement>("#start-btn")!,
  stopBtn: document.querySelector<HTMLButtonElement>("#stop-btn")!,
  clickOnceBtn: document.querySelector<HTMLButtonElement>("#click-once-btn")!,
  intervalMs: document.querySelector<HTMLInputElement>("#interval-ms")!,
  selectedName: document.querySelector("#selected-name")!,
  selectedSelector: document.querySelector("#selected-selector")!,
  preview: document.querySelector("#preview")!,
  snapshotMeta: document.querySelector("#snapshot-meta")!,
  refreshBtn: document.querySelector<HTMLButtonElement>("#refresh-btn")!,
  logs: document.querySelector<HTMLOListElement>("#logs")!,
  logCount: document.querySelector("#log-count")!,
  clearLogsBtn: document.querySelector<HTMLButtonElement>("#clear-logs-btn")!,
};

let lastLogId = 0;
let latestState: ClientState | null = null;

function formatTime(ms: number): string {
  return new Intl.DateTimeFormat("ko-KR", {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  }).format(new Date(ms));
}

function setBusy(button: HTMLButtonElement, busy: boolean): void {
  button.disabled = busy;
  button.dataset.busy = busy ? "true" : "false";
}

async function command<T>(name: string, args: Record<string, unknown> = {}): Promise<T | null> {
  try {
    return await invoke<T>(name, args);
  } catch (error) {
    renderTransientError(error instanceof Error ? error.message : String(error));
    await refreshState();
    return null;
  }
}

function renderTransientError(message: string): void {
  const item = document.createElement("li");
  item.className = "log error";
  item.innerHTML = `<time>${formatTime(Date.now())}</time><span>${escapeHtml(message)}</span>`;
  els.logs.prepend(item);
}

function escapeHtml(value: string): string {
  return value.replace(/[&<>"']/g, (char) => {
    const map: Record<string, string> = {
      "&": "&amp;",
      "<": "&lt;",
      ">": "&gt;",
      '"': "&quot;",
      "'": "&#039;",
    };
    return map[char];
  });
}

function renderState(state: ClientState): void {
  latestState = state;

  if (state.targetUrl) {
    els.targetStatus.textContent = state.targetUrl;
    els.targetUrl.value = state.targetUrl;
  } else {
    els.targetStatus.textContent = "대상 웹뷰를 준비 중입니다.";
    if (els.targetUrl.value.trim().length === 0) {
      els.targetUrl.value = DEFAULT_TARGET_URL;
    }
  }

  els.runState.textContent = state.running
    ? "자동 클릭 중"
    : state.inspectorEnabled
      ? "검사 중"
      : "대기";
  els.runState.className = `run-state ${state.running ? "active" : state.inspectorEnabled ? "inspect" : ""}`;

  els.inspectBtn.classList.toggle("active", state.inspectorEnabled);
  els.inspectBtn.textContent = state.inspectorEnabled ? "검사 종료" : "인스펙터";
  els.startBtn.disabled = !state.selected;
  els.clickOnceBtn.disabled = !state.selected;
  els.stopBtn.disabled = !state.running && !state.inspectorEnabled;
  els.intervalMs.value = String(state.intervalMs);

  if (state.selected) {
    els.selectedName.textContent = state.selected.name;
    els.selectedSelector.textContent = state.selected.selector;
  } else {
    els.selectedName.textContent = "없음";
    els.selectedSelector.textContent = "인스펙터로 대상 버튼을 선택하세요.";
  }

  renderPreview(state.snapshot);
  renderLogs(state.logs);
}

function renderPreview(snapshot?: PageSnapshot | null): void {
  if (!snapshot) {
    els.snapshotMeta.textContent = "아직 동기화된 화면이 없습니다.";
    els.preview.innerHTML = `<div class="empty-preview">대상 웹뷰에서 화면을 가져오는 중입니다.</div>`;
    return;
  }

  const captured = formatTime(snapshot.capturedAt);
  els.snapshotMeta.textContent = `${snapshot.title || "제목 없음"} · ${Math.round(snapshot.width)}x${Math.round(snapshot.height)} · ${captured}`;

  const safeWidth = Math.max(snapshot.width, 1);
  const safeHeight = Math.max(snapshot.height, 1);
  const boxes = snapshot.candidates
    .slice(0, 120)
    .map((candidate) => {
      const rect = candidate.rect;
      const style = [
        `left:${(rect.x / safeWidth) * 100}%`,
        `top:${(rect.y / safeHeight) * 100}%`,
        `width:${(rect.width / safeWidth) * 100}%`,
        `height:${(rect.height / safeHeight) * 100}%`,
      ].join(";");
      const label = escapeHtml(candidate.label || "button");
      return `<div class="preview-box ${candidate.selected ? "selected" : ""}" style="${style}" title="${label}"><span>${label}</span></div>`;
    })
    .join("");

  const selected = snapshot.selectedRect
    ? `<div class="preview-selection" style="left:${(snapshot.selectedRect.x / safeWidth) * 100}%;top:${(snapshot.selectedRect.y / safeHeight) * 100}%;width:${(snapshot.selectedRect.width / safeWidth) * 100}%;height:${(snapshot.selectedRect.height / safeHeight) * 100}%"></div>`
    : "";

  const image = snapshot.image
    ? `<img src="${snapshot.image}" alt="대상 웹뷰 미러 화면" />`
    : `<div class="dom-fallback">캡처 이미지를 만들 수 없어 DOM 위치만 표시합니다.</div>`;

  els.preview.innerHTML = `
    <div class="preview-stage" style="aspect-ratio:${safeWidth} / ${safeHeight}">
      ${image}
      <div class="preview-overlay">${boxes}${selected}</div>
    </div>
  `;
}

function renderLogs(logs: LogEntry[]): void {
  els.logCount.textContent = `${logs.length}개 이벤트`;
  const maxId = logs.reduce((value, log) => Math.max(value, log.id), lastLogId);
  const shouldStickToBottom =
    els.logs.scrollTop + els.logs.clientHeight >= els.logs.scrollHeight - 32 || maxId !== lastLogId;
  lastLogId = maxId;

  els.logs.innerHTML = logs
    .slice()
    .reverse()
    .map(
      (log) => `
        <li class="log ${log.level}">
          <time>${formatTime(log.ts)}</time>
          <span>${escapeHtml(log.message)}</span>
        </li>
      `,
    )
    .join("");

  if (shouldStickToBottom) {
    els.logs.scrollTop = 0;
  }
}

async function refreshState(): Promise<void> {
  const state = await command<ClientState>("get_state");
  if (state) {
    renderState(state);
  }
}

els.form.addEventListener("submit", async (event) => {
  event.preventDefault();
  const url = els.targetUrl.value.trim() || DEFAULT_TARGET_URL;
  const button = els.form.querySelector<HTMLButtonElement>("button[type='submit']")!;
  setBusy(button, true);
  const state = await command<ClientState>("open_target", { url });
  setBusy(button, false);
  if (state) {
    renderState(state);
  }
});

els.inspectBtn.addEventListener("click", async () => {
  const enabled = !(latestState?.inspectorEnabled ?? false);
  const state = await command<ClientState>("set_inspector", { enabled });
  if (state) {
    renderState(state);
  }
});

els.startBtn.addEventListener("click", async () => {
  const intervalMs = Number(els.intervalMs.value);
  const state = await command<ClientState>("start_automation", { intervalMs });
  if (state) {
    renderState(state);
  }
});

els.stopBtn.addEventListener("click", async () => {
  const state = await command<ClientState>("stop_automation");
  if (state) {
    renderState(state);
  }
});

els.clickOnceBtn.addEventListener("click", async () => {
  const state = await command<ClientState>("click_once");
  if (state) {
    renderState(state);
  }
});

els.intervalMs.addEventListener("change", async () => {
  const intervalMs = Number(els.intervalMs.value);
  const state = await command<ClientState>("set_interval", { intervalMs });
  if (state) {
    renderState(state);
  }
});

els.refreshBtn.addEventListener("click", refreshState);

els.clearLogsBtn.addEventListener("click", async () => {
  const state = await command<ClientState>("clear_logs");
  if (state) {
    renderState(state);
  }
});

void refreshState();
window.setInterval(refreshState, 900);
