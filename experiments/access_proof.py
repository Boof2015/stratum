#!/usr/bin/env python3
"""Real disposable Navidrome + Rust access service. Silent; synthetic files only.

By default this checks 64-byte prefixes; --audio adds real FLAC/Opus delivery.
Never reads Astra credentials or accepts an existing server URL.
"""
import argparse
import contextlib
import hashlib
import json
import math
import os
from pathlib import Path
import secrets
import shutil
import socket
import struct
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
import wave

from navidrome import HTTP, Navidrome, require, stop_group


def request(base, path, method="GET", body=None, headers=None):
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(base + path, data=data, method=method,
                                    headers={"Content-Type": "application/json", **(headers or {})})
    try:
        response = HTTP.open(request, timeout=12)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        payload = response.read(65537)
        require(len(payload) <= 65536, "response too large")
        return response.status, dict(response.headers), payload


def payload(result, expected=200):
    require(result[0] == expected, f"unexpected HTTP status {result[0]}, expected {expected}")
    return json.loads(result[2]) if result[2] else None


@contextlib.contextmanager
def child(command, directory, name, environment=None, lifetime=180):
    with (directory / (name + ".log")).open("wb") as log:
        process = subprocess.Popen(command, cwd=directory, env=environment,
                                   stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                                   start_new_session=True)
        timer = threading.Timer(lifetime, stop_group, args=(process,))
        timer.daemon = True
        timer.start()
        try:
            yield process
        finally:
            timer.cancel()
            stop_group(process)
            require(process.poll() is not None, "test child did not exit")


def port():
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        return reservation.getsockname()[1]


def wait_ready(base, path, process):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        require(process.poll() is None, "test server exited during startup")
        try:
            if request(base, path)[0] == 200:
                return
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(0.05)
    raise RuntimeError("test server startup timed out")


def fixture(path, frequency, seconds=1):
    with wave.open(str(path), "wb") as output:
        output.setnchannels(2)
        output.setsampwidth(2)
        output.setframerate(48000)
        for second in range(seconds):
            output.writeframesraw(b"".join(struct.pack("<hh", sample, sample) for sample in
                                          (int(3000 * math.sin(2 * math.pi * frequency * n / 48000))
                                           for n in range(second * 48000, (second + 1) * 48000))))


