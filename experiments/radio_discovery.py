#!/usr/bin/env python3
"""Disposable real Navidrome radio discovery + direct pinned TLS experiment.

No existing server URL, real credentials, audio, GUI, or persistent service.
The record format and TLS fixture are experiments, not a production protocol.
"""
import argparse
import contextlib
import copy
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import subprocess
import tempfile
import time
import urllib.parse

from access_proof import child, payload, port, request, wait_ready
from navidrome import Navidrome, require

MARKER = "stratum-v1="
CAPABILITIES = "/proxy/stratum/access/v1/capabilities"


def descriptor(metadata):
    return metadata["endpoints"][0] + "#" + MARKER + urllib.parse.quote(
        json.dumps(metadata, separators=(",", ":")), safe="")


def parse_record(radio):
    """Bounded candidate parser. Do not recognize announcements by name alone."""
    homepage = radio.get("homePageUrl", "")
    if not isinstance(homepage, str) or len(homepage) > 4096:
        return None
    try:
        url = urllib.parse.urlsplit(homepage)
        if not url.fragment.startswith(MARKER):
            return None
        metadata = json.loads(urllib.parse.unquote(url.fragment[len(MARKER):]))
        if not isinstance(metadata, dict) or type(metadata.get("version")) is not int or metadata["version"] != 1:
            return None
        if not re.fullmatch(r"[a-f0-9]{32}", metadata.get("installation", "")):
            return None
        if not re.fullmatch(r"[a-f0-9]{64}", metadata.get("spkiSha256", "")):
            return None
        endpoints = metadata.get("endpoints")
        if not isinstance(endpoints, list) or not 1 <= len(endpoints) <= 4:
            return None
        for endpoint in endpoints:
            if not isinstance(endpoint, str) or len(endpoint) > 1024:
                return None
            parsed = urllib.parse.urlsplit(endpoint)
            if (parsed.scheme != "https" or not parsed.hostname or parsed.username or
                    parsed.password or parsed.query or parsed.fragment or
                    any(ord(c) <= 32 or ord(c) == 127 for c in endpoint)):
                return None
            _ = parsed.port
        if endpoints[0] != radio.get("streamUrl") or homepage.split("#", 1)[0] != endpoints[0]:
            return None
        return metadata
    except (ValueError, TypeError, AttributeError):
        return None


def unique_candidate(radios):
    candidates = [value for radio in radios if (value := parse_record(radio)) is not None]
    # Deliberately refuse ambiguity; no first-result wins.
    return candidates[0] if len(candidates) == 1 else None


def make_certificate(state, name):
    key, cert = state / (name + ".key"), state / (name + ".pem")
    subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
                    "-keyout", str(key), "-out", str(cert), "-days", "1",
                    "-subj", "/CN=stratum-radio-experiment"], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=15)
    key.chmod(0o600)
    public = subprocess.run(["openssl", "x509", "-in", str(cert), "-pubkey", "-noout"],
                            check=True, capture_output=True, timeout=5).stdout
    der = subprocess.run(["openssl", "pkey", "-pubin", "-outform", "DER"], input=public,
                         check=True, capture_output=True, timeout=5).stdout
    return key, cert, hashlib.sha256(der).hexdigest()


@contextlib.contextmanager
def tls_fixture(state, name, installation):
    key, cert, fingerprint = make_certificate(state, name)
    ready, requests = state / (name + "-ready.json"), state / (name + "-requests.jsonl")

    def seen():
        return [json.loads(line) for line in requests.read_text().splitlines()] if requests.exists() else []

    with child(["node", str(Path(__file__).with_name("radio_tls_probe.mjs")), "serve",
                str(key), str(cert), installation, str(ready), str(requests)], state, name, lifetime=90) as process:
        deadline = time.monotonic() + 5
        while not ready.exists():
            require(process.poll() is None and time.monotonic() < deadline, "TLS fixture did not start")
            time.sleep(0.02)
        yield json.loads(ready.read_text())["endpoint"], fingerprint, seen


