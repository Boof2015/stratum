#!/usr/bin/env python3
"""Bounded owner setup check with a disposable real Navidrome and Sidecar.

No existing server URL, library, browser, audio or reusable credentials.
"""
import argparse
import json
import os
from pathlib import Path
import secrets
import shutil
import ssl
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

from access_proof import child, port, wait_ready
from navidrome import require


def main(args):
    require(ssl.HAS_TLSv1_3, "This check needs Python with TLS 1.3 support (macOS's bundled Python may lack it).")
    with tempfile.TemporaryDirectory(prefix="sidecar-owner-setup-") as temporary:
        state = Path(temporary)
        music = state / "music"
        music.mkdir()
        data = state / "sidecar"
        binary = str(args.sidecar.resolve())
        password = secrets.token_urlsafe(24)

        def cli(*arguments, input=None):
            result = subprocess.run([binary, *arguments, "--data-dir", str(data)], input=input,
                                    capture_output=True, text=True, timeout=12)
            require(result.returncode == 0, "Sidecar command failed: " + result.stderr)
            require(password not in result.stdout + result.stderr, "password appeared in output")
            return result.stdout

        nav_port, owner_port = port(), port()
        base = f"http://127.0.0.1:{nav_port}/music"
        origin = f"https://localhost:{owner_port}"
        env = {k: v for k, v in os.environ.items() if not k.startswith("ND_")}
        env.update({"ND_ADDRESS":"127.0.0.1", "ND_PORT":str(nav_port), "ND_BASEURL":"/music",
                    "ND_MUSICFOLDER":str(music), "ND_DATAFOLDER":str(state / "nav-data"),
                    "ND_CACHEFOLDER":str(state / "nav-cache"), "ND_LOGLEVEL":"error",
                    "ND_ENABLEEXTERNALSERVICES":"false", "ND_ENABLEINSIGHTSCOLLECTOR":"false",
                    "ND_ENABLEGRAVATAR":"false", "ND_ENABLEARTWORKPRECACHE":"false",
                    "ND_SCANNER_SCHEDULE":"0", "ND_FFMPEGPATH":shutil.which("ffmpeg") or "ffmpeg"})
        cli("init", "--listen", f"127.0.0.1:{owner_port}", "--admin-origin", origin)
        cli("set-password", "--password-stdin", input=password)
        private = json.loads((data / "private.json").read_text())
        context = ssl.create_default_context(cadata=private["certificate_pem"])
        context.minimum_version = ssl.TLSVersion.TLSv1_3
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), urllib.request.HTTPSHandler(context=context))
        cookie = None

        def api(path, body=None):
            headers = {"Origin":origin, "Content-Type":"application/json"}
            if cookie:
                headers["Cookie"] = cookie
            req = urllib.request.Request(origin + "/admin/api/" + path,
                                         data=None if body is None else json.dumps(body).encode(), headers=headers)
            with opener.open(req, timeout=8) as response:
                return json.loads(response.read(32768)), response.headers

        with child([str(args.navidrome.resolve())], state, "navidrome", env, lifetime=55) as nav:
            wait_ready(f"http://127.0.0.1:{nav_port}", "/ping", nav)
            detection = json.loads(cli("check-navidrome", "--url", base))
            require(detection["url"] == base and detection["authenticated"] is False, "CLI detection failed")
            print("PASS real Navidrome detected at a path prefix, without a listener account")
            with child([binary, "serve", "--data-dir", str(data)], state, "sidecar", lifetime=40) as sidecar:
                deadline = time.monotonic() + 8
                while True:
                    require(sidecar.poll() is None, "Sidecar stopped")
                    try:
                        api("status")
                        break
                    except (OSError, urllib.error.URLError):
                        require(time.monotonic() < deadline, "Sidecar startup timeout")
                        time.sleep(.05)
                _, headers = api("login", {"password":password})
                cookie = headers["Set-Cookie"].split(";", 1)[0]
                saved, _ = api("setup/navidrome", {"revision":"initial", "url":base + "/"})
                require(saved["detection"]["authenticated"] is False and saved["stage"] == "listeners", "web detection failed")
                print("PASS verified HTTPS owner request checks and saves real Navidrome")
                listeners = [{"url":"https://sidecar.home.lan:7843", "trust":"sidecar_key"},
                             {"url":"https://music.example/stratum", "trust":"https_proxy"}]
                saved, _ = api("setup/listeners", {"revision":saved["setup"]["revision"], "listeners":listeners})
                require(saved["stage"] == "connections-saved", "connections were not saved")
                settings = json.loads(cli("status"))["config"]["setup"]["settings"]
                require(settings == {"navidrome_url":base, "listeners":listeners}, "CLI and web settings disagree")
                print("PASS direct/proxy addresses persist and terminal reads the same settings")
                bad = subprocess.run([binary, "check-navidrome", "--data-dir", str(data), "--url", base.replace("/music", "/wrong")],
                                     capture_output=True, text=True, timeout=12)
                require(bad.returncode != 0, "wrong prefix accepted")
                require(json.loads(cli("status"))["config"]["setup"] == saved["setup"], "failed check changed saved settings")
                print("PASS a wrong path fails without changing saved settings")
                print("Navidrome version: " + str(detection["server_version"]))
        print("PASS disposable services stopped; temporary state removed on exit")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--navidrome", type=Path, required=True)
    parser.add_argument("--sidecar", type=Path, required=True)
    main(parser.parse_args())
