const api = typeof browser !== "undefined" ? browser : chrome;

function fmtSize(n) {
  if (!n) return "";
  const u = ["B", "KB", "MB", "GB"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(i ? 1 : 0)} ${u[i]}`;
}

function nameOf(url) {
  try {
    const p = new URL(url).pathname.split("/").pop();
    return decodeURIComponent(p) || new URL(url).hostname;
  } catch {
    return url;
  }
}

async function init() {
  const [tab] = await api.tabs.query({ active: true, currentWindow: true });
  const state = await api.runtime.sendMessage({ type: "state", tabId: tab?.id });
  const status = document.getElementById("status");
  status.textContent = state.connected ? "Connected to RDM" : "RDM not reachable — open the app";
  status.className = "status " + (state.connected ? "ok" : "bad");

  const toggle = document.getElementById("enabled");
  toggle.checked = state.enabled;
  toggle.onchange = () => api.runtime.sendMessage({ type: "toggle", enabled: toggle.checked });

  const list = document.getElementById("media");
  document.getElementById("empty").hidden = state.media.length > 0;
  for (const m of state.media) {
    const li = document.createElement("li");
    const info = document.createElement("div");
    info.className = "info";
    const name = document.createElement("div");
    name.className = "name";
    name.textContent = tab?.title && m.type.includes("stream") ? tab.title : nameOf(m.url);
    name.title = m.url;
    const meta = document.createElement("div");
    meta.className = "meta";
    meta.textContent = [m.type, fmtSize(m.size)].filter(Boolean).join(" · ");
    info.append(name, meta);
    const btn = document.createElement("button");
    btn.textContent = "Download";
    btn.onclick = async () => {
      btn.disabled = true;
      btn.textContent = "Sent";
      const filename = m.type.includes("stream") && tab?.title ? tab.title.replace(/[\\/:*?"<>|]/g, "_") + ".mp4" : undefined;
      await api.runtime.sendMessage({ type: "download-media", url: m.url, referrer: tab?.url, filename });
    };
    li.append(info, btn);
    list.append(li);
  }

  const url = document.getElementById("url");
  document.getElementById("add").onclick = async () => {
    if (!url.value.trim()) return;
    await api.runtime.sendMessage({ type: "add-url", url: url.value.trim() });
    url.value = "";
  };
  document.getElementById("open").onclick = () => api.runtime.sendMessage({ type: "show" });
}

init();
