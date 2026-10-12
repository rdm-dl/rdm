// Runs in the page's own JavaScript context. Watches the page's fetch/XMLHttpRequest
// traffic for stream manifests and media responses (including ones a service worker
// serves, which the extension's webRequest listener can't attribute to the tab) and
// reports them to content.js with window.postMessage.

(() => {
  if (window.__rdmDocHook) return;
  window.__rdmDocHook = true;
  try {
    document.documentElement.dataset.rdmHook = "1";
  } catch {}
  const MAGIC = 0x52444d34; // "RDM4"
  const URL_RE = /\.(m3u8|mpd|mp4|m4v|m4a|webm|mkv|mp3|aac|flac|ogg|ogv|mov|flv|vtt|srt|ttml|dfxp)(\?|#|$)/i;
  const TYPE_RE = /^(video\/|audio\/|application\/(vnd\.apple\.mpegurl|x-mpegurl|dash\+xml|ttml\+xml)|text\/vtt)/i;
  const SEGMENT_RE = /\.(ts|m4s|m4f|cmfv|cmfa)(\?|#|$)/i;
  const seen = new Set();

  function report(url, type, size) {
    try {
      if (!url || seen.has(url) || !/^https?:/i.test(url)) return;
      if (/\.googlevideo\.com\//i.test(url)) return; // handled from the player data
      const t = (type || "").split(";")[0].trim();
      if (SEGMENT_RE.test(url) && !/mpegurl|dash/i.test(t)) return;
      if (!URL_RE.test(url) && !TYPE_RE.test(t)) return;
      seen.add(url);
      window.postMessage({ __rdm: MAGIC, url, type: t, size: size || 0 }, "*");
    } catch {}
  }

  function abs(u) {
    try {
      return new URL(u, location.href).href;
    } catch {
      return "";
    }
  }

  // ---- YouTube: the player's own data (formats, qualities, title).
  const YT = /(^|\.)(youtube\.com|youtube-nocookie\.com|youtube\.googleapis\.com)$/.test(location.hostname);
  const PLAYER_API = /\/youtubei\/v1\/player\b/;
  let lastYt = "";

  function ytReport(resp) {
    try {
      if (!resp || !resp.streamingData || !resp.videoDetails) return;
      const sd = resp.streamingData;
      const vd = resp.videoDetails;
      const fmt = (f) => ({
        itag: f.itag,
        url: f.url || "",
        cipher: !!(f.signatureCipher || f.cipher),
        mime: f.mimeType || "",
        w: f.width || 0,
        h: f.height || 0,
        fps: f.fps || 0,
        label: f.qualityLabel || "",
        bitrate: f.bitrate || 0,
        clen: +f.contentLength || 0,
        drm: !!(f.drmFamilies && f.drmFamilies.length),
        audioDefault: f.audioTrack ? !!f.audioTrack.audioIsDefault : true,
        audioName: f.audioTrack ? f.audioTrack.displayName || "" : "",
        audioId: f.audioTrack ? f.audioTrack.id || "" : "",
        lmt: f.lastModified || "",
        xtags: f.xtags || "",
        durMs: +f.approxDurationMs || 0,
      });
      const data = {
        videoId: vd.videoId,
        title: vd.title || document.title,
        duration: +vd.lengthSeconds || 0,
        live: !!vd.isLive,
        formats: [...(sd.formats || []).map(fmt), ...(sd.adaptiveFormats || []).map(fmt)],
        hls: sd.hlsManifestUrl || "",
        sabr: !!sd.serverAbrStreamingUrl,
        sabrUrl: sd.serverAbrStreamingUrl || "",
        ustreamer: (((resp.playerConfig || {}).mediaCommonConfig || {}).mediaUstreamerRequestConfig || {}).videoPlaybackUstreamerConfig || "",
        clientVersion: (window.ytcfg && window.ytcfg.get && window.ytcfg.get("INNERTUBE_CLIENT_VERSION")) || "",
        visitorData: (window.ytcfg && window.ytcfg.get && window.ytcfg.get("VISITOR_DATA")) || "",
      };
      const key = data.videoId + ":" + data.formats.length + ":" + data.formats.filter((f) => f.url).length;
      if (key === lastYt) return;
      lastYt = key;
      window.postMessage({ __rdm: MAGIC, yt: data }, "*");
    } catch {}
  }

  if (YT) {
    const poll = () => {
      try {
        const p = document.getElementById("movie_player");
        const resp = (p && typeof p.getPlayerResponse === "function" && p.getPlayerResponse()) || window.ytInitialPlayerResponse;
        ytReport(resp);
      } catch {}
    };
    document.addEventListener("yt-navigate-finish", () => setTimeout(poll, 500));
    document.addEventListener("yt-page-data-updated", () => setTimeout(poll, 500));
    window.addEventListener("load", poll);
    setInterval(poll, 2000);
  }


  // ---- YouTube streaming requests (SABR): keep the player's latest request (URL + body)
  // as the template RDM replays, plus a few response samples for diagnostics.
  const SABR = /\.googlevideo\.com\/videoplayback\?.*sabr=1/;
  let lastSabrPost = 0;
  let samples = 0;

  function b64(bytes) {
    let s = "";
    for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
    return btoa(s);
  }

  async function bodyBytes(body) {
    try {
      if (!body) return null;
      if (body instanceof Uint8Array) return body;
      if (body instanceof ArrayBuffer) return new Uint8Array(body);
      if (ArrayBuffer.isView(body)) return new Uint8Array(body.buffer, body.byteOffset, body.byteLength);
      if (body instanceof Blob) return new Uint8Array(await body.arrayBuffer());
      if (typeof body === "string") return new TextEncoder().encode(body);
    } catch {}
    return null;
  }

  async function sabrRequest(url, body) {
    try {
      if (Date.now() - lastSabrPost < 1500) return;
      const bytes = await bodyBytes(body);
      if (!bytes || !bytes.length) return;
      lastSabrPost = Date.now();
      window.postMessage({ __rdm: MAGIC, sabr: { url, body: b64(bytes) } }, "*");
    } catch {}
  }

  async function sabrSample(url, response) {
    try {
      if (samples >= 3 || !response || !response.body) return;
      samples++;
      const reader = response.body.getReader();
      const parts = [];
      let n = 0;
      while (n < 65536) {
        const { value, done } = await reader.read();
        if (done || !value) break;
        parts.push(value);
        n += value.length;
      }
      try {
        reader.cancel();
      } catch {}
      const all = new Uint8Array(n);
      let o = 0;
      for (const p of parts) {
        all.set(p, o);
        o += p.length;
      }
      window.postMessage({ __rdm: MAGIC, sabrSample: { url, status: response.status, type: response.headers.get("content-type"), data: b64(all.subarray(0, 65536)) } }, "*");
    } catch {}
  }

  const origFetch = window.fetch;
  if (typeof origFetch === "function") {
    const hooked = function (input, init) {
      const p = origFetch.apply(this, arguments);
      try {
        const url = abs(typeof input === "string" ? input : input && input.url);
        if (YT && SABR.test(url)) {
          if (init && init.body) sabrRequest(url, init.body);
          else if (input && typeof input === "object" && input.clone) input.clone().arrayBuffer().then((b) => sabrRequest(url, b), () => {});
        }
        p.then(
          (r) => {
            try {
              if (YT && SABR.test(url)) {
                if (samples < 3) sabrSample(url, r.clone());
                return;
              }
              if (YT && PLAYER_API.test(r.url || url)) {
                r.clone().json().then(ytReport, () => {});
                return;
              }
              report(r.url || url, r.headers.get("content-type"), +r.headers.get("content-length") || 0);
            } catch {}
          },
          () => {}
        );
      } catch {}
      return p;
    };
    try {
      Object.defineProperty(hooked, "toString", { value: () => origFetch.toString() });
    } catch {}
    window.fetch = hooked;
  }

  const XHR = window.XMLHttpRequest && window.XMLHttpRequest.prototype;
  if (XHR) {
    const origOpen = XHR.open;
    const origSend = XHR.send;
    XHR.send = function (body) {
      try {
        if (YT && this.__rdmUrl && SABR.test(this.__rdmUrl)) sabrRequest(this.__rdmUrl, body);
      } catch {}
      return origSend.apply(this, arguments);
    };
    XHR.open = function (method, url) {
      try {
        const target = abs(url);
        this.__rdmUrl = target;
        this.addEventListener("load", () => {
          try {
            const u = this.responseURL || target;
            if (YT && PLAYER_API.test(u)) {
              const body = this.responseType === "json" ? this.response : this.responseType === "" || this.responseType === "text" ? JSON.parse(this.responseText) : null;
              ytReport(body);
              return;
            }
            report(u, this.getResponseHeader("content-type"), +this.getResponseHeader("content-length") || 0);
          } catch {}
        });
      } catch {}
      return origOpen.apply(this, arguments);
    };
  }
})();
