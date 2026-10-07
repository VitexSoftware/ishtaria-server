#!/usr/bin/env python3
"""A small load test for an Ishtaria server: simulated players register and then walk, look around,
poll events, chat and ask for their profile for a while; the tool reports throughput and latency.

Python standard library only. Run it against a server that you own and that is NOT your live world
(it creates accounts), and raise the limits of that server first, because all simulated players come from
one address:

    [limits]
    auth_per_minute = 100000
    requests_per_second = 100000
    burst = 100000

    tools/loadtest.py --url http://127.0.0.1:7411 --players 30 --seconds 30
"""
import argparse
import json
import math
import random
import statistics
import threading
import time
import urllib.error
import urllib.request


def call(base, method, path, token=None, body=None, timeout=15):
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(base + path, data=data, method=method)
    request.add_header("Content-Type", "application/json")
    if token:
        request.add_header("Authorization", "Bearer " + token)
    started = time.perf_counter()
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            payload = response.read()
            status = response.status
    except urllib.error.HTTPError as error:
        payload, status = error.read(), error.code
    except Exception:  # connection refused, timeout, ...
        return 0, None, (time.perf_counter() - started) * 1000
    elapsed = (time.perf_counter() - started) * 1000
    try:
        parsed = json.loads(payload) if payload else None
    except ValueError:
        parsed = None
    return status, parsed, elapsed


class Stats:
    def __init__(self):
        self.lock = threading.Lock()
        self.latency = {}
        self.status = {}

    def add(self, label, status, elapsed):
        with self.lock:
            self.latency.setdefault(label, []).append(elapsed)
            key = "2xx" if 200 <= status < 300 else ("429" if status == 429 else ("4xx" if 400 <= status < 500 else "5xx/none"))
            self.status[key] = self.status.get(key, 0) + 1


def percentile(values, share):
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, math.ceil(share * len(ordered)) - 1)]


def player(base, number, run, deadline, stats, think):
    name = f"load{run}_{number}"
    for _ in range(40):  # the server hashes a few passwords at a time and may answer 429 while busy
        status, reply, elapsed = call(base, "POST", "/players", body={"username": name, "password": "load-test-pass"})
        stats.add("register", status, elapsed)
        if status != 429:
            break
        time.sleep(random.uniform(0.1, 0.5))
    if status != 201 or not reply:
        return
    token = reply["token"]
    sequence = 0
    heading = random.uniform(0, 2 * math.pi)
    while time.time() < deadline:
        roll = random.random()
        if roll < 0.55:
            sequence += 1
            heading += random.uniform(-0.3, 0.3)
            body = {"direction": [math.cos(heading), 0.0, math.sin(heading)], "sequence": str(sequence), "run": random.random() < 0.3}
            status, _, elapsed = call(base, "POST", "/players/me/move", token, body)
            stats.add("move", status, elapsed)
        elif roll < 0.70:
            status, _, elapsed = call(base, "GET", "/players/nearby", token)
            stats.add("nearby", status, elapsed)
        elif roll < 0.82:
            status, _, elapsed = call(base, "GET", "/events?after=0", token)
            stats.add("events", status, elapsed)
        elif roll < 0.92:
            status, _, elapsed = call(base, "GET", "/players/me", token)
            stats.add("profile", status, elapsed)
        elif roll < 0.97:
            status, _, elapsed = call(base, "POST", "/chat/say", token, {"text": "hello from the load test"})
            stats.add("chat", status, elapsed)
        else:
            status, _, elapsed = call(base, "GET", "/world/placed?x=0&y=0&z=6371000", token)
            stats.add("placed", status, elapsed)
        time.sleep(think / 1000 * random.uniform(0.5, 1.5))


def main():
    parser = argparse.ArgumentParser(description="Load test of an Ishtaria server (creates accounts!)")
    parser.add_argument("--url", required=True, help="base URL, for example http://127.0.0.1:7411")
    parser.add_argument("--players", type=int, default=20)
    parser.add_argument("--seconds", type=int, default=30)
    parser.add_argument("--think-ms", type=int, default=100, help="mean pause between the requests of one player")
    arguments = parser.parse_args()
    base = arguments.url.rstrip("/")
    status, _, _ = call(base, "GET", "/health")
    if status != 200:
        raise SystemExit(f"{base}/health answered {status}: is the server running?")
    stats = Stats()
    run = int(time.time()) % 100000
    deadline = time.time() + arguments.seconds
    started = time.time()
    threads = [threading.Thread(target=player, args=(base, i, run, deadline, stats, arguments.think_ms)) for i in range(arguments.players)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    seconds = max(time.time() - started, 0.001)
    total = sum(len(values) for values in stats.latency.values())
    print(f"{arguments.players} players, {seconds:.1f} s, {total} requests, {total / seconds:.0f} requests/s")
    print("answers:", ", ".join(f"{key}={count}" for key, count in sorted(stats.status.items())))
    print(f"{'request':10} {'count':>7} {'mean':>8} {'p50':>8} {'p95':>8} {'p99':>8} {'max':>8}   (ms)")
    for label, values in sorted(stats.latency.items()):
        print(f"{label:10} {len(values):7d} {statistics.mean(values):8.1f} {percentile(values, .50):8.1f} {percentile(values, .95):8.1f} {percentile(values, .99):8.1f} {max(values):8.1f}")


if __name__ == "__main__":
    main()
