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

from access_proof import child, port, wait_ready, request as nav_request, payload as nav_payload
from navidrome import Navidrome
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
        # This fixture exercises many admin operations in seconds. Do not change
        # a real server's authentication policy.
        if args.publication:
            env["ND_AUTHREQUESTLIMIT"] = "100"
        cli("init", "--listen", f"127.0.0.1:{owner_port}", "--admin-origin", origin)
        cli("set-password", "--password-stdin", input=password)
        private = json.loads((data / "private.json").read_text())
        context = ssl.create_default_context(cadata=private["certificate_pem"])
        context.minimum_version = ssl.TLSVersion.TLSv1_3
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), urllib.request.HTTPSHandler(context=context))
        cookie = None

        def api(path, body=None, expected=200):
            headers = {"Origin":origin, "Content-Type":"application/json"}
            if cookie:
                headers["Cookie"] = cookie
            req = urllib.request.Request(origin + "/admin/api/" + path,
                                         data=None if body is None else json.dumps(body).encode(), headers=headers)
            try:
                response = opener.open(req, timeout=35)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                value = json.loads(response.read(32768))
                require(response.status == expected, f"Owner API returned {response.status}, expected {expected}: {value.get('error', '')}")
                return value, response.headers

        with child([str(args.navidrome.resolve())], state, "navidrome", env, lifetime=90) as nav:
            wait_ready(f"http://127.0.0.1:{nav_port}", "/ping", nav)
            detection = json.loads(cli("check-navidrome", "--url", base))
            require(detection["url"] == base and detection["authenticated"] is False, "CLI detection failed")
            print("PASS real Navidrome detected at a path prefix, without a listener account")
            with child([binary, "serve", "--data-dir", str(data)], state, "sidecar", lifetime=70) as sidecar:
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
                if args.publication:
                    admin_password = secrets.token_urlsafe(24)
                    auth = nav_payload(nav_request(base, "/auth/createAdmin", "POST", {"username":"owner", "password":admin_password}))
                    native_headers = {"x-nd-authorization":"Bearer " + auth["token"]}
                    listener_password = secrets.token_urlsafe(24)
                    nav_payload(nav_request(base, "/api/user", "POST", {"userName":"listener", "name":"Listener", "password":listener_password, "isAdmin":False}, native_headers))
                    admin = Navidrome(base, "owner", admin_password)
                    reader = Navidrome(base, "listener", listener_password)
                    admin.request("createInternetRadioStation", name="Ordinary radio", streamUrl="https://radio.example/live", homepageUrl="https://radio.example/")

                    def radios():
                        return reader.request("getInternetRadioStations").get("internetRadioStations", {}).get("internetRadioStation", [])

                    def publication(action, username="owner", secret=None, expected=200, recreate=False, update_listeners=None):
                        current, _ = api("setup")
                        return api("discovery", {"action":action, "setup_revision":current["setup"]["revision"],
                            "discovery_revision":current["discovery"]["revision"], "username":username,
                            "password":admin_password if secret is None else secret, "recreate":recreate,
                            **({"listeners":update_listeners} if update_listeners is not None else {})}, expected)[0]

                    publication("publish", "listener", listener_password, expected=400)
                    require(len(radios()) == 1, "ordinary account changed radio list")
                    current, _ = api("setup")
                    api("discovery", {"action":"manual", "setup_revision":current["setup"]["revision"], "discovery_revision":current["discovery"]["revision"]})
                    print("PASS ordinary account cannot publish; manual setup remains available")
                    publication("publish", secret="wrong-password", expected=400)
                    require(len(radios()) == 1, "wrong password changed radio list")
                    result = publication("publish")
                    require(result["discovery"]["status"] == "published", "publication not verified")
                    station = next(row for row in radios() if row["name"].startswith("Stratum — "))
                    from urllib.parse import unquote
                    metadata = json.loads(unquote(station["homePageUrl"].split("#stratum-v1=", 1)[1]))
                    require(metadata["endpoints"] == listeners, "trust/address metadata changed")
                    require(metadata["spkiSha256"] == json.loads(cli("status"))["identity"]["spkiSha256"], "identity mismatch")
                    print("PASS real administrator publishes exact direct/proxy metadata for ordinary listeners")
                    publication("publish")
                    require(len(radios()) == 2, "repeated publish created a duplicate")
                    listeners[0]["url"] = "https://changed.home.lan:7843"
                    current, _ = api("setup")
                    api("setup/listeners", {"revision":current["setup"]["revision"], "listeners":listeners}, expected=400)
                    publication("publish", secret="wrong-password", expected=400, update_listeners=listeners)
                    unchanged, _ = api("setup")
                    require(unchanged["setup"] == current["setup"], "failed address update saved new settings")
                    result = publication("publish", update_listeners=listeners)
                    require(result["setup"]["settings"]["listeners"] == listeners, "successful update did not save addresses")
                    require(result["discovery"]["status"] == "published", "address update left discovery stale")
                    updated = next(row for row in radios() if row["id"] == station["id"])
                    require(updated["streamUrl"].startswith(listeners[0]["url"]), "did not update same station")
                    print("PASS repeated publication is idempotent and changes update the saved station")
                    admin.request("deleteInternetRadioStation", id=station["id"])
                    publication("publish", expected=400)
                    require(len(radios()) == 1, "missing announcement was recreated without explicit choice")
                    publication("publish", recreate=True)
                    print("PASS externally removed announcement requires explicit recreation")
                    cli("remove-discovery", "--username", "owner", "--password-stdin", input=admin_password)
                    require(len(radios()) == 1 and radios()[0]["name"] == "Ordinary radio", "removal damaged unrelated station")
                    cli("publish-discovery", "--username", "owner", "--password-stdin", input=admin_password)
                    require(len(radios()) == 2, "CLI publication failed")
                    publication("remove")
                    require(len(radios()) == 1, "web removal failed")
                    print("PASS CLI and web share verified publication/removal state")
                    for filename in ["config.json", "private.json", "discovery.json"]:
                        contents = (data / filename).read_text()
                        require(all(secret not in contents for secret in [admin_password, listener_password, auth["token"], auth["subsonicToken"]]), "Navidrome credential saved")
                    print("PASS no Navidrome administrator credentials retained in Sidecar state")
                print("Navidrome version: " + str(detection["server_version"]))
        print("PASS disposable services stopped; temporary state removed on exit")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--navidrome", type=Path, required=True)
    parser.add_argument("--sidecar", type=Path, required=True)
    parser.add_argument("--publication", action="store_true", help="also exercise announcement maintenance with disposable accounts")
    main(parser.parse_args())
