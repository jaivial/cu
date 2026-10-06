#!/usr/bin/env python3
"""Benchmark the `cu` agent-browser round trips against a real Chromium.

Measures the four latencies an agent feels most:

  start      cold daemon start, browser up and answering DevTools
  navigate   POST /v1/navigate until the page is usable
  shot       GET /v1/screenshot until bytes are on disk
  snapshot   GET /v1/snapshot (compact DOM for the LLM)

Everything is driven over the real loopback HTTP API, so the numbers include
the JSON parse, the CDP bridge and the process hand-off, exactly what an agent
pays per action. Pages are served from a local HTTP server so no run depends
on the network.

Usage: bench/cu_bench.py [--bin PATH] [--iters N] [--out FILE]
"""
import argparse
import json
import os
import shutil
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

BROWSER = shutil.which("chromium") or shutil.which("chromium-browser") \
    or shutil.which("google-chrome-stable") or shutil.which("google-chrome")
TOKEN = "bench-token"

PAGE = b"""<!doctype html><title>bench</title><h1>Bench target</h1>
<ul id=links><li><a href="/one">one</a><li><a href="/two">two</a>
<li><button id=go>Go</button></ul>
<form><input name=q><button>Search</button></form>
<p>Some content so the DOM is not trivially small.</p>"""
SSE_PAGE = b"""<!doctype html><title>sse</title><body>
<script>var es = new EventSource('/stream');es.onmessage=function(e){};</script>
<h1>SSE keeps a connection open</h1><p>Loaded.</p>"""


class Site(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _send(self, body, ctype="text/html", code=200):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/stream":  # never ends: blocks a networkidle wait
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", "0")
            self.end_headers()
            try:
                while True:
                    time.sleep(1)
                    self.wfile.write(b": ping\n\n")
            except OSError:
                pass
            return
        self._send({"%2F": PAGE}.get(self.path.replace("/", "%2F"),
                                    SSE_PAGE if "sse" in self.path else PAGE))


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


class Cu:
    """One daemon under benchmark, plus its browser and data dir."""

    def __init__(self, binary, workdir, port, cdp_port):
        self.binary, self.dir = binary, workdir
        self.token = TOKEN
        self.port, self.cdp_port = port, cdp_port
        self.proc = None

    def start(self):
        env = dict(os.environ, CU_BROWSER=BROWSER, CU_CDP_PORT=str(self.cdp_port),
                   CU_DATA_DIR=self.dir)
        t0 = time.perf_counter()
        self.proc = subprocess.Popen(
            [self.binary, "start", "--port", str(self.port), "--data", self.dir],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        while True:
            try:
                self.token = json.load(open(os.path.join(self.dir, "server.json")))["token"]
                self.call("GET", "/v1/status")
                break
            except OSError:
                if self.proc.poll() is not None:
                    raise SystemExit("cu start exited early")
                if time.perf_counter() - t0 > 60:
                    raise SystemExit("cu never came up")
                time.sleep(0.002)
        return (time.perf_counter() - t0) * 1000

    def url(self, path):
        return "http://127.0.0.1:%d%s" % (self.port, path)

    def call(self, method, path, body=None, timeout=30):
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(
            self.url(path), data=data, method=method,
            headers={"Authorization": "Bearer " + self.token,
                     "Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.read()

    def stop(self):
        if self.proc:
            self.proc.terminate()
            try:
                self.proc.wait(5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        subprocess.run(["pkill", "-f", "--", "--user-data-dir=%s/profiles" % self.dir],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def pct(values, p):
    values = sorted(values)
    if not values:
        return 0.0
    i = min(len(values) - 1, int(round(p / 100 * (len(values) - 1))))
    return values[i]


def bench(name, iters, fn):
    """Runs fn iters times, then a few warmup-discarded extra rounds."""
    for _ in range(min(3, iters)):
        fn()
    samples = []
    for _ in range(iters):
        t0 = time.perf_counter()
        fn()
        samples.append((time.perf_counter() - t0) * 1000)
    return {
        "action": name, "n": iters,
        "min": round(min(samples), 2), "median": round(statistics.median(samples), 2),
        "p95": round(pct(samples, 95), 2), "max": round(max(samples), 2),
        "mean": round(statistics.fmean(samples), 2),
    }


def bench_parallel(cu, iters, workers=8):
    """Wall clock for `iters` shots issued concurrently by `workers` threads."""
    barrier = threading.Barrier(workers)
    out = []

    def work():
        barrier.wait()
        t0 = time.perf_counter()
        cu.call("GET", "/v1/screenshot")
        out.append((time.perf_counter() - t0) * 1000)

    for _ in range(iters):
        threads = [threading.Thread(target=work) for _ in range(workers)]
        t0 = time.perf_counter()
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        wall = (time.perf_counter() - t0) * 1000
    return {"action": "shot x%d parallel" % workers, "n": iters,
            "min": round(min(out), 2), "median": round(statistics.median(out), 2),
            "p95": round(pct(out, 95), 2), "max": round(max(out), 2),
            "mean": round(statistics.fmean(out), 2),
            "wall_ms": round(wall, 2)}


def has_snapshot(cu):
    try:
        cu.call("GET", "/v1/snapshot", timeout=10)
        return True
    except Exception as e:
        code = getattr(getattr(e, "code", None), "__int__", lambda: 0)() if e else 0
        return False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default="target/release/cu")
    ap.add_argument("--iters", type=int, default=25)
    ap.add_argument("--out", default=None)
    args = ap.parse_args()

    workdir = tempfile.mkdtemp(prefix="cu-bench-")
    site = ThreadingHTTPServer(("127.0.0.1", 0), Site)
    threading.Thread(target=site.serve_forever, daemon=True).start()
    page = "http://127.0.0.1:%d/" % site.server_address[1]

    cu = Cu(os.path.abspath(args.bin), workdir, free_port(), free_port())
    results = {"environment": {"binary": args.bin, "browser": BROWSER,
                               "pages": page, "iters": args.iters,
                               "date": time.strftime("%Y-%m-%dT%H:%M:%S%z")}}
    try:
        results["start_ms"] = round(cu.start(), 2)
        cu.call("POST", "/v1/navigate", {"url": page})
        results["actions"] = [
            bench("navigate", args.iters,
                  lambda: cu.call("POST", "/v1/navigate", {"url": page})),
            bench("shot(jpeg)", args.iters,
                  lambda: cu.call("GET", "/v1/screenshot")),
            bench("shot(png)", args.iters,
                  lambda: cu.call("GET", "/v1/screenshot?format=png")),
        ]
        # Compaction: what the LLM is sent instead of the DOM it would have to
        # read. The bulk page has 180 interactive nodes.
        body = cu.call("GET", "/v1/snapshot").decode()
        results["snapshot_bytes"] = len(body)
        results["page_bytes"] = len(PAGE)
        results["compaction"] = round(len(body) / len(PAGE), 3)
        if results["snapshot_supported"]:
            results["actions"].insert(
                1, bench("snapshot", args.iters, lambda: cu.call("GET", "/v1/snapshot")))
        results["actions"].append(bench_parallel(cu, max(1, args.iters // 5)))
    finally:
        cu.stop()
        site.shutdown()
        shutil.rmtree(workdir, ignore_errors=True)

    print(json.dumps(results, indent=2))
    if args.out:
        with open(args.out, "w") as f:
            json.dump(results, f, indent=2)
    return 0


if __name__ == "__main__":
    sys.exit(main())
