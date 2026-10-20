#!/usr/bin/env python3
"""Persistent HTTPS Sidecar + disposable Navidrome, scoped users and real audio.

Silent, synthetic fixtures only. No existing server URL or installation accepted.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import secrets
import shutil
import ssl
import subprocess
import tempfile
import time
import urllib.parse
import urllib.error
import urllib.request

from access_proof import child, fixture, port, wait_ready, request, payload
from authenticated_audio import AudioProbe, request as audio_request
from navidrome import Navidrome, NoRedirect, require, stop_group


def main(args):
    require(ssl.HAS_TLSv1_3, "Use Python with TLS 1.3 support.")
    checks = []

    def check(name, condition):
        require(condition, name)
        checks.append(name)
        print("PASS " + name, flush=True)

    with tempfile.TemporaryDirectory(prefix="sidecar-owner-streaming-") as temporary:
        root = Path(temporary)
        a_dir, b_dir = root / "a", root / "b"
        a_dir.mkdir()
        b_dir.mkdir()
        fixture(a_dir / "alpha.wav", 220, 60)
        fixture(b_dir / "beta.wav", 330, 60)
        nav_port, side_port = port(), port()
        base = f"http://localhost:{nav_port}/music"
        origin = f"https://localhost:{side_port}"
        side = origin + "/stratum"
        data = root / "sidecar"
        binary = str(args.sidecar.resolve())
        owner_password, admin_password = secrets.token_urlsafe(24), secrets.token_urlsafe(24)
        sensitive = [owner_password, admin_password]

        def cli(*arguments, input=None):
            result = subprocess.run([binary, *arguments, "--data-dir", str(data)], input=input,
                                    text=True, capture_output=True, timeout=12)
            require(result.returncode == 0, "Sidecar CLI failed: " + result.stderr)
            require(not any(secret in result.stdout + result.stderr for secret in sensitive),
                    "credential appeared in CLI output")
            return result.stdout

        cli("init", "--listen", f"127.0.0.1:{side_port}", "--admin-origin", origin)
        cli("set-password", "--password-stdin", input=owner_password)
        settings = root / "settings.json"
        settings.write_text(json.dumps({"navidrome_url":base,
            "listeners":[{"url":origin, "trust":"sidecar_key"}]}))
        cli("configure", "--settings", str(settings))
        private = json.loads((data / "private.json").read_text())
        context = ssl.create_default_context(cadata=private["certificate_pem"])
        context.minimum_version = ssl.TLSVersion.TLSv1_3
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect(),
                                             urllib.request.HTTPSHandler(context=context))

        def audio(path, method="GET", body=None, token=None):
            return audio_request(side, path, method, body, token, opener=opener)

        def owner_api(path, body=None, cookie=None, expected=200):
            headers = {"Origin":origin, "Content-Type":"application/json"}
            if cookie:
                headers["Cookie"] = cookie
            req = urllib.request.Request(origin + "/admin/api/" + path,
                data=None if body is None else json.dumps(body).encode(), headers=headers)
            try:
                response = opener.open(req, timeout=12)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                require(response.status == expected, "unexpected owner API status")
                result = response.read(65537)
                require(len(result) <= 65536, "owner response exceeded limit")
                return json.loads(result), response.headers.get("Set-Cookie")

        env = {k:v for k,v in os.environ.items() if not k.startswith("ND_")}
        env.update({"ND_ADDRESS":"127.0.0.1", "ND_PORT":str(nav_port), "ND_BASEURL":"/music",
                    "ND_MUSICFOLDER":str(a_dir), "ND_DATAFOLDER":str(root / "nav-data"),
                    "ND_CACHEFOLDER":str(root / "nav-cache"), "ND_LOGLEVEL":"error",
                    "ND_ENABLEEXTERNALSERVICES":"false", "ND_ENABLEINSIGHTSCOLLECTOR":"false",
                    "ND_ENABLEGRAVATAR":"false", "ND_ENABLEARTWORKPRECACHE":"false",
                    "ND_SCANNER_SCHEDULE":"0", "ND_FFMPEGPATH":shutil.which("ffmpeg") or "ffmpeg"})
        with child([str(args.navidrome.resolve())], root, "navidrome", env, lifetime=150) as nav:
            wait_ready(f"http://localhost:{nav_port}", "/ping", nav)
            auth = payload(request(base, "/auth/createAdmin", "POST",
                                   {"username":"setup-admin", "password":admin_password}))
            sensitive.extend([auth["token"], auth["subsonicToken"]])
            admin_headers = {"x-nd-authorization":"Bearer " + auth["token"]}

            def native(path, method="GET", body=None):
                return payload(request(base, "/api" + path, method, body, admin_headers))

            library_b = native("/library", "POST",
                               {"name":"B", "path":str(b_dir), "defaultNewUsers":False})
            admin = Navidrome(base, "setup-admin", admin_password)
            admin.request("startScan")
            deadline = time.monotonic() + 20
            while True:
                songs = admin.request("getRandomSongs", size=10).get("randomSongs", {}).get("song", [])
                if len(songs) == 2:
                    break
                require(time.monotonic() < deadline, "scan did not complete")
                time.sleep(.1)
            tracks = {"alice":next(s["id"] for s in songs if s["title"] == "alpha"),
                      "bob":next(s["id"] for s in songs if s["title"] == "beta")}
            credentials, users, readers = {}, {}, {}
            for name, libraries in [("alice", [1]), ("bob", [int(library_b["id"])])]:
                password, salt = secrets.token_urlsafe(24), secrets.token_hex(16)
                token = hashlib.md5((password + salt).encode()).hexdigest()
                sensitive.extend([password, salt, token])
                user = native("/user", "POST", {"userName":name, "name":name,
                                                "password":password, "isAdmin":False})
                native(f"/user/{user['id']}/library", "PUT", {"libraryIds":libraries})
                check(name + " is an ordinary scoped user", not native("/user/" + user["id"])["isAdmin"])
                credentials[name] = {"username":name, "token":token, "salt":salt}
                users[name] = user
                readers[name] = Navidrome(base, name, password)
            cli("publish-discovery", "--username", "setup-admin", "--password-stdin", input=admin_password)
            station = readers["alice"].request("getInternetRadioStations")["internetRadioStations"]["internetRadioStation"][0]
            descriptor = json.loads(urllib.parse.unquote(station["homePageUrl"].split("#stratum-v1=")[1]))

            with child([binary, "serve", "--data-dir", str(data)], root, "sidecar", lifetime=120) as service:
                deadline = time.monotonic() + 8
                while True:
                    require(service.poll() is None, "Sidecar exited")
                    try:
                        cap = payload(audio("/access/v1/capabilities"))
                        break
                    except OSError:
                        require(time.monotonic() < deadline, "Sidecar did not start")
                        time.sleep(.05)
                check("published endpoint serves matching identity over verified HTTPS",
                      station["streamUrl"] == side + "/access/v1/capabilities"
                      and cap["spkiSha256"] == descriptor["spkiSha256"]
                      and cap["installation"] == descriptor["installation"] and cap["ready"] is True)
                check("anonymous audio denied", audio("/audio/v1/playbacks/" + tracks["alice"], "POST")[0] == 401)
                owner_api("dashboard", expected=401)
                _, owner_cookie = owner_api("login", {"password":owner_password})
                owner_cookie = owner_cookie.split(";", 1)[0]
                sensitive.append(owner_cookie.split("=", 1)[1])
                live, _ = owner_api("dashboard", cookie=owner_cookie)
                check("owner dashboard starts empty and requires owner sign-in",
                      live["activity"]["clients"] == [] and live["bytes_per_second"] == 0)
                check("wrong listener sign-in denied", audio("/access/v1/sessions", "POST",
                      {**credentials["alice"], "token":"0"*32})[0] == 401)
                tokens = {}
                for name in users:
                    tokens[name] = payload(audio("/access/v1/sessions", "POST", credentials[name]), 201)["session"]
                    sensitive.append(tokens[name])
                probe = AudioProbe(side, tokens, tracks, {"alice":a_dir/"alpha.wav", "bob":b_dir/"beta.wav"},
                                   root, check, opener=opener)
                live, _ = owner_api("dashboard", cookie=owner_cookie)
                clients = live["activity"]["clients"]
                check("dashboard observes real streamed quality and cached delivery",
                      len(clients) == 2 and sum(len(c["streams"]) for c in clients) == 2
                      and all(c["streams"][0]["quality"] == "opus-128" for c in clients)
                      and live["bytes_per_second"] > 0)
                check("dashboard detects real Navidrome without retaining listener identity",
                      live["navidrome"]["status"] == "responding"
                      and not any(secret in json.dumps(live) for secret in [*tokens.values(), "alice", "bob"]))
                native(f"/user/{users['alice']['id']}/library", "PUT", {"libraryIds":[int(library_b["id"])]})
                probe.denied("alice", 404, "live library revocation")
                native(f"/user/{users['alice']['id']}/library", "PUT", {"libraryIds":[1]})
                probe.reopen("alice", tokens["alice"])
                check("logout succeeds", audio("/access/v1/session", "DELETE", token=tokens["alice"])[0] == 204)
                probe.denied("alice", 401, "logout")
                live, _ = owner_api("dashboard", cookie=owner_cookie)
                check("dashboard removes a logged-out listener", len(live["activity"]["clients"]) == 1)
                stop_group(nav)
                probe.denied("bob", 503, "upstream outage")
                stop_group(service)

            # Reuse the same persistent identity/configuration after a real restart.
            with child([binary, "serve", "--data-dir", str(data)], root, "restarted-sidecar", lifetime=20) as restarted:
                deadline = time.monotonic() + 8
                while True:
                    require(restarted.poll() is None, "restarted Sidecar exited")
                    try:
                        cap = payload(audio("/access/v1/capabilities"))
                        break
                    except OSError:
                        require(time.monotonic() < deadline, "restart did not complete")
                        time.sleep(.05)
                check("restart keeps advertised identity but rejects old listener sessions",
                      cap["spkiSha256"] == descriptor["spkiSha256"]
                      and audio("/audio/v1/playbacks/" + tracks["bob"], "POST", token=tokens["bob"])[0] == 401)
            for path in [*root.glob("*sidecar.log"), *data.glob("*.json")]:
                # Owner password is intentionally hashed. Listener/admin secrets
                # and bearer sessions must never be persisted or logged.
                require(not any(secret in path.read_text() for secret in sensitive), "credential retained")
            check("Sidecar state and logs contain no reusable credentials", True)
    print(json.dumps({"passed":len(checks), "all_children_stopped":True, "audible_playback":False}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--navidrome", type=Path, required=True)
    parser.add_argument("--sidecar", type=Path, required=True)
    main(parser.parse_args())