def verify_endpoint(metadata):
    value = subprocess.run(["node", str(Path(__file__).with_name("radio_tls_probe.mjs"))],
        input=json.dumps({"endpoint": metadata["endpoints"][0], "spkiSha256": metadata["spkiSha256"]}).encode(),
        capture_output=True, timeout=5, check=True)
    return json.loads(value.stdout)


def main(args):
    args.out.mkdir(parents=True, exist_ok=False)
    checks, observations, private = [], {}, []

    def check(name, condition):
        require(condition, name)
        checks.append(name)
        print("PASS " + name, flush=True)

    with tempfile.TemporaryDirectory(prefix="instance-", dir=args.out) as directory:
        state = Path(directory)
        (state / "music-a").mkdir()
        (state / "music-b").mkdir()
        nav_port = port()
        base = f"http://127.0.0.1:{nav_port}"
        env = {k: v for k, v in os.environ.items() if not k.startswith("ND_")}
        env.update({"ND_ADDRESS": "127.0.0.1", "ND_PORT": str(nav_port),
                    "ND_MUSICFOLDER": str(state / "music-a"), "ND_DATAFOLDER": str(state / "data"),
                    "ND_CACHEFOLDER": str(state / "cache"), "ND_LOGLEVEL": "error",
                    "ND_ENABLEEXTERNALSERVICES": "false", "ND_ENABLEINSIGHTSCOLLECTOR": "false",
                    "ND_ENABLEGRAVATAR": "false", "ND_ENABLEARTWORKPRECACHE": "false",
                    "ND_SCANNER_SCHEDULE": "0", "ND_FFMPEGPATH": shutil.which("ffmpeg") or "ffmpeg"})
        with child([str(args.navidrome.resolve())], state, "navidrome", env, lifetime=90) as process:
            wait_ready(base, "/ping", process)
            password = secrets.token_urlsafe(24)
            auth = payload(request(base, "/auth/createAdmin", "POST", {"username": "owner", "password": password}))
            admin_headers = {"x-nd-authorization": "Bearer " + auth["token"]}
            private.extend([password, auth["token"]])
            admin = Navidrome(base, "owner", password)

            def native(path, method="GET", body=None):
                return payload(request(base, "/api" + path, method, body, admin_headers))

            library_b = native("/library", "POST", {"name": "Restricted B", "path": str(state / "music-b"), "defaultNewUsers": False})
            users = []
            for username, libraries in [("alice", [1]), ("bob", [int(library_b["id"])])]:
                user_password = secrets.token_urlsafe(24)
                user = native("/user", "POST", {"userName": username, "name": username, "password": user_password, "isAdmin": False})
                native(f"/user/{user['id']}/library", "PUT", {"libraryIds": libraries})
                check(username + " is non-admin with only its assigned library", not native("/user/" + user["id"])["isAdmin"] and
                      [int(lib["id"]) for lib in native(f"/user/{user['id']}/library")] == libraries)
                login = payload(request(base, "/auth/login", "POST", {"username": username, "password": user_password}))
                private.extend([user_password, login["token"]])
                users.append((Navidrome(base, username, user_password), {"x-nd-authorization": "Bearer " + login["token"]}))

            def stations(client):
                return client.request("getInternetRadioStations")["internetRadioStations"].get("internetRadioStation", [])

            def publish(metadata, name="Stratum — discovery experiment"):
                admin.request("createInternetRadioStation", name=name, streamUrl=metadata["endpoints"][0], homepageUrl=descriptor(metadata))

            installation = secrets.token_hex(16)
            with tls_fixture(state, "sidecar", installation) as (endpoint, fingerprint, seen), \
                    tls_fixture(state, "wrong-sidecar", installation) as (wrong_endpoint, wrong_fingerprint, wrong_seen):
                metadata = {"version": 1, "installation": installation, "spkiSha256": fingerprint,
                            "endpoints": [endpoint, "https://music.example.invalid/prefix/stratum/access/v1/capabilities",
                                          "https://[fd7a:115c:a1e0::1]:9876/stratum/access/v1/capabilities"]}
                publish(metadata)
                first = stations(admin)
                check("admin published exactly one radio announcement", len(first) == 1)
                radio = first[0]
                observations["returned_fields"] = sorted(radio)
                observations["descriptor_bytes"] = len(descriptor(metadata).encode())
                observations["candidate_metadata"] = metadata
                check("escaped metadata, alternate addresses and base path round-trip exactly", radio["homePageUrl"] == descriptor(metadata) and parse_record(radio) == metadata)
                check("create/list did not fetch the advertised endpoint", not seen())
                for client, headers in users:
                    who = client.auth["u"]
                    check(who + " reads exact announcement despite different library scope", stations(client) == first)
                    check(who + " sees announcement as an ordinary native radio row", any(row["id"] == radio["id"] for row in payload(request(base, "/api/radio", headers=headers))))
                    for action, params in [
                        ("createInternetRadioStation", {"name": "forged", "streamUrl": wrong_endpoint}),
                        ("updateInternetRadioStation", {"id": radio["id"], "name": "forged", "streamUrl": wrong_endpoint, "homepageUrl": wrong_endpoint}),
                        ("deleteInternetRadioStation", {"id": radio["id"]}),
                    ]:
                        body = payload(request(base, client.url(action, **params).removeprefix(base)))["subsonic-response"]
                        check(who + " denied " + action + " with code 50 and no mutation", body["status"] == "failed" and body.get("error", {}).get("code") == 50 and stations(admin) == first)
                    for method, path, body in [
                        ("POST", "/api/radio", {"name": "forged", "streamUrl": wrong_endpoint}),
                        ("PUT", "/api/radio/" + radio["id"], {"name": "forged", "streamUrl": wrong_endpoint, "homePageUrl": wrong_endpoint}),
                        ("DELETE", "/api/radio/" + radio["id"], None),
                    ]:
                        check(who + " denied native " + method + " with 403 and no mutation", request(base, path, method, body, headers)[0] == 403 and stations(admin) == first)

                anonymous = payload(request(base, "/rest/getInternetRadioStations.view?v=1.16.1&c=stratum-radio-proof&f=json"))["subsonic-response"]
                check("anonymous Subsonic reader cannot obtain announcement", anonymous["status"] == "failed" and "internetRadioStations" not in anonymous)
                check("anonymous native reader cannot obtain announcement", request(base, "/api/radio")[0] == 401)
                bad = Navidrome(base, "alice", "deliberately-wrong-password")
                refused = payload(request(base, bad.url("getInternetRadioStations").removeprefix(base)))["subsonic-response"]
                check("wrong password cannot read announcement", refused["status"] == "failed" and refused["error"]["code"] == 40)

                candidate = unique_candidate(stations(users[0][0]))
                check("exactly one valid announcement selected", candidate == metadata)
                result = verify_endpoint(candidate)
                check("advertised key verified through real TLS before fetching capabilities", result.get("ok") and result["pinMatched"] and result["info"]["installation"] == installation and len(seen()) == 1)
                wrong = copy.deepcopy(candidate)
                wrong["endpoints"][0] = wrong_endpoint
                result = verify_endpoint(wrong)
                check("substituted endpoint rejected before any HTTP request", result == {"pinMatched": False, "ok": False, "reason": "identity-mismatch"} and not wrong_seen())
                bad_pin = {**candidate, "spkiSha256": "0" * 64}
                result = verify_endpoint(bad_pin)
                check("wrong advertised key rejected before any HTTP request", not result["ok"] and not result["pinMatched"] and len(seen()) == 1)

                admin.request("createInternetRadioStation", name="Ordinary station", streamUrl="https://radio.example.invalid/live", homepageUrl="https://radio.example.invalid/")
                check("ordinary station does not interfere with discovery", unique_candidate(stations(users[0][0])) == metadata)
                for name, change in [("future version", {"version": 2}), ("malformed pin", {"spkiSha256": "not-a-key"}),
                                     ("credential-bearing endpoint", {"endpoints": ["https://user:pass@example.invalid/"]}),
                                     ("unsupported endpoint scheme", {"endpoints": ["file:///private/tmp/example"]})]:
                    broken = {**metadata, **change}
                    check(name + " is not a candidate", parse_record({"homePageUrl": descriptor(broken), "streamUrl": broken["endpoints"][0]}) is None)
                check("oversized announcement is not a candidate", parse_record({"homePageUrl": endpoint + "#" + MARKER + "x" * 4096}) is None)
                check("malformed JSON is not a candidate", parse_record({"homePageUrl": endpoint + "#" + MARKER + "%7B"}) is None)

                duplicate_reply = payload(request(base, admin.url("createInternetRadioStation",
                    name=radio["name"], streamUrl=metadata["endpoints"][0], homepageUrl=descriptor(metadata)).removeprefix(base)))["subsonic-response"]
                observations["identical_station_rejection"] = duplicate_reply.get("error")
                check("Navidrome refuses an identical station without changing discovery", duplicate_reply["status"] == "failed" and unique_candidate(stations(users[0][0])) == metadata)
                same_name = payload(request(base, admin.url("createInternetRadioStation",
                    name=radio["name"], streamUrl=wrong_endpoint, homepageUrl=wrong_endpoint).removeprefix(base)))["subsonic-response"]
                check("station name is unique even when its URL differs", same_name["status"] == "failed" and "radio.name" in same_name.get("error", {}).get("message", "") and unique_candidate(stations(users[0][0])) == metadata)
                publish(metadata, "Stratum — same URL, different name")
                same_url = stations(users[0][0])
                check("same URL under a different name creates an ambiguous second announcement", len([r for r in same_url if parse_record(r)]) == 2 and unique_candidate(same_url) is None)
                same_url_id = next(r["id"] for r in same_url if parse_record(r) and r["id"] != radio["id"])
                admin.request("deleteInternetRadioStation", id=same_url_id)
                alternate = {**metadata, "spkiSha256": wrong_fingerprint, "endpoints": [wrong_endpoint]}
                publish(alternate, "Stratum — second discovery experiment")
                doubled = stations(users[0][0])
                check("distinct announcement names and URLs coexist; client refuses ambiguity", len([r for r in doubled if parse_record(r)]) == 2 and unique_candidate(doubled) is None)
                duplicate_id = next(r["id"] for r in doubled if parse_record(r) and r["id"] != radio["id"])
                admin.request("deleteInternetRadioStation", id=duplicate_id)
                check("owner duplicate cleanup restores unique discovery", unique_candidate(stations(users[0][0])) == metadata)

                rotated = {**metadata, "spkiSha256": wrong_fingerprint, "endpoints": [wrong_endpoint]}
                admin.request("updateInternetRadioStation", id=radio["id"], name=radio["name"], streamUrl=wrong_endpoint, homepageUrl=descriptor(rotated))
                refreshed = unique_candidate(stations(users[0][0]))
                check("owner endpoint/key update reaches listener exactly", refreshed == rotated and refreshed != candidate)
                check("previously approved pin rejects newly advertised key", not verify_endpoint({**refreshed, "spkiSha256": candidate["spkiSha256"]})["ok"] and not wrong_seen())
                check("explicitly selected replacement pin verifies replacement endpoint", verify_endpoint(refreshed)["ok"] and len(wrong_seen()) == 1)
                check("discovery sent no credentials, cookies or auth query parameters", all(not item["has_auth"] and item["path"] == CAPABILITIES for item in seen() + wrong_seen()))

                admin.request("deleteInternetRadioStation", id=radio["id"])
                remaining = stations(users[0][0])
                check("owner removal withdraws Stratum but preserves ordinary station", unique_candidate(remaining) is None and len(remaining) == 1 and remaining[0]["name"] == "Ordinary station")
        check("disposable Navidrome process stopped", process.poll() is not None)
    check("temporary database, accounts and private TLS keys removed", not state.exists())
    report = {"navidrome_version": subprocess.check_output([str(args.navidrome), "--version"], timeout=5).decode().strip(),
              "checks": checks, "observations": observations,
              "scope": "Radio publication/discovery and direct pinned TLS fixture; no production Sidecar transport, proxy, saved-source integration, credentials forwarding, or audio."}
    encoded = json.dumps(report, indent=2) + "\n"
    require(all(secret not in encoded for secret in private), "secret in report")
    (args.out / "report.json").write_text(encoded)
    print(f"{len(checks)} checks passed; temporary services stopped; report: {args.out / 'report.json'}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--navidrome", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    main(parser.parse_args())
