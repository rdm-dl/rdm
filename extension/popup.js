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

// Whole shows on sites where the extension plays the episodes itself.
function showSection(show, pageUrl) {
  if (!show || (!show.page && !show.running)) return;
  const box = document.getElementById("show");
  const text = document.getElementById("showText");
  const pick = document.getElementById("showSeason");
  const go = document.getElementById("showGo");
  box.hidden = false;
  if (show.running) {
    text.textContent = `Getting ${show.at + 1} of ${show.total}. Tabs will open and close; leave them be.`;
    go.textContent = "Stop";
    go.onclick = async () => {
      await api.runtime.sendMessage({ type: "show-stop" });
      window.close();
    };
    return;
  }
  if (show.total) {
    const missed = show.failed.length ? ` ${show.failed.length} couldn't be read: ${show.failed.slice(0, 3).join(", ")}${show.failed.length > 3 ? "…" : ""}` : "";
    text.textContent = `Sent ${show.saved} of ${show.total} to RDM.${missed}`;
  }
  go.onclick = async () => {
    go.disabled = true;
    go.textContent = "Reading the list…";
    const reply = await api.runtime.sendMessage({ type: "show-list", url: pageUrl });
    go.disabled = false;
    if (reply.error || !reply.list?.length) {
      text.textContent = reply.error || "Nothing to get on this page.";
      go.textContent = "Try again";
      return;
    }
    const seasons = [...new Set(reply.list.map((e) => e.folder))];
    pick.replaceChildren(new Option(`All ${reply.list.length}`, ""), ...seasons.map((s) => new Option(s.split("/").pop(), s)));
    pick.hidden = false;
    text.textContent = "Pick what to get. Tabs will open and close, one for each.";
    go.textContent = "Start";
    go.onclick = async () => {
      const list = reply.list.filter((e) => !pick.value || e.folder === pick.value);
      await api.runtime.sendMessage({ type: "show-start", list });
      window.close();
    };
  };
}

async function init() {
  // popup.html?tab=<id> shows another tab's media (used when the popup is opened as a page).
  const forced = +new URLSearchParams(location.search).get("tab");
  const [tab] = forced ? [await api.tabs.get(forced)] : await api.tabs.query({ active: true, currentWindow: true });
  const state = await api.runtime.sendMessage({ type: "state", tabId: tab?.id, pageUrl: tab?.url });
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

  showSection(state.show, tab?.url);
  const list = document.getElementById("media");
  document.getElementById("empty").hidden = state.media.length > 0;
  for (const m of state.media) {
    const li = document.createElement("li");
    if (m.kind === "yt-note") {
      li.className = "note";
      li.textContent = m.text;
      list.append(li);
      continue;
    }
    if (m.kind === "yt") {
      const info = document.createElement("div");
      info.className = "info";
      const name = document.createElement("div");
      name.className = "name";
      name.textContent = `${m.label} · ${m.title}`;
      const meta = document.createElement("div");
      meta.className = "meta";
      meta.textContent = [m.container, m.fps > 30 ? `${m.fps}fps` : "", fmtSize(m.size), fmtDur(m.duration)].filter(Boolean).join(" · ");
      info.append(name, meta);
      li.append(info, button("Download", () => api.runtime.sendMessage({ type: "download-media", tabId: tab?.id, kind: "yt", videoId: m.videoId, itagV: m.itagV, itagA: m.itagA, size: m.size, title: m.title, ext: m.ext })));
      list.append(li);
      continue;
    }
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
