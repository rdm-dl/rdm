"""Build the Windows setup program from what package.py left in dist/.

usage: python tools/installer.py [--name <platform-name>]
Needs Inno Setup 6.6 or newer. Run after tools/package.py.

Signing: set RDM_SIGN to a signing command with $f where the file goes, for example
    signtool sign /fd sha256 /tr http://timestamp.example /td sha256 $f
package.py signs the programs with it, and this signs the setup and its uninstaller.
"""
import argparse
import hashlib
import os
import shutil
import subprocess

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def version():
    for line in open(os.path.join(ROOT, "Cargo.toml"), encoding="utf-8"):
        if line.strip().startswith("version"):
            return line.split("=")[1].strip().strip('"')
    return "0.0.0"


def find_iscc():
    places = [
        os.environ.get("ISCC"),
        shutil.which("iscc"),
        os.path.join(os.environ.get("LOCALAPPDATA", ""), "Programs", "Inno Setup 6", "ISCC.exe"),
        os.path.join(os.environ.get("ProgramFiles(x86)", ""), "Inno Setup 6", "ISCC.exe"),
        os.path.join(os.environ.get("ProgramFiles", ""), "Inno Setup 6", "ISCC.exe"),
    ]
    for place in places:
        if place and os.path.exists(place):
            return place
    raise SystemExit("Inno Setup not found. Install it, or point ISCC at ISCC.exe.")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--name", default="windows-x64")
    a = ap.parse_args()
    ver = version()
    pkg = f"RDM-{ver}-{a.name}"
    source = os.path.join(ROOT, "dist", pkg)
    if not os.path.isdir(source):
        raise SystemExit(f"{source} not found: run tools/package.py first.")
    out_name = f"{pkg}-setup"
    command = [
        find_iscc(),
        "/Q",
        f"/DAppVersion={ver}",
        f"/DSourceDir={source}",
        f"/DOutputDir={os.path.join(ROOT, 'dist')}",
        f"/DOutputName={out_name}",
    ]
    sign = os.environ.get("RDM_SIGN")
    if sign:
        # Inno Setup writes a quote as $q.
        quoted = sign.replace('"', "$q")
        command += ["/DSign=1", f"/Srdmsign={quoted}"]
    command.append(os.path.join(ROOT, "tools", "installer", "rdm.iss"))
    subprocess.run(command, check=True)
    setup = os.path.join(ROOT, "dist", out_name + ".exe")
    # RDM checks its updates against this.
    digest = hashlib.sha256(open(setup, "rb").read()).hexdigest()
    with open(setup + ".sha256", "w", newline="\n") as f:
        f.write(f"{digest}  {os.path.basename(setup)}\n")
    print(setup)


if __name__ == "__main__":
    main()
