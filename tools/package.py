"""Package release binaries + browser extensions into dist/.

usage: python tools/package.py [--target <rust-target>] [--name <platform-name>]
Run after `cargo build --release [--target ...]`.
"""
import argparse
import json
import os
import platform
import shutil
import tarfile
import zipfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EXT_FILES = ["background.js", "popup.html", "popup.css", "popup.js"]


def version():
    for line in open(os.path.join(ROOT, "Cargo.toml"), encoding="utf-8"):
        if line.strip().startswith("version"):
            return line.split("=")[1].strip().strip('"')
    return "0.0.0"


def build_extensions(out_dir):
    src = os.path.join(ROOT, "extension")
    chrome = os.path.join(out_dir, "extension-chrome")
    os.makedirs(os.path.join(chrome, "icons"), exist_ok=True)
    for f in EXT_FILES + ["manifest.json"]:
        shutil.copy(os.path.join(src, f), chrome)
    for f in os.listdir(os.path.join(src, "icons")):
        shutil.copy(os.path.join(src, "icons", f), os.path.join(chrome, "icons"))
    # Firefox: same files, Firefox manifest, packed as .xpi (a zip).
    xpi = os.path.join(out_dir, "rdm-firefox.xpi")
    with zipfile.ZipFile(xpi, "w", zipfile.ZIP_DEFLATED) as z:
        for f in EXT_FILES:
            z.write(os.path.join(src, f), f)
        z.write(os.path.join(src, "manifest.firefox.json"), "manifest.json")
        for f in os.listdir(os.path.join(src, "icons")):
            z.write(os.path.join(src, "icons", f), f"icons/{f}")


def mac_app(out_dir, bin_dir, ver):
    app = os.path.join(out_dir, "RDM.app", "Contents")
    os.makedirs(os.path.join(app, "MacOS"), exist_ok=True)
    os.makedirs(os.path.join(app, "Resources"), exist_ok=True)
    for b in ("rdm-gui", "rdm", "rdm-bridge"):
        shutil.copy2(os.path.join(bin_dir, b), os.path.join(app, "MacOS", b))
    plist = {
        "CFBundleName": "RDM",
        "CFBundleDisplayName": "RDM",
        "CFBundleIdentifier": "app.rdm.downloadmanager",
        "CFBundleVersion": ver,
        "CFBundleShortVersionString": ver,
        "CFBundleExecutable": "rdm-gui",
        "CFBundlePackageType": "APPL",
        "LSMinimumSystemVersion": "11.0",
        "NSHighResolutionCapable": True,
    }
    lines = ['<?xml version="1.0" encoding="UTF-8"?>', '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">', '<plist version="1.0"><dict>']
    for k, v in plist.items():
        lines.append(f"<key>{k}</key>" + ("<true/>" if v is True else f"<string>{v}</string>"))
    lines.append("</dict></plist>")
    open(os.path.join(app, "Info.plist"), "w").write("\n".join(lines))
    shutil.copy(os.path.join(ROOT, "assets", "icon256.png"), os.path.join(app, "Resources", "icon.png"))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--target", default="")
    ap.add_argument("--name", default="")
    ap.add_argument("--bin-dir", default="")
    a = ap.parse_args()
    ver = version()
    system = platform.system().lower()
    name = a.name or {"windows": "windows-x64", "darwin": "macos", "linux": "linux-x64"}.get(system, system)
    exe = ".exe" if system == "windows" else ""
    bin_dir = a.bin_dir or (os.path.join(ROOT, "target", a.target, "release") if a.target else os.path.join(ROOT, "target", "release"))
    pkg = f"RDM-{ver}-{name}"
    out = os.path.join(ROOT, "dist", pkg)
    shutil.rmtree(out, ignore_errors=True)
    os.makedirs(out)
    if system == "darwin":
        mac_app(out, bin_dir, ver)
    else:
        for b in ("rdm-gui", "rdm", "rdm-bridge"):
            shutil.copy2(os.path.join(bin_dir, b + exe), out)
    build_extensions(out)
    for f in ("README.md", "LICENSE"):
        if os.path.exists(os.path.join(ROOT, f)):
            shutil.copy(os.path.join(ROOT, f), out)
    json.dump({"version": ver, "platform": name}, open(os.path.join(out, "build.json"), "w"))
    dist = os.path.join(ROOT, "dist")
    if system == "windows":
        archive = os.path.join(dist, pkg + ".zip")
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as z:
            for base, _, files in os.walk(out):
                for f in files:
                    full = os.path.join(base, f)
                    z.write(full, os.path.join(pkg, os.path.relpath(full, out)))
    else:
        archive = os.path.join(dist, pkg + ".tar.gz")
        with tarfile.open(archive, "w:gz") as t:
            t.add(out, arcname=pkg)
    print(archive)


if __name__ == "__main__":
    main()
