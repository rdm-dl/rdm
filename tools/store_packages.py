"""Build the browser-store uploads into dist/store/.

usage: python tools/store_packages.py

- rdm-chrome-web-store.zip  Chrome Web Store (also used by Brave, Vivaldi, Opera), without YouTube support
- rdm-edge-addons.zip        Microsoft Edge Add-ons
- rdm-firefox-amo.zip        addons.mozilla.org (listed or unlisted signing)

The stores assign their own extension IDs, so the signing "key" used for loading the
unpacked extension is removed from the store copies.
"""
import json
import os
import zipfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EXT = os.path.join(ROOT, "extension")
OUT = os.path.join(ROOT, "dist", "store")
FILES = ["background.js", "content.js", "document.js", "popup.html", "popup.css", "popup.js", "logo.svg", "logo-light.svg"]


def build(name, manifest_file, youtube=True):
    manifest = json.load(open(os.path.join(EXT, manifest_file), encoding="utf-8"))
    manifest.pop("key", None)
    path = os.path.join(OUT, name)
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("manifest.json", json.dumps(manifest, indent=2) + "\n")
        for f in FILES:
            text = open(os.path.join(EXT, f), "rb").read()
            if not youtube and f.endswith(".js"):
                flag = b"const YOUTUBE_SUPPORT = true;"
                text = text.replace(flag, b"const YOUTUBE_SUPPORT = false;")
            z.writestr(f, text)
        for f in sorted(os.listdir(os.path.join(EXT, "icons"))):
            z.write(os.path.join(EXT, "icons", f), f"icons/{f}")
    print(f"{path}  (version {manifest['version']})")


if __name__ == "__main__":
    os.makedirs(OUT, exist_ok=True)
    # The Chrome Web Store does not allow YouTube downloaders.
    build("rdm-chrome-web-store.zip", "manifest.json", youtube=False)
    build("rdm-edge-addons.zip", "manifest.json")
    build("rdm-firefox-amo.zip", "manifest.firefox.json")
