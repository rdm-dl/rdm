# Publishing the RDM extension

Uploads are built by `python tools/store_packages.py` into `dist/store/`:

| File | Store |
|---|---|
| `rdm-chrome-web-store.zip` | Chrome Web Store (Chrome, Brave, Vivaldi and Opera install from it) |
| `rdm-edge-addons.zip` | Microsoft Edge Add-ons |
| `rdm-firefox-amo.zip` | Firefox Add-ons (addons.mozilla.org) |

After each store approves the extension, put its ID in `extension/store-ids.json`. RDM's
**Set up browser integration** then adds the extension to that browser automatically
(and the browser bridge accepts the store build).

You'll also need:
- **Icon**: `extension/icons/icon128.png`
- **Screenshots**: 1280×800 PNGs (see "Screenshots" at the end)
- **Privacy policy URL**: https://jaylex32.github.io/rdm/privacy.html
- **Website**: https://jaylex32.github.io/rdm/

---

## 1. Chrome Web Store

1. Open the developer dashboard: https://chrome.google.com/webstore/devconsole
2. **Add new item** → upload `rdm-chrome-web-store.zip`.
3. **Store listing** tab
   - Title: `RDM Download Manager`
   - Summary: use "Short description" below
   - Description: use "Full description" below
   - Category: `Productivity` (or `Tools`)
   - Language: English
   - Store icon: `icon128.png`
   - Screenshots: at least one 1280×800
4. **Privacy practices** tab
   - Single purpose: use "Single purpose" below
   - Permission justifications: copy each one from "Permissions" below
   - Remote code: **No, I am not using remote code**
   - Data usage: tick nothing (RDM collects no user data); certify the three statements
   - Privacy policy URL: https://jaylex32.github.io/rdm/privacy.html
5. **Distribution** tab: Public (or Unlisted), all regions.
6. **Submit for review.** Review usually takes from a few days to a couple of weeks.
7. When it's published, copy the item ID (the 32-letter ID in the item's URL) into
   `extension/store-ids.json` → `"chrome_web_store"`.

## 2. Microsoft Edge Add-ons

1. Open Partner Center: https://partner.microsoft.com/dashboard/microsoftedge/overview
2. **Create new extension** → upload `rdm-edge-addons.zip`.
3. **Availability**: Public, all markets.
4. **Properties**: Category `Productivity`, privacy policy URL `https://jaylex32.github.io/rdm/privacy.html`, website `https://jaylex32.github.io/rdm/`.
5. **Store listings** → English: description (full description below), extension logo
   300×300 (use `assets/brand` icon), screenshots 1280×800.
6. **Submit** (notes for certification: "Requires the free RDM desktop app; the extension
   talks to it through native messaging host com.rdm.bridge.").
7. When it's published, copy the extension ID (on the extension's overview page) into
   `extension/store-ids.json` → `"edge_addons"`.

## 3. Firefox Add-ons

1. Open https://addons.mozilla.org/developers/ → **Submit a New Add-on**.
2. Choose where it's distributed:
   - **On this site** — listed publicly on addons.mozilla.org, or
   - **On your own** — Mozilla signs it but doesn't list it (fastest; signed in minutes).
3. Upload `rdm-firefox-amo.zip`. When asked for source code: **No** (the extension isn't
   minified or generated).
4. Listed: fill in the same texts and screenshots as above. Unlisted: just submit.
5. Download the **signed** `.xpi` Mozilla gives you and replace
   `dist/RDM-…/rdm-firefox.xpi` (and keep a copy for releases). RDM's browser integration
   registers that file with Firefox, which then offers to enable it.

---

## Texts

### Short description (≤ 132 characters)
Send downloads and videos to RDM: fast multi-connection downloads with resume, a video download button, and a site grabber.

### Full description
RDM Download Manager connects your browser to the free RDM desktop app.

• Downloads you start in the browser are handed to RDM, which splits each file across
  several connections for faster downloads and can pause and resume them — even after a
  restart or a dropped connection.
• A "Download this video" button appears on videos and audio players. It lists the
  qualities and formats the page offers (HLS/DASH streams, MP4, WebM, audio) and sends
  your choice to RDM.
• Right-click "Download with RDM", "Download all links" and "Download selected links".
• Hold Alt while clicking to always use RDM, or Ctrl to let the browser download instead.

Requires the RDM desktop app (Windows, macOS, Linux). The extension talks only to the RDM
app on your own computer; it sends nothing to any other server.

### Single purpose
Hands downloads and media from the browser to the RDM download manager app on the user's computer.

### Permissions
- **downloads** — Detects downloads the user starts so they can be handed to RDM, and cancels the browser's copy when RDM takes over.
- **contextMenus** — Adds "Download with RDM", "Download all links" and "Download selected links" to the right-click menu.
- **nativeMessaging** — Talks to the RDM desktop app on the user's computer (host `com.rdm.bridge`).
- **cookies** — Passes the site's cookies for a download to RDM so downloads that require being signed in work.
- **storage** — Remembers the user's settings (capture on/off, video button on/off, sites where the button is hidden).
- **webRequest** — Detects media files and streaming playlists the page loads, so the video button can offer them, and the request details (referrer) RDM needs to download them.
- **tabs** — Associates detected media with the tab it belongs to and shows the count on the toolbar icon.
- **scripting** — Collects the links on a page when the user chooses "Download all links".
- **Host permission (all sites)** — Downloads and videos can come from any website the user visits; the extension only acts when a download or media is found.

### Privacy policy
RDM Download Manager does not collect, store or share personal data.

The extension runs in your browser and communicates only with the RDM desktop app installed
on your own computer, through the browser's native messaging. When you download something
with RDM, the extension passes that download's address, the page it came from, and the
site's cookies for it to the RDM app on your computer so the app can download the file.
This information never leaves your computer except in the download request the app makes to
the website you are downloading from.

The extension has no analytics, no advertising, and sends nothing to the developers or to
any third party. Settings are stored locally in your browser.

---

## Screenshots

1280×800 PNGs that work well:
1. The video button menu on a page with a video (qualities and formats)
2. The RDM app's main window with downloads in progress
3. The popup (media on this page, capture switch)
4. The download details window (connections, speed)

Ready-made files in `docs/screenshots/`:
1. `extension-menu.png`
2. `main-dark.png`
3. `extension-popup.png`
4. `details.png`
