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

function fmtDur(s) {
  if (!s) return "";
  const m = Math.floor(s / 60);
  return `${m}:${String(Math.floor(s % 60)).padStart(2, "0")}`;
}

function nameOf(url) {
  try {
    const p = new URL(url).pathname.split("/").pop();
    return decodeURIComponent(p) || new URL(url).hostname;
  } catch {
    return url;
  }
}

function button(text, onClick) {
  const b = document.createElement("button");
  b.textContent = text;
  b.onclick = async () => {
    b.disabled = true;
    const reply = await onClick();
    b.textContent = reply && (reply.type === "accepted" || reply.type === "ok") ? "Sent" : "Failed";
  };
  return b;
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

  const showButton = document.getElementById("showButton");
  showButton.checked = state.show_button;
  showButton.onchange = () => api.runtime.sendMessage({ type: "show-button", enabled: showButton.checked });

  const blocked = document.getElementById("blocked");
  const renderBlocked = (hosts) => {
    document.getElementById("blockedWrap").hidden = hosts.length === 0;
    blocked.replaceChildren();
    for (const h of hosts) {
      const chip = document.createElement("span");
      chip.className = "chip";
      chip.textContent = h;
      const x = document.createElement("button");
      x.textContent = "×";
      x.title = "Show the button on this site again";
      x.onclick = async () => {
        await api.runtime.sendMessage({ type: "unblock-host", host: h });
        renderBlocked(hosts.filter((v) => v !== h));
      };
      chip.append(x);
      blocked.append(chip);
    }
  };
  renderBlocked(state.blocked_hosts || []);

  const list = document.getElementById("media");
  document.getElementById("empty").hidden = state.media.length > 0;
  for (const m of state.media) {
    const li = document.createElement("li");
    const info = document.createElement("div");
    info.className = "info";
    const name = document.createElement("div");
    name.className = "name";
    name.textContent = tab?.title && m.kind === "stream" ? tab.title : nameOf(m.url);
    name.title = m.url;
    const meta = document.createElement("div");
    meta.className = "meta";
    const kind = m.kind === "subtitle" ? "Subtitles" : m.type;
    meta.textContent = [kind, fmtSize(m.size), fmtDur(m.duration)].filter(Boolean).join(" · ");
    info.append(name, meta);
    li.append(info);
    const send = (quality) => api.runtime.sendMessage({ type: "download-media", tabId: tab?.id, url: m.url, kind: m.kind, quality });
    if (m.kind === "stream" && m.qualities && m.qualities.length > 1) {
      const q = document.createElement("div");
      q.className = "qualities";
      for (const h of m.qualities) q.append(button(`${h}p`, () => send(h)));
      li.append(q);
    } else {
      li.append(button("Download", () => send()));
    }
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
