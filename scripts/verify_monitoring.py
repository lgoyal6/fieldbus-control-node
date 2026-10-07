#!/usr/bin/env python3
"""Build the local monitoring stack and verify durable metrics reach Prometheus."""
import json
import os
from pathlib import Path
import subprocess
import time
from urllib.parse import urlencode
from urllib.request import urlopen
import uuid

repo = Path(__file__).resolve().parents[1]
project = "fieldbus-gate-" + uuid.uuid4().hex[:10]
env = {**os.environ, "FIELDBUS_CLOUD_PORT": "0", "FIELDBUS_PROMETHEUS_PORT": "0", "FIELDBUS_GRAFANA_PORT": "0"}
command = ["docker", "compose", "-p", project, "-f", str(repo / "infra/compose.yml")]

def compose(*args):
    return subprocess.check_output([*command, *args], env=env, text=True).strip()

def port(service, internal):
    return int(compose("port", service, str(internal)).rsplit(":", 1)[1])

def get(url):
    with urlopen(url, timeout=3) as response:
        return json.loads(response.read())

def wait(check, description):
    for _ in range(40):
        try:
            if check():
                return
        except (OSError, ValueError):
            pass
        time.sleep(2)
    raise RuntimeError(description)

try:
    compose("build", "cloud")
    compose("run", "--rm", "--no-deps", "--entrypoint", "/usr/local/bin/fieldbus-cloud", "cloud", "simulate", "/var/lib/fieldbus/events.log")
    compose("up", "-d")
    cloud = f"http://127.0.0.1:{port('cloud', 8080)}"
    prometheus = f"http://127.0.0.1:{port('prometheus', 9090)}"
    grafana = f"http://127.0.0.1:{port('grafana', 3000)}"
    wait(lambda: get(cloud + "/healthz")["status"] == "ok", "cloud readiness")
    def samples():
        return get(prometheus + "/api/v1/query?" + urlencode({"query": 'fieldbus_readings_stored{job="fieldbus-cloud"}'}))["data"]["result"]
    wait(lambda: samples() and float(samples()[0]["value"][1]) == 10, "Prometheus never scraped durable readings")
    def rules_ready():
        groups = get(prometheus + "/api/v1/rules")["data"]["groups"]
        rules = [rule for group in groups for rule in group["rules"]]
        return len(rules) == 5 and all(rule["health"] == "ok" for rule in rules)
    wait(rules_ready, "Prometheus rules never evaluated successfully")
    wait(lambda: get(grafana + "/api/health")["database"] == "ok", "Grafana readiness")
    wait(lambda: get(grafana + "/api/dashboards/uid/fieldbus-cloud")["dashboard"]["title"] == "Fieldbus cloud reference", "dashboard provisioning")
    compose("restart", "cloud")
    cloud = f"http://127.0.0.1:{port('cloud', 8080)}"
    wait(lambda: get(cloud + "/healthz")["status"] == "ok", "cloud restart readiness")
    with urlopen(cloud + "/metrics", timeout=3) as response:
        metrics = response.read().decode()
    assert "fieldbus_readings_stored 10" in metrics
    assert "fieldbus_store_up 1" in metrics
    print("Monitoring verified: durable metrics scraped, rules loaded, dashboard provisioned, cloud restarted")
except Exception:
    subprocess.run([*command, "logs", "--tail", "30"], env=env, check=False)
    raise
finally:
    subprocess.run([*command, "down", "-v", "--remove-orphans", "--rmi", "local"], env=env, check=False)