def main(args):
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    checks = []
    secrets_used = []
    started = time.monotonic()

    def check(name, condition):
        require(condition, name)
        checks.append(name)
        print("PASS " + name, flush=True)

    # State and encrypted disposable credentials disappear after the run.
    with tempfile.TemporaryDirectory(prefix="state-", dir=out) as temporary:
        state = Path(temporary)
        a_dir, b_dir = state / "library-a", state / "library-b"
        a_dir.mkdir(); b_dir.mkdir()
        fixture(a_dir / "alpha.wav", 220, 60 if args.audio else 1)
        fixture(b_dir / "beta.wav", 330, 60 if args.audio else 1)
        expected_a = (a_dir / "alpha.wav").read_bytes()[:64]
        expected_b = (b_dir / "beta.wav").read_bytes()[:64]
        nav_port = port()
        base = f"http://127.0.0.1:{nav_port}"
        env = {k: v for k, v in os.environ.items() if not k.startswith("ND_")}
        env.update({"ND_ADDRESS": "127.0.0.1", "ND_PORT": str(nav_port),
                    "ND_MUSICFOLDER": str(a_dir), "ND_DATAFOLDER": str(state / "data"),
                    "ND_CACHEFOLDER": str(state / "cache"), "ND_LOGLEVEL": "error",
                    "ND_ENABLEEXTERNALSERVICES": "false", "ND_ENABLEINSIGHTSCOLLECTOR": "false",
                    "ND_ENABLEGRAVATAR": "false", "ND_ENABLEARTWORKPRECACHE": "false",
                    "ND_SCANNER_SCHEDULE": "0", "ND_FFMPEGPATH": shutil.which("ffmpeg") or "ffmpeg"})
        with child([str(args.navidrome.resolve())], state, "navidrome", env) as nav_process:
            wait_ready(base, "/ping", nav_process)
            admin_password = secrets.token_urlsafe(24)
            secrets_used.append(admin_password)
            auth = payload(request(base, "/auth/createAdmin", "POST", {"username": "setup-admin", "password": admin_password}))
            secrets_used.append(auth["token"])
            admin_headers = {"x-nd-authorization": "Bearer " + auth["token"]}

            def native(path, method="GET", body=None):
                return payload(request(base, "/api" + path, method, body, admin_headers))

            lib_b = native("/library", "POST", {"name": "Restricted B", "path": str(b_dir), "defaultNewUsers": False})
            admin = Navidrome(base, "setup-admin", admin_password)
            admin.request("startScan")
            deadline = time.monotonic() + 20
            songs = []
            while time.monotonic() < deadline:
                songs = admin.request("getRandomSongs", size=10).get("randomSongs", {}).get("song", [])
                if len(songs) == 2:
                    break
                time.sleep(0.1)
            require(len(songs) == 2, "two disposable libraries were not scanned")
            song_a = next(song["id"] for song in songs if song["title"] == "alpha")
            song_b = next(song["id"] for song in songs if song["title"] == "beta")
            users = {}
            credentials = {}
            for username, libraries in [("alice", [1]), ("bob", [int(lib_b["id"])])]:
                password = secrets.token_urlsafe(24)
                user = native("/user", "POST", {"userName": username, "name": username, "password": password, "isAdmin": False})
                native(f"/user/{user['id']}/library", "PUT", {"libraryIds": libraries})
                check(username + " is an ordinary scoped user", not native("/user/" + user["id"])["isAdmin"])
                salt = secrets.token_hex(16)
                token = hashlib.md5((password + salt).encode()).hexdigest()
                credentials[username] = {"username": username, "token": token, "salt": salt}
                users[username] = user
                secrets_used.extend([password, token, salt])

            def start_sidecar(stack, name, ttl=600):
                address = f"http://127.0.0.1:{port()}"
                config_path = state / (name + ".json")
                config_path.write_text(json.dumps({"upstream": base, "listen": address.removeprefix("http://"),
                                                   "session_seconds": ttl, "run_seconds": 120, "connection_seconds": 2}))
                process = stack.enter_context(child([str(args.sidecar.resolve()), "--config", str(config_path)], state, name, lifetime=150))
                wait_ready(address, "/access/v1/capabilities", process)
                return address, process

            with contextlib.ExitStack() as stack:
                side, side_process = start_sidecar(stack, "sidecar")

                def login(address, username):
                    value = payload(request(address, "/access/v1/sessions", "POST", credentials[username]), 201)
                    secrets_used.append(value["session"])
                    return value["session"]

                def media(address, token, track):
                    return request(address, "/access/v1/media/" + track,
                                   headers={"Authorization": "Bearer " + token} if token else {})

                check("capabilities explicitly deny playback support", payload(request(side, "/access/v1/capabilities"))["playback"] is False)
                check("anonymous media denied", media(side, None, song_a)[0] == 401)
                wrong = {**credentials["alice"], "token": "0" * 32}
                check("wrong credentials denied", request(side, "/access/v1/sessions", "POST", wrong)[0] == 401)
                a, b = login(side, "alice"), login(side, "bob")
                audio = None
                if args.audio:
                    from authenticated_audio import AudioProbe
                    audio = AudioProbe(side, {"alice": a, "bob": b}, {"alice": song_a, "bob": song_b},
                                       {"alice": a_dir / "alpha.wav", "bob": b_dir / "beta.wav"}, out, check)
                check("Alice receives exact original prefix", media(side, a, song_a)[2] == expected_a)
                warm = media(side, a, song_a)
                check("Alice warm cache served after revalidation", warm[0] == 200 and warm[1].get("x-stratum-cache-hit") == "1" and warm[2] == expected_a)
                check("Bob receives exact original prefix", media(side, b, song_b)[2] == expected_b)
                check("Bob warms his cache", media(side, b, song_b)[1].get("x-stratum-cache-hit") == "1")
                check("Alice denied known Bob track with warm cache", media(side, a, song_b)[0] == 404)
                check("Bob denied known Alice track", media(side, b, song_a)[0] == 404)
                native(f"/user/{users['alice']['id']}/library", "PUT", {"libraryIds": [int(lib_b["id"])]})
                before = time.monotonic()
                check("library revocation blocks Alice warm cache", media(side, a, song_a)[0] == 404)
                revoked_latency = time.monotonic() - before
                if audio: audio.denied("alice", 404, "library revocation")
                native(f"/user/{users['alice']['id']}/library", "PUT", {"libraryIds": [1]})
                check("restored permission can acquire again", media(side, a, song_a)[0] == 200)
                if audio: audio.reopen("alice", a)

                other, other_process = start_sidecar(stack, "other-sidecar")
                check("bearer cannot cross Sidecar instances", media(other, a, song_a)[0] == 401)
                expired, expired_process = start_sidecar(stack, "expiry-sidecar", ttl=1)
                exp = login(expired, "alice")
                check("short session initially works", media(expired, exp, song_a)[0] == 200)
                time.sleep(1.1)
                check("expired warm session denied", media(expired, exp, song_a)[0] == 401)
                check("logout succeeds", request(side, "/access/v1/session", "DELETE", headers={"Authorization": "Bearer " + a})[0] == 204)
                check("logged out bearer denied", media(side, a, song_a)[0] == 401)
                if audio: audio.denied("alice", 401, "logout")
                a = login(side, "alice")
                if audio: audio.reopen("alice", a)
                replacement = secrets.token_urlsafe(24)
                secrets_used.append(replacement)
                native("/user/" + users["alice"]["id"], "PUT", {"userName": "alice", "name": "alice", "isAdmin": False, "password": replacement})
                if audio: audio.denied("alice", 401, "changed password")
                check("password change invalidates existing upstream credential", media(side, a, song_a)[0] == 401)
                new_salt = secrets.token_hex(16)
                new_token = hashlib.md5((replacement + new_salt).encode()).hexdigest()
                old_credentials = credentials["alice"]
                credentials["alice"] = {"username": "alice", "token": new_token, "salt": new_salt}
                secrets_used.extend([new_salt, new_token])
                fresh_login = request(base, "/auth/login", "POST", {"username": "alice", "password": replacement})
                check(f"Navidrome accepts changed password directly (HTTP {fresh_login[0]})", fresh_login[0] == 200)
                fresh_payload = payload(fresh_login)
                secrets_used.append(fresh_payload["token"])
                check("changed-password tuple matches Navidrome derivation",
                      hashlib.md5((replacement + fresh_payload["subsonicSalt"]).encode()).hexdigest() == fresh_payload["subsonicToken"])
                fresh_a = login(side, "alice")
                check("changed password can establish a fresh session", media(side, fresh_a, song_a)[0] == 200)
                if audio: audio.reopen("alice", fresh_a)
                native("/user/" + users["alice"]["id"], "DELETE")
                if audio: audio.denied("alice", 401, "deleted account")
                check("account deletion blocks its warm session", media(side, fresh_a, song_a)[0] == 401)
                credentials["alice"] = old_credentials
                check("deleted account cannot create session", request(side, "/access/v1/sessions", "POST", credentials["alice"])[0] == 401)

                # Keep Bob's authorized warm session across a real Navidrome shutdown.
                check("Bob remains healthy before outage", media(side, b, song_b)[0] == 200)
                stop_group(nav_process)
                unavailable = media(side, b, song_b)
                check("upstream outage blocks warm cache without blaming password", unavailable[0] == 503 and json.loads(unavailable[2])["error"] == "upstream_unavailable")
                if audio:
                    audio.denied("bob", 503, "upstream outage")
                    audio.finish()
                stop_group(side_process)
                restarted, restarted_process = start_sidecar(stack, "restarted-sidecar")
                check("restart rejects old session before upstream contact", media(restarted, b, song_b)[0] == 401)

            # No credentials or sessions should appear in service output, including errors.
            for file in state.glob("*sidecar.log"):
                content = file.read_text()
                require(not any(secret in content for secret in secrets_used), "service log contains a credential")
            check("service logs contain no credentials", True)
            for file in state.glob("*.log"):
                # Save only logs which pass redaction, never the server DB or config secrets.
                content = file.read_text(errors="replace")
                for secret in secrets_used:
                    content = content.replace(secret, "[redacted]")
                (out / file.name).write_text(content)

    report = {"checks": checks, "passed": len(checks), "seconds": time.monotonic() - started,
              "revocation_request_seconds": revoked_latency, "playback": False,
              "probe_bytes": 64, "encoded_audio": args.audio, "all_children_stopped": True,
              "limits": "No TLS/discovery/encoding/source-revision claim. Two disposable ordinary users."}
    (out / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"passed": len(checks), "seconds": report["seconds"], "all_children_stopped": True}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--navidrome", type=Path, required=True)
    parser.add_argument("--sidecar", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--audio", action="store_true", help="Also test real encoded delivery from generated 60-second WAVs")
    main(parser.parse_args())
