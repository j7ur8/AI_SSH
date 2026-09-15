#!/usr/bin/env python3
"""End-to-end acceptance test for the ai-ssh MCP surface.

Runs the real `aisshd` daemon and the real `aissh-mcp` stdio server against a
local SSH server, with HOME pointed at a throwaway directory so the operator's
own ~/.aissh configuration and their real hosts are never touched.

Usage:
    AISSH_TEST_SSH_PORT=... AISSH_TEST_SSH_KEY=/path/to/id python3 scripts/e2e_mcp_check.py

AISSH_TEST_SSH_HOST (default 127.0.0.1) and AISSH_TEST_SSH_USER (default root)
are optional. The user only needs passwordless key access to the test host.
"""

import base64
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
AISSH_MCP = REPO / "target" / "debug" / "aissh-mcp"
AISSH_D = REPO / "target" / "debug" / "aisshd"

checks = []


def check(label, condition, detail=""):
    checks.append((label, bool(condition), detail))
    status = "PASS" if condition else "FAIL"
    print(f"[{status}] {label}" + (f" -- {detail}" if detail and not condition else ""))


class McpClient:
    def __init__(self, env):
        self.proc = subprocess.Popen(
            [str(AISSH_MCP)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            text=True,
            bufsize=1,
        )
        self.next_id = 1
        self.call("initialize", {"protocolVersion": "2025-03-26", "clientInfo": {"name": "e2e"}})

    def call(self, method, params):
        request_id = self.next_id
        self.next_id += 1
        payload = {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
        self.proc.stdin.write(json.dumps(payload) + "\n")
        self.proc.stdin.flush()
        line = self.proc.stdout.readline()
        if not line:
            raise RuntimeError(
                f"no response to {method}; stderr: {self.proc.stderr.read()[:2000]}"
            )
        response = json.loads(line)
        if "error" in response:
            raise RuntimeError(f"{method} failed: {response['error']}")
        return response["result"]

    def tool(self, name, **args):
        result = self.call("tools/call", {"name": name, "arguments": args})
        return result

    def tool_data(self, name, **args):
        """Returns the response body, unwrapping the {kind, data} envelope."""
        result = self.tool(name, **args)
        value = result.get("structuredContent", {})
        if isinstance(value, dict) and "kind" in value and "data" in value:
            return value["data"]
        return value

    def close(self):
        self.proc.stdin.close()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def main():
    port = os.environ["AISSH_TEST_SSH_PORT"]
    key = Path(os.environ["AISSH_TEST_SSH_KEY"]).resolve()
    host = os.environ.get("AISSH_TEST_SSH_HOST", "127.0.0.1")
    user = os.environ.get("AISSH_TEST_SSH_USER", "root")

    home = Path(tempfile.mkdtemp(prefix="aissh-e2e-home."))
    aissh = home / ".aissh"
    for sub in ("keys", "data", "run", "bin"):
        (aissh / sub).mkdir(parents=True, exist_ok=True)
        (aissh / sub).chmod(0o700)

    # A key inside ~/.aissh/keys is the only layout the config accepts.
    shutil.copy(key, aissh / "keys" / "id")
    (aissh / "keys" / "id").chmod(0o600)
    config = (
        "version = 1\n"
        "retention_days = 30\n"
        "idle_timeout_seconds = 1800\n"
        "connect_timeout_seconds = 15\n"
        "keepalive_seconds = 30\n"
        "recording_limit_mib = 500\n"
        "reconnect_attempts = 1\n"
        "reconnect_backoff_seconds = 1\n"
        "quit_daemon_on_app_exit = false\n"
        "launch_at_login = false\n"
        "\n[[targets]]\n"
        'id = "local"\n'
        'name = "Local test server"\n'
        f'host = "{host}"\n'
        f"port = {port}\n"
        f'username = "{user}"\n'
        "\n[targets.auth]\n"
        'type = "private_key"\n'
        'path = "id"\n'
    )
    (aissh / "config.toml").write_text(config)
    (aissh / "config.toml").chmod(0o600)

    env = dict(os.environ)
    env["HOME"] = str(home)
    env["RUST_LOG"] = "warn"

    daemon = subprocess.Popen(
        [str(AISSH_D)],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    socket = aissh / "run" / "aisshd.sock"
    for _ in range(100):
        if socket.exists():
            break
        time.sleep(0.1)
    else:
        print("daemon did not start:", daemon.stderr.read()[:2000])
        return 1

    client = McpClient(env)
    remote_dir = f"/tmp/aissh-e2e-{os.getpid()}"
    local_out = home / "downloaded.bin"
    payload = bytes(range(256)) * 40 + b"\n'quoted' `tick` $VAR \\\n\x00\xff\xfe\n"

    try:
        # --- tool surface -------------------------------------------------
        tools = client.call("tools/list", {})["tools"]
        names = [tool["name"] for tool in tools]
        check("tools/list exposes 21 tools", len(tools) == 21, f"got {len(tools)}")
        for expected in (
            "ssh_file_upload",
            "ssh_file_download",
            "ssh_file_read",
            "ssh_file_write",
            "ssh_file_stat",
            "ssh_file_mkdir",
            "ssh_commands_list",
        ):
            check(f"tool {expected} is registered", expected in names)

        targets = client.tool("ssh_targets_list")["structuredContent"]
        check(
            "targets_list never returns credentials",
            "password" not in json.dumps(targets),
        )
        check(
            "targets_list keeps the {kind, data} envelope",
            targets.get("kind") == "targets" and isinstance(targets.get("data"), list),
        )

        # --- session ------------------------------------------------------
        session = client.tool_data("ssh_session_create", target_id="local", purpose="e2e")
        session_id = session["id"]
        check("session reaches ready", session["status"] == "ready", session["status"])
        check(
            "session reports the host fingerprint",
            bool(session.get("host_fingerprint")),
        )

        # --- file writes, verified and staged -----------------------------
        written = client.tool_data(
            "ssh_file_write",
            session_id=session_id,
            remote_path=f"{remote_dir}/nested/written.txt",
            content="from the MCP tool\n",
            create_dirs=True,
            mode=0o644,
        )
        check("file_write reports a digest", len(written.get("sha256", "")) == 64)
        check("file_write is verified against the remote", written["verified"] is True)
        check("file_write reports changed", written["changed"] is True)

        # The same content again, with if_changed, must be skipped rather than rewritten.
        again = client.tool_data(
            "ssh_file_write",
            session_id=session_id,
            remote_path=f"{remote_dir}/nested/written.txt",
            content="from the MCP tool\n",
            if_changed=True,
        )
        check("if_changed skips an identical write", again["changed"] is False)

        read_back = client.tool_data(
            "ssh_file_read",
            session_id=session_id,
            remote_path=f"{remote_dir}/nested/written.txt",
        )
        check("file_read returns text", read_back.get("text") == "from the MCP tool\n")
        check(
            "file_read omits base64 in compact mode",
            "data_base64" not in read_back,
        )
        check("file_read is not truncated", read_back["truncated"] is False)

        # --- binary round trip through upload/download ---------------------
        source = home / "source.bin"
        source.write_bytes(payload)
        uploaded = client.tool_data(
            "ssh_file_upload",
            session_id=session_id,
            local_path=str(source),
            remote_path=f"{remote_dir}/payload.bin",
            create_dirs=True,
            verify=True,
        )
        check(
            "upload is byte-verified remotely",
            uploaded["verified"] is True and uploaded["bytes"] == len(payload),
            json.dumps(uploaded),
        )

        # Read it back through the tool and confirm the digest is unchanged.
        remote_read = client.tool_data(
            "ssh_file_read",
            session_id=session_id,
            remote_path=f"{remote_dir}/payload.bin",
            encoding="base64",
            max_bytes=len(payload) + 16,
        )
        remote_bytes = base64.b64decode(remote_read["data_base64"])
        check(
            "remote bytes match the local payload exactly",
            remote_bytes == payload,
            f"{len(remote_bytes)} vs {len(payload)} bytes",
        )

        downloaded = client.tool_data(
            "ssh_file_download",
            session_id=session_id,
            remote_path=f"{remote_dir}/payload.bin",
            local_path=str(local_out),
            verify=True,
        )
        check("download is verified", downloaded["verified"] is True)
        check(
            "downloaded file matches byte for byte",
            local_out.read_bytes() == payload,
        )

        second = client.tool(
            "ssh_file_download",
            session_id=session_id,
            remote_path=f"{remote_dir}/payload.bin",
            local_path=str(local_out),
        )
        error = second.get("structuredContent", {})
        check(
            "download refuses to clobber without overwrite",
            error.get("code") == "FILE_EXISTS",
            json.dumps(error),
        )

        # --- stat with a digest -------------------------------------------
        stat = client.tool_data(
            "ssh_file_stat",
            session_id=session_id,
            remote_path=f"{remote_dir}/payload.bin",
            hash=True,
        )
        check("file_stat reports kind and size", stat["kind"] == "file" and stat["size"] == len(payload))
        check(
            "file_stat digest matches the local digest",
            stat["sha256"] == hashlib.sha256(payload).hexdigest(),
        )

        # --- exec, blocking poll, and rendering ---------------------------
        started = client.tool_data(
            "ssh_exec_start",
            session_id=session_id,
            command="sleep 1; printf 'first\\n'; sleep 1; printf 'second\\n'",
        )
        command_id = started["id"]

        began = time.time()
        first_poll = client.tool(
            "ssh_command_poll", command_id=command_id, wait_seconds=30, max_bytes=65536
        )
        elapsed = time.time() - began
        page = first_poll["structuredContent"]
        page_text_value = first_poll["content"][0]["text"]
        check(
            "a blocking poll returns as soon as output arrives",
            page["chunks"] and elapsed < 20,
            f"elapsed={elapsed:.1f}s chunks={page.get('chunks')}",
        )
        check(
            "compact poll omits per-event base64",
            "data_base64" not in json.dumps(page),
        )
        check(
            "compact poll uses chunks rather than repeated per-event metadata",
            "chunks" in page and "events" not in page,
        )
        check(
            "poll reports the truncation levels separately",
            set(page["truncation"]) >= {"page_truncated", "recording_truncated", "live_tail_dropped"},
            json.dumps(page.get("truncation")),
        )
        check(
            "the text channel carries the output of that same response",
            "first" in page_text_value,
            page_text_value[:200],
        )
        check(
            "the text channel is not a JSON re-serialization",
            '"chunks"' not in page_text_value,
        )

        # Drain to completion, accumulating what the pages actually carried.
        sequence = page["next_sequence"]
        streamed = json.dumps(page.get("chunks"))
        for _ in range(20):
            page = client.tool_data(
                "ssh_command_poll",
                command_id=command_id,
                after_sequence=sequence,
                wait_seconds=30,
            )
            streamed += json.dumps(page.get("chunks"))
            sequence = page["next_sequence"]
            if page["poll_complete"] or page["progress"]["timed_out"]:
                break
        check("poll_complete is reached", page["poll_complete"] is True)
        check(
            "the later output arrived through a blocking poll",
            "second" in streamed,
            streamed[:300],
        )
        check("the command completed", page["command"]["status"] == "completed", page["command"]["status"])

        # --- fleet visibility ---------------------------------------------
        listing = client.tool_data("ssh_commands_list", include_finished=True)
        ids = [item["id"] for item in listing["commands"]]
        check("commands_list includes the finished command", command_id in ids)
        check(
            "commands_list exposes previews",
            all("command_preview" in item for item in listing["commands"]),
        )
        running = client.tool_data("ssh_commands_list")
        check(
            "commands_list defaults to running commands only",
            all(item["status"] == "running" for item in running["commands"]),
            json.dumps(running["commands"])[:300],
        )

        # --- git-style end to end: upload a tarball ----------------------
        tarball = home / "deploy_src.tgz"
        subprocess.run(
            ["tar", "czf", str(tarball), "-C", str(REPO), "Cargo.toml", "README.md"],
            check=True,
        )
        tarball_digest = hashlib.sha256(tarball.read_bytes()).hexdigest()
        uploaded = client.tool_data(
            "ssh_file_upload",
            session_id=session_id,
            local_path=str(tarball),
            remote_path=f"{remote_dir}/deploy_src.tgz",
            verify=True,
        )
        check(
            "tarball upload digest matches locally",
            uploaded["sha256"] == tarball_digest,
            f"{uploaded['sha256']} vs {tarball_digest}",
        )
        verify_cmd = client.tool_data(
            "ssh_exec_start",
            session_id=session_id,
            command=f"sha256sum {remote_dir}/deploy_src.tgz",
        )
        verify_id = verify_cmd["id"]
        verify_page = client.tool_data(
            "ssh_command_poll", command_id=verify_id, wait_seconds=30
        )
        remote_digest = verify_page["chunks"][0]["text"].split()[0]
        check(
            "the remote host computes the same digest for the uploaded tarball",
            remote_digest == tarball_digest,
            f"{remote_digest} vs {tarball_digest}",
        )

        # --- error surface ------------------------------------------------
        missing = client.tool_data(
            "ssh_file_read", session_id=session_id, remote_path="/definitely/not/here"
        )
        check(
            "a missing remote file returns a stable code",
            missing.get("code") == "FILE_NOT_FOUND",
            json.dumps(missing),
        )
        bad_wait = client.tool(
            "ssh_command_poll", command_id=command_id, wait_seconds=10**9
        )
        check(
            "an out-of-range wait is clamped rather than rejected",
            "error" not in bad_wait or bad_wait.get("isError") is not True,
        )
        bad_detail = client.tool(
            "ssh_command_poll", command_id=command_id, detail="verbose"
        )
        check(
            "an unknown detail value is rejected",
            bad_detail.get("isError") is True,
        )

        # --- quantified response size, compact vs full ---------------------
        # Mirrors the reported failure: hundreds of separate small writes, which
        # is what a file-hash listing looks like on the wire.
        listing_command = (
            "i=0; while [ $i -lt 338 ]; do "
            "printf '%064d  /etc/hostname\\n' $i; sleep 0.002; i=$((i+1)); done"
        )

        def drain(detail):
            started = client.tool_data("ssh_exec_start", session_id=session_id, command=listing_command)
            # Let the command finish first, so one page carries the whole
            # listing: that is the shape of the reported 408 KB response.
            time.sleep(2.5)
            cursor = 0
            total = 0
            raw = 0
            pages = 0
            while True:
                result = client.tool(
                    "ssh_command_poll",
                    command_id=started["id"],
                    after_sequence=cursor,
                    wait_seconds=30,
                    detail=detail,
                )
                body = result["structuredContent"]
                total += len(json.dumps(body))
                total += len(result["content"][0]["text"])
                raw += sum(len(c.get("text", "").encode()) for c in body.get("chunks", []))
                raw += sum(
                    len(base64.b64decode(e["data_base64"]))
                    for e in body.get("events", [])
                )
                pages += 1
                cursor = body["next_sequence"]
                if body["poll_complete"] or body["progress"]["timed_out"]:
                    return total, raw, pages, body

        compact_bytes, raw_bytes, compact_pages, compact_body = drain("compact")
        full_bytes, _, full_pages, _ = drain("full")
        ratio = full_bytes / max(compact_bytes, 1)
        events_seen = sum(c.get("events", 0) for c in compact_body.get("chunks", []))
        print(
            f"      raw output ~{raw_bytes} bytes across {events_seen} events; "
            f"compact {compact_bytes} bytes in {compact_pages} page(s), "
            f"full {full_bytes} bytes in {full_pages} page(s) -> {ratio:.2f}x smaller"
        )
        check(
            "compact responses are substantially smaller than full ones",
            ratio > 1.5,
            f"compact={compact_bytes} full={full_bytes}",
        )
        check(
            "compact output carries no duplicate base64 payload",
            "data_base64" not in json.dumps(compact_body),
        )
        check(
            "compact never repeats the full command text",
            "command" not in compact_body["command"],
        )

        closed = client.tool_data("ssh_session_close", session_id=session_id)
        check("session closes", closed.get("code") is None)
    finally:
        client.close()
        env2 = dict(env)
        subprocess.run(
            [str(AISSH_MCP)], input="", env=env2, capture_output=True, text=True, timeout=5
        )
        daemon.terminate()
        try:
            daemon.wait(timeout=5)
        except subprocess.TimeoutExpired:
            daemon.kill()
        subprocess.run(
            ["ssh", "-i", str(key), "-o", "StrictHostKeyChecking=no",
             "-o", "UserKnownHostsFile=/dev/null", "-p", port, f"{user}@{host}",
             f"rm -rf {remote_dir}"],
            capture_output=True,
        )
        shutil.rmtree(home, ignore_errors=True)

    failed = [item for item in checks if not item[1]]
    print(f"\n{len(checks) - len(failed)}/{len(checks)} checks passed")
    if failed:
        print("failed:")
        for label, _, detail in failed:
            print(f"  - {label} ({detail})")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
