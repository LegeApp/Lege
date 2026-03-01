import os
import sys
import json
import time
import shlex
import subprocess
from pathlib import Path
from typing import Optional

# --------------------- CONFIG ---------------------
OWNER = "LegeApp"
REPO  = "Lege"

# Release tags & targets
TAG_DEB          = "Linux-deb"        # this one will be (re)marked as latest
TAG_LINUX_UNIV   = "linux-universal"
TAG_WINDOWS_LOCAL= "Windows-local"

# Whether to make the deb release the "latest" one
MARK_DEB_AS_LATEST = True

# WSL distro name (wsl -l -v to list)
WSL_DISTRO = "Ubuntu"

# Windows-visible shared output dir for all final artifacts:
WIN_OUT = Path(r"Z:\Lege\out")  # CHANGE to a real Windows path you have; e.g. r"C:\lege\out"
WIN_OUT.mkdir(parents=True, exist_ok=True)

# WSL build/output dirs (inside Linux)
WSL_PROJECT_ROOT = "/home/dk/Lege"  # your Linux project checkout (adjust)
WSL_LINUX_RUN    = "/home/dk/Desktop/lege-run/installer/linux64"
WSL_OUT          = "/home/dk/Lege/out"  # will be bind-mounted or just copied to /mnt/… that Windows can see

# Windows inputs for installer/MSIX
WIN_INSTALLER_DIR = Path(r"/mnt/Samsung980_1TB/Lege/installer")  # accessible via WSL path too, but we run makeappx on Windows
WIN_MSIX_DIR      = Path(r"/mnt/Samsung980_1TB/Lege/Lege-MSIX/Lege-MSIX")  # Contains appxmanifest.xml etc.
MSIX_NAME         = "Lege-fixed.msix"

# Expected filenames (final artifacts)
LINUX_TAR_GZ_NAME = "lege-linux64.tar.gz"     # universal
WIN_LOCAL_EXE     = "lege-installer.exe"      # your local installer exe name
# .deb name will be discovered from cargo-deb output (*.deb)

# GitHub API
GITHUB_API = "https://api.github.com"
GITHUB_UPLOADS = "https://uploads.github.com"
GITHUB_TOKEN = os.environ.get("GITHUB_TOKEN")  # MUST be set on Windows
# --------------------------------------------------

def run(cmd: list[str], cwd: Optional[Path] = None, check=True):
    print(f":: RUN {' '.join(map(shlex.quote, cmd))}")
    return subprocess.run(cmd, cwd=str(cwd) if cwd else None, check=check)

def run_wsl(bash_script: str):
    """Run a multi-line bash script inside the configured WSL distro."""
    cmd = ["wsl", "-d", WSL_DISTRO, "bash", "-lc", bash_script]
    return run(cmd)

def gh_api(method: str, url: str, headers=None, data=None, is_json=True, binary=False):
    import urllib.request, urllib.error
    req_headers = headers or {}
    req_headers["Authorization"] = f"token {GITHUB_TOKEN}"
    if is_json and not binary:
        req_headers["Accept"] = "application/vnd.github+json"
    req = urllib.request.Request(url, method=method, headers=req_headers)
    if data is not None:
        if is_json and not binary:
            payload = json.dumps(data).encode("utf-8")
            req.add_header("Content-Type", "application/json")
        else:
            payload = data
    else:
        payload = None
    try:
        with urllib.request.urlopen(req, data=payload) as resp:
            body = resp.read()
            if is_json and not binary:
                return json.loads(body.decode("utf-8")) if body else {}
            return body
    except urllib.error.HTTPError as e:
        msg = e.read().decode("utf-8", errors="ignore")
        print(f"HTTP {e.code}: {msg}")
        raise

def ensure_release(tag: str, make_latest=False):
    # Try get
    url = f"{GITHUB_API}/repos/{OWNER}/{REPO}/releases/tags/{tag}"
    try:
        rel = gh_api("GET", url)
        # optionally flip latest: create a new draft+publish pattern is typical,
        # but GitHub now has "make latest" in UI; API supports editing release
        if make_latest:
            edit_url = f"{GITHUB_API}/repos/{OWNER}/{REPO}/releases/{rel['id']}"
            rel = gh_api("PATCH", edit_url, data={"make_latest": "true"})
        return rel
    except Exception:
        pass

    # Create if not found
    create_url = f"{GITHUB_API}/repos/{OWNER}/{REPO}/releases"
    data = {
        "tag_name": tag,
        "name": tag,
        "prerelease": False,
        "draft": False
    }
    if make_latest:
        data["make_latest"] = "true"
    rel = gh_api("POST", create_url, data=data)
    return rel

def upload_asset(rel: dict, file_path: Path, name_override: Optional[str] = None):
    assert file_path.exists(), f"Missing file: {file_path}"
    name = name_override or file_path.name
    print(f"Uploading {file_path} as {name} to release {rel['tag_name']}")

    # delete existing asset with same name (if present)
    assets = rel.get("assets", [])
    for a in assets:
        if a["name"] == name:
            del_url = f"{GITHUB_API}/repos/{OWNER}/{REPO}/releases/assets/{a['id']}"
            gh_api("DELETE", del_url)
            break

    upload_url = f"{GITHUB_UPLOADS}/repos/{OWNER}/{REPO}/releases/{rel['id']}/assets?name={name}"
    headers = {"Content-Type": "application/octet-stream"}
    with open(file_path, "rb") as f:
        data = f.read()
    gh_api("POST", upload_url, headers=headers, data=data, is_json=False, binary=True)
    print(f"✔ Uploaded {name}")

def find_deb(out_dir: Path) -> Path:
    cands = list(out_dir.glob("*.deb"))
    if not cands:
        raise FileNotFoundError("No .deb found in output dir")
    # Pick the newest
    cands.sort(key=lambda p: p.stat().st_mtime, reverse=True)
    return cands[0]

def main():
    if not GITHUB_TOKEN:
        print("Set GITHUB_TOKEN on Windows environment.")
        sys.exit(
