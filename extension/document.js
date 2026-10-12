// Runs in the page's own JavaScript context. Watches the page's fetch/XMLHttpRequest
// traffic for stream manifests and media responses (including ones a service worker
// serves, which the extension's webRequest listener can't attribute to the tab) and
// reports them to content.js with window.postMessage.

(() => {
  if (window.__rdmDocHook) return;
  window.__rdmDocHook = true;
  const MAGIC = 0x52444d34; // "RDM4"
  const URL_RE = /\.(m3u8|mpd|mp4|m4v|m4a|webm|mkv|mp3|aac|flac|ogg|ogv|mov|flv|vtt|srt|ttml|dfxp)(\?|#|$)/i;
  const TYPE_RE = /^(video\/|audio\/|application\/(vnd\.apple\.mpegurl|x-mpegurl|dash\+xml|ttml\+xml)|text\/vtt)/i;
  const SEGMENT_RE = /\.(ts|m4s|m4f|cmfv|cmfa)(\?|#|$)/i;
  const seen = new Set();

  function report(url, type, size) {
    try {
      if (!url || seen.has(url) || !/^https?:/i.test(url)) return;
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

  const origFetch = window.fetch;
  if (typeof origFetch === "function") {
    const hooked = function (input, init) {
      const p = origFetch.apply(this, arguments);
      try {
        const url = abs(typeof input === "string" ? input : input && input.url);
        p.then(
          (r) => {
            try {
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
    XHR.open = function (method, url) {
      try {
        const target = abs(url);
        this.addEventListener("load", () => {
          try {
            report(this.responseURL || target, this.getResponseHeader("content-type"), +this.getResponseHeader("content-length") || 0);
          } catch {}
        });
      } catch {}
      return origOpen.apply(this, arguments);
    };
  }
})();
