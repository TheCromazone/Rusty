#!/usr/bin/env python3
"""Execute a paid-provider-free worker journey and optionally record real wire fixtures."""

import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import queue
import signal
import subprocess
import tempfile
import threading
import time

MAX_FRAME = 4 * 1024 * 1024
MAX_WIRE = 16 * 1024 * 1024
MAX_REQUEST = 1024 * 1024
MAX_DIAGNOSTICS = 64 * 1024
TIMEOUT = 90
FIXED = "def add(a, b):\n    return a + b\n"
CHECK = "from calc import add; assert add(2, 3) == 5; print('test passed')"


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            result.update(block)
    return result.hexdigest()


def tool(name, arguments):
    return {"choices": [{"delta": {"tool_calls": [{
        "index": 0, "id": "call", "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments)},
    }]}, "finish_reason": "tool_calls"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5}}


class FixtureServer(http.server.HTTPServer):
    def __init__(self):
        super().__init__(("127.0.0.1", 0), FixtureHandler)
        self.requests = []
        self.replies = [
            tool("write_file", {"path": "calc.py", "content": FIXED}),
            tool("bash", {"command": 'python3 -c "' + CHECK + '"'}),
            {"choices": [{"delta": {"content": "Fixed addition and executed the check."},
                          "finish_reason": "stop"}],
             "usage": {"prompt_tokens": 10, "completion_tokens": 8}},
        ]

    def get_request(self):
        connection, address = super().get_request()
        connection.settimeout(2)
        return connection, address


class FixtureHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        if not 0 < length <= MAX_REQUEST or len(self.server.requests) >= 3:
            self.send_error(413)
            return
        body = self.rfile.read(length)
        if len(body) != length:
            self.send_error(400)
            return
        request = json.loads(body)
        index = len(self.server.requests)
        self.server.requests.append(request)
        response = ("data: " + json.dumps(self.server.replies[index])
                    + "\n\ndata: [DONE]\n\n").encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(response)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(response)


class Worker:
    def __init__(self, binary, arguments, root, environment, deadline):
        self.deadline = deadline
        self.frames = queue.Queue(maxsize=16)
        self.records = []
        self.diagnostics = bytearray()
        self.stop = threading.Event()
        self.process = subprocess.Popen(
            [str(binary), *arguments], cwd=root, env=environment,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            start_new_session=True,
        )
        self.threads = [threading.Thread(target=self.read_frames, daemon=True),
                        threading.Thread(target=self.read_diagnostics, daemon=True)]
        for thread in self.threads:
            thread.start()

    def put(self, item):
        while not self.stop.is_set():
            try:
                self.frames.put(item, timeout=0.1)
                return
            except queue.Full:
                continue

    def read_frames(self):
        total = 0
        try:
            while not self.stop.is_set():
                line = self.process.stdout.readline(MAX_FRAME + 1)
                if not line:
                    break
                total += len(line)
                if len(line) > MAX_FRAME or not line.endswith(b"\n") or total > MAX_WIRE:
                    raise RuntimeError("worker wire exceeded its frame/total budget")
                self.put(json.loads(line))
        except Exception as error:
            self.put(error)
        finally:
            self.put(None)

    def read_diagnostics(self):
        while True:
            block = self.process.stderr.read(8192)
            if not block:
                return
            self.diagnostics.extend(block)
            if len(self.diagnostics) > MAX_DIAGNOSTICS:
                del self.diagnostics[:-MAX_DIAGNOSTICS]

    def send(self, request_id, method, params):
        frame = json.dumps({"protocol": 1, "request_id": request_id,
                            "method": method, "params": params}).encode() + b"\n"
        if len(frame) > MAX_FRAME:
            raise RuntimeError("outgoing frame exceeds budget")
        self.process.stdin.write(frame)
        self.process.stdin.flush()

    def next(self, allow_eof=False):
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("journey deadline exceeded")
        event = self.frames.get(timeout=remaining)
        if isinstance(event, Exception):
            raise event
        if event is None:
            if allow_eof:
                return None
            raise RuntimeError("worker closed before journey completed")
        self.records.append(event)
        if len(self.records) > 4096:
            raise RuntimeError("worker event count exceeds budget")
        return event

    def until(self, predicate):
        while True:
            event = self.next()
            if predicate(event):
                return event

    def shutdown(self):
        self.send("journey-shutdown", "shutdown", {})
        self.process.stdin.close()
        while self.next(allow_eof=True) is not None:
            pass
        if self.process.wait(timeout=max(0.1, self.deadline - time.monotonic())) != 0:
            raise RuntimeError("worker exited unsuccessfully")

    def close(self):
        self.stop.set()
        # A child can retain a pipe after the worker exits. Reap our private
        # process group as well as the direct process, even on failure.
        try:
            os.killpg(self.process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            self.process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            os.killpg(self.process.pid, signal.SIGKILL)
            self.process.wait(timeout=2)
        for thread in self.threads:
            thread.join(timeout=3)
        if any(thread.is_alive() for thread in self.threads):
            raise RuntimeError("worker reader thread did not stop")
        for stream in [self.process.stdin, self.process.stdout, self.process.stderr]:
            if not stream.closed:
                stream.close()


def write_frames(path, events):
    path.write_text("".join(json.dumps(event, ensure_ascii=False) + "\n" for event in events))


def normalize(value, temporary):
    if isinstance(value, str):
        return value.replace(temporary, "<TEMP>")
    if isinstance(value, list):
        return [normalize(item, temporary) for item in value]
    if isinstance(value, dict):
        return {key: normalize(item, temporary) for key, item in value.items()}
    return value


def run(binary, output, record_fixture):
    start = time.monotonic()
    deadline = start + TIMEOUT
    source = Path(__file__).resolve().parent.parent
    receipt = {"status": "failed", "binary": str(binary), "binary_sha256": digest(binary),
               "script_sha256": digest(Path(__file__)), "paid_requests": 0,
               "provider": "local scripted HTTP fixture only",
               "scope": "Stage 0 worker journey; not native-app E2E or live-provider qualification",
               "source_sha256": {str(path): digest(source / path) for path in
                                 ["src/main.rs", "src/protocol.rs", "src/session_worker.rs", "src/inspect.rs"]}}
    receipt["source_head"] = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=source, timeout=5, text=True).strip()
    receipt["source_tracked_dirty"] = bool(subprocess.check_output(
        ["git", "status", "--porcelain", "--untracked-files=no"], cwd=source, text=True).strip())
    server = FixtureServer()
    server_thread = threading.Thread(target=server.serve_forever)
    server_thread.start()
    workers = []
    temporary_directory = tempfile.TemporaryDirectory(prefix="rusty-native-journey-")
    temporary = temporary_directory.name
    try:
        root, home = Path(temporary) / "repo", Path(temporary) / "home"
        root.mkdir()
        home.mkdir()
        (root / "calc.py").write_text("def add(a, b):\n    return a - b\n")
        environment = {"PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "HOME": str(home),
                       "RUSTY_HOME": str(home), "RUSTY_NO_DOTENV": "1",
                       "NVIDIA_API_KEY": "offline-fixture-value", "NO_COLOR": "1",
                       "RUSTY_BASE_URL": f"http://127.0.0.1:{server.server_port}/v1"}
        for arguments in [["init", "-q"], ["add", "."],
                          ["-c", "user.email=test@example.invalid", "-c", "user.name=Fixture",
                           "commit", "-qm", "fixture"]]:
            subprocess.run(["git", *arguments], cwd=root, env=environment, check=True, timeout=5,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        worker_start = time.monotonic()
        worker = Worker(binary, ["--session-worker", "--protocol-stdio", "--memory", "off",
                                "--mode", "standard", "--agents", "off", "--permissions", "ask",
                                "--context-tokens", "128000", "--max-requests", "3"], root, environment, deadline)
        workers.append(worker)
        worker.until(lambda event: event["kind"] == "hello")
        receipt["hello_ms"] = round((time.monotonic() - worker_start) * 1000, 2)
        worker.until(lambda event: event["kind"] == "session.ready")
        worker.send("journey-1", "message.submit", {"text": "Fix addition and run a local assertion."})
        approvals = 0
        while True:
            event = worker.next()
            if event["kind"] == "approval.requested":
                approvals += 1
                payload = event["payload"]
                worker.send(f"approve-{approvals}", "approval.decide", {
                    "approval_id": payload["approval_id"], "operation_digest": payload["operation_digest"],
                    "allowed": True, "typed": payload.get("typed_phrase"),
                })
            if event["kind"] == "run.completed":
                if event["payload"]["state"] != "completed":
                    raise RuntimeError("fixture run did not complete")
                break
        if (root / "calc.py").read_text() != FIXED or len(server.requests) != 3 or approvals != 2:
            raise RuntimeError("edit/request/approval journey assertions failed")
        worker.send("inspect-on-session", "inspect", {"operation": "file.read", "arguments": {"path": "calc.py"}})
        rejected = worker.until(lambda event: event["kind"] == "reply" and
                                event["payload"]["request_id"] == "inspect-on-session")
        if rejected["payload"]["ok"] is not False:
            raise RuntimeError("session channel admitted inspection")
        inspector = Worker(binary, ["--inspect-worker", "--protocol-stdio"], root,
                           {"PATH": environment["PATH"], "HOME": str(home)}, deadline)
        workers.append(inspector)
        inspector.send("diff", "inspect", {"operation": "diff.hunks", "arguments": {"path": "calc.py"}})
        inspected = inspector.until(lambda event: event["kind"] == "reply" and
                                    event["payload"]["request_id"] == "diff")
        if inspected["payload"]["ok"] is not True:
            raise RuntimeError("provider-free inspector rejected real edit")
        subprocess.run(["/usr/bin/python3", "-c", CHECK], cwd=root, env=environment,
                       check=True, timeout=5, stdout=subprocess.DEVNULL)
        inspector.shutdown()
        worker.shutdown()
        write_frames(output / "journey.ndjson", worker.records)
        write_frames(output / "inspection.ndjson", inspector.records)
        if record_fixture:
            record_fixture.parent.mkdir(parents=True, exist_ok=True)
            write_frames(record_fixture, normalize(worker.records, temporary))
            receipt["recorded_fixture"] = str(record_fixture)
            receipt["fixture_normalization"] = "temporary directory strings replaced by <TEMP>; all rows are executed wire events"
        receipt.update(status="passed", fixture_model_requests=len(server.requests),
                       approval_round_trips=approvals, edited_file=FIXED,
                       assertion="executed Python add(2,3)==5", event_count=len(worker.records))
    except Exception as error:
        receipt["error"] = str(error)
        raise
    finally:
        cleanup_errors = []
        for index, worker in enumerate(workers):
            try:
                worker.close()
            except Exception as error:
                cleanup_errors.append(str(error))
            name = "worker" if index == 0 else "inspect"
            write_frames(output / (name + "-partial.ndjson"), worker.records)
            (output / (name + "-diagnostics.txt")).write_bytes(worker.diagnostics)
        try:
            server.shutdown()
            server.server_close()
            server_thread.join(timeout=3)
            if server_thread.is_alive():
                cleanup_errors.append("fixture server thread did not stop")
        except Exception as error:
            cleanup_errors.append(str(error))
        try:
            temporary_directory.cleanup()
        except Exception as error:
            cleanup_errors.append(str(error))
        if cleanup_errors:
            receipt["status"] = "failed"
            receipt["cleanup_errors"] = cleanup_errors
        receipt["fixture_model_requests"] = len(server.requests)
        receipt["source_sha256_after"] = {name: digest(source / name) for name in receipt["source_sha256"]}
        receipt["elapsed_seconds"] = round(time.monotonic() - start, 3)
        (output / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
        if cleanup_errors:
            raise RuntimeError("journey cleanup failed: " + "; ".join(cleanup_errors))
    print(json.dumps(receipt, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--record-fixture", type=Path)
    arguments = parser.parse_args()
    output = arguments.out.resolve()
    output.mkdir(parents=True, exist_ok=False)
    run(arguments.binary.resolve(), output,
        arguments.record_fixture.resolve() if arguments.record_fixture else None)


if __name__ == "__main__":
    main()
