#!/usr/bin/env python3
"""Bounded Docker packaging checks using disposable volumes, no personal server."""
import argparse
import http.client
import json
from pathlib import Path
import re
import signal
import ssl
import subprocess
import time
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", default="astra-sidecar:local")
    parser.add_argument("--archive", type=Path, help="Optional standalone archive matching this image")
    args = parser.parse_args()
    prefix = "stratum-package-test-" + uuid.uuid4().hex[:12]
    name = prefix + "-server"
    volumes = [prefix + "-data", prefix + "-empty"]
    checks = []

    def docker(*command, input=None, ok=True, timeout=30):
        result = subprocess.run(["docker", *command], input=input, text=True,
                                capture_output=True, timeout=timeout)
        if ok and result.returncode:
            # Commands can contain setup-code output/password input. Never dump
            # complete output or arguments on a failed check.
            raise RuntimeError("Docker " + command[0] + " failed: " + result.stderr[-1200:])
        return result

    def check(label, condition):
        if not condition:
            raise AssertionError(label)
        checks.append(label)
        print("PASS " + label, flush=True)

    def cli(*command, input=None, ok=True):
        return docker("exec", "-i", name, "sidecar", *command,
                      "--data-dir", "/var/lib/sidecar", input=input, ok=ok)

    def run(volume, origin="https://localhost:7843"):
        docker("run", "-d", "--name", name,
               "--publish", "127.0.0.1::7843",
               "--mount", "type=volume,src=" + volume + ",dst=/var/lib/sidecar",
               "--read-only", "--tmpfs", "/tmp:rw,noexec,nosuid,size=128m,mode=1777",
               "--cap-drop", "ALL", "--security-opt", "no-new-privileges:true",
               "--pids-limit", "256", "--stop-timeout", "20",
               "--env", "SIDECAR_ADMIN_ORIGIN=" + origin, args.image)

    def state():
        return json.loads(docker("inspect", "--format", "{{json .State}}", name).stdout)

    def await_start():
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            log = docker("logs", name).stdout
            if "Administration address:" in log and state()["Running"]:
                status = cli("status", ok=False)
                if status.returncode == 0:
                    identity = json.loads(docker("exec", name, "cat", "/var/lib/sidecar/private.json").stdout)
                    context = ssl.create_default_context(cadata=identity["certificate_pem"])
                    context.minimum_version = ssl.TLSVersion.TLSv1_3
                    ports = json.loads(docker("inspect", "--format", "{{json .NetworkSettings.Ports}}", name).stdout)
                    port = int(ports["7843/tcp"][0]["HostPort"])
                    connection = http.client.HTTPSConnection("localhost", port, context=context, timeout=1)
                    try:
                        connection.request("GET", "/admin/api/status", headers={"Host": "localhost:7843"})
                        response = connection.getresponse()
                        response.read()
                        if response.status == 200:
                            return context, port
                    except (OSError, http.client.HTTPException):
                        pass
                    finally:
                        connection.close()
            if not state()["Running"]:
                raise AssertionError("Packaged Sidecar exited during startup")
            time.sleep(.1)
        raise AssertionError("Packaged Sidecar startup timed out")

    def await_failure():
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            value = state()
            if not value["Running"]:
                return value["ExitCode"] != 0
            time.sleep(.1)
        return False

    def request(path, body=None, cookie=None):
        connection = http.client.HTTPSConnection("localhost", port, context=context, timeout=5)
        headers = {"Host": "localhost:7843", "Origin": "https://localhost:7843"}
        if cookie:
            headers["Cookie"] = cookie
        if body is not None:
            headers["Content-Type"] = "application/json"
        try:
            connection.request("GET" if body is None else "POST", "/admin/api/" + path,
                               body=None if body is None else json.dumps(body), headers=headers)
            response = connection.getresponse()
            payload = response.read(1024 * 1024)
            return response.status, response.getheader("Set-Cookie", "").split(";", 1)[0], payload
        finally:
            connection.close()

    def edit(script, input=None):
        return docker("run", "--rm", "-i", "--name", prefix + "-edit",
                      "--mount", "type=volume,src=" + volumes[0] + ",dst=/var/lib/sidecar",
                      "--entrypoint", "/bin/sh", args.image, "-ec", script, input=input)

    def expired(_signum, _frame):
        raise TimeoutError("Packaging checks exceeded five minutes")

    signal.signal(signal.SIGALRM, expired)
    signal.signal(signal.SIGTERM, expired)
    signal.alarm(300)
    try:
        docker("image", "inspect", args.image)
        for volume in volumes:
            docker("volume", "create", volume)
        run(volumes[0])
        context, port = await_start()
        check("non-root runtime, direct Sidecar PID 1",
              docker("exec", name, "id", "-u").stdout.strip() == "10001"
              and docker("exec", name, "cat", "/proc/1/comm").stdout.strip() == "sidecar")
        check("private persistent directory and state files",
              docker("exec", name, "stat", "-c", "%a", "/var/lib/sidecar",
                     "/var/lib/sidecar/config.json", "/var/lib/sidecar/private.json").stdout.split() == ["700", "600", "600"])
        inspect = json.loads(docker("inspect", name).stdout)[0]
        check("published port stays on loopback for this test and root filesystem is read-only",
              inspect["HostConfig"]["ReadonlyRootfs"]
              and inspect["NetworkSettings"]["Ports"]["7843/tcp"][0]["HostIp"] == "127.0.0.1")
        check("verified HTTPS protects owner dashboard", request("dashboard")[0] == 401)
        code = re.findall(r"Owner setup code \(valid for 30 minutes\): ([a-f0-9]+)", docker("logs", name).stdout)[-1]
        password = "disposable packaging password"
        replacement = "replacement packaging password"
        claimed = request("claim", {"code": code, "password": password})
        cookie = claimed[1]
        check("browser setup code enrolls owner", claimed[0] == 200 and bool(cookie))
        cli("configure-cache", "--limit-gb", "2")
        cli("configure-resources", "--conversions", "1", "--working-mb", "128")
        saved = json.loads(cli("status").stdout)
        check("packaged CLI updates cache and resource settings",
              saved["config"]["storage"]["limit_bytes"] == 2_000_000_000
              and saved["config"]["resources"]["working_bytes"] == 128_000_000)
        # These are silent generated files, not an audio-device test.
        docker("exec", name, "ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "lavfi",
               "-i", "anullsrc=r=48000:cl=stereo", "-t", "0.1", "-c:a", "libopus", "/tmp/check.opus")
        probe = docker("exec", name, "ffprobe", "-v", "error", "-show_entries", "stream=codec_name",
                       "-of", "json", "/tmp/check.opus")
        check("runtime includes working FFmpeg, ffprobe and Opus encoder",
              json.loads(probe.stdout)["streams"][0]["codec_name"] == "opus")
        docker("stop", "--time", "20", name)
        check("SIGTERM exits cleanly", state()["ExitCode"] == 0)
        docker("start", name)
        context, port = await_start()
        check("restart preserves identity and settings", json.loads(cli("status").stdout) == saved)
        check("restart revokes previous dashboard session", request("setup", cookie=cookie)[0] == 401)
        logged = request("login", {"password": password})
        check("owner can sign in after restart", logged[0] == 200)
        cli("reset-password", "--password-stdin", input=replacement + "\n")
        check("live recovery revokes old session and password",
              request("setup", cookie=logged[1])[0] == 401
              and request("login", {"password": password})[0] == 401
              and request("login", {"password": replacement})[0] == 200
              and json.loads(cli("status").stdout) == saved)
        docker("kill", "--signal", "KILL", name)
        docker("start", name)
        context, port = await_start()
        check("abrupt exit releases locks and keeps installation", json.loads(cli("status").stdout) == saved)
        docker("stop", "--time", "20", name)
        docker("rm", name)
        run(volumes[0], "https://changed.example:9999")
        context, port = await_start()
        check("container replacement reuses saved config instead of initialization environment",
              json.loads(cli("status").stdout) == saved)
        docker("stop", "--time", "20", name)
        original = edit("cat /var/lib/sidecar/config.json").stdout
        private_before = edit("sha256sum /var/lib/sidecar/private.json").stdout
        edit("printf broken > /var/lib/sidecar/config.json")
        docker("start", name)
        check("corrupt configuration stops without replacing private identity",
              await_failure() and edit("sha256sum /var/lib/sidecar/private.json").stdout == private_before)
        edit("cat > /var/lib/sidecar/config.json", input=original)
        edit("mv /var/lib/sidecar/private.json /var/lib/sidecar/private.saved")
        docker("start", name)
        check("missing private identity is never regenerated on restart", await_failure()
              and edit("test ! -e /var/lib/sidecar/private.json").returncode == 0)
        edit("mv /var/lib/sidecar/private.saved /var/lib/sidecar/private.json")
        docker("start", name)
        context, port = await_start()
        check("restoring original files recovers same installation", json.loads(cli("status").stdout) == saved)
        docker("stop", "--time", "20", name)
        docker("rm", name)
        run(volumes[1], "")
        check("fresh volume requires explicit administration origin", await_failure())
        docker("rm", name)
        run(volumes[1], "http://localhost:7843")
        check("fresh volume rejects plaintext administration origin", await_failure())
        if args.archive:
            archive = args.archive.resolve(strict=True)
            # The same binary is shipped in the image and archive. Confirm that
            # relationship, then exercise the unpacked CLI independently.
            script = """set -eu
mkdir /tmp/package
tar -xzf /package.tar.gz -C /tmp/package
set -- /tmp/package/sidecar-*/sidecar
test "$#" -eq 1
binary=$1
cmp "$binary" /usr/local/bin/sidecar
test -f "$(dirname "$binary")/sidecar.service"
test -x "$(dirname "$binary")/install.sh"
test -s "$(dirname "$binary")/INSTALL.md"
"$binary" init --data-dir /tmp/state >/dev/null
"$binary" set-password --data-dir /tmp/state --password-stdin >/dev/null
"$binary" status --data-dir /tmp/state | grep -q '"ownerConfigured": true'
test "$(stat -c %a /tmp/state/private.json)" = 600
"$binary" init --data-dir /tmp/state --listen 0.0.0.0:9999 --admin-origin https://changed.example:9999 >/dev/null
"$binary" status --data-dir /tmp/state | grep -q '127.0.0.1:7843'
"""
            docker("run", "--rm", "-i", "--name", prefix + "-archive", "--network", "none",
                   "--read-only", "--tmpfs", "/tmp:rw,exec,nosuid,size=128m,mode=1777",
                   "--mount", "type=bind,src=" + str(archive) + ",dst=/package.tar.gz,readonly",
                   "--entrypoint", "/bin/sh", args.image, "-ec", script,
                   input="disposable standalone password\n")
            check("standalone archive contains the tested binary and supports independent setup", True)
    finally:
        signal.alarm(0)
        for container in [name, prefix + "-edit", prefix + "-archive"]:
            docker("rm", "--force", container, ok=False)
        for volume in volumes:
            docker("volume", "rm", volume, ok=False)
        if (docker("ps", "-aq", "--filter", "name=" + prefix).stdout.strip()
                or docker("volume", "ls", "-q", "--filter", "name=" + prefix).stdout.strip()):
            raise RuntimeError("Could not remove all disposable packaging resources: " + prefix)
    print(json.dumps({"passed": len(checks), "cleanup_complete": True, "audio_played": False}))


if __name__ == "__main__":
    main()
