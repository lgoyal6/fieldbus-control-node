"""HTTP framing and restart telemetry gate, without physical device claims."""
from pathlib import Path
import socket
import subprocess
import tempfile
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen

ROOT = Path(__file__).resolve().parents[1]
subprocess.run(["cargo", "build", "--locked", "-p", "fieldbus-cloud", "-j", "2"], cwd=ROOT, check=True)
with tempfile.TemporaryDirectory() as tmp:
    path = Path(tmp) / "events.log"
    subprocess.run([str(ROOT / "target/debug/fieldbus-cloud"), "simulate", str(path)], check=True)
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    base = f"http://127.0.0.1:{port}"
    def start():
        child = subprocess.Popen([str(ROOT / "target/debug/fieldbus-cloud"), "serve", f"127.0.0.1:{port}", str(path)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(100):
            try:
                with urlopen(base + "/healthz", timeout=1) as response:
                    assert response.status == 200
                return child
            except OSError:
                if child.poll() is not None: raise RuntimeError("cloud failed to start")
                time.sleep(0.02)
        child.terminate(); child.wait(timeout=5)
        raise RuntimeError("cloud readiness timeout")
    def metric(name):
        with urlopen(base + "/metrics", timeout=2) as response:
            text = response.read().decode()
        return float(next(line.split()[1] for line in text.splitlines() if line.startswith(name + " ")))
    child = start()
    try:
        assert metric("fieldbus_readings_stored") == 10
        assert metric("fieldbus_devices_registered") == 1
        assert metric("fieldbus_store_up") == 1
        assert metric("fieldbus_http_duration_seconds_count") >= 1
        # A standard client keeps its write side open while reading the reply.
        # The server must use Content-Length rather than waiting for client EOF.
        request = Request(base + "/ingest", data=b"[]", headers={"Content-Type":"application/json"})
        try:
            urlopen(request, timeout=2)
            raise AssertionError("invalid batch accepted")
        except HTTPError as error:
            assert error.code == 400
        assert metric("fieldbus_http_failures_total") == 1
    finally:
        child.terminate(); child.wait(timeout=5)
    child = start()
    try:
        assert metric("fieldbus_readings_stored") == 10
        assert metric("fieldbus_log_bytes") == path.stat().st_size
    finally:
        child.terminate(); child.wait(timeout=5)
print("HTTP and durable telemetry smoke: PASS")
