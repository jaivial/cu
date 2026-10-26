#!/usr/bin/env python3
"""Time every cu tool the way an agent drives it.

Measures the daemon's HTTP API and the CLI (one process per action, the way
an agent calls it) for each tool: navigate, snapshot, screenshot, click, text
and a batch.

With `--compare A,B` the two binaries are run alternately, action by action,
so machine load falls on both alike; the report then carries the ratio.

Usage:
  python3 bench/cu_speed.py --iters 20
  python3 bench/cu_speed.py --compare /tmp/cu-baseline,target/release/cu --iters 20
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

BROWSER = os.environ.get("CU_BROWSER") or shutil.which("chromium") or shutil.which("google-chrome")

PAGE = b"""<!doctype html><title>bench</title><h1>Bench target</h1>
<ul id=links><li><a href="/one">one</a><li><a href="/two">two</a>
<li><button id=go>Go</button></ul>
<form><input name=q><button>Search</button></form>
<p>Some content so the DOM is not trivially small.</p>"""


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


class Site(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def do_GET(self):
        # One write, and no Nagle: the head and the body go out in a single
        # segment. Written in pieces they waited for the peer's delayed ACK,
        # which added 40 ms to every navigation -- more than the browser's
        # own work, and the same for every tool under test.
        body = PAGE
        head = (
            "HTTP/1.1 200 OK\r\n"
            "Content-Type: text/html\r\n"
            f"Content-Length: {len(body)}\r\n"
            "Connection: keep-alive\r\n\r\n"
        ).encode()
        self.wfile.write(head + body)


def pct(values, p):
    values = sorted(values)
    if not values:
        return 0.0
    i = min(len(values) - 1, int(round(p / 100 * (len(values) - 1))))
    return values[i]


class Daemon:
    """One cu daemon under benchmark, with its browser and data dir."""

    def __init__(self, binary, site_port):
        self.binary = os.path.abspath(binary)
        self.dir = tempfile.mkdtemp(prefix="cu-speed-")
        self.port, self.cdp_port = free_port(), free_port()
        self.token = None
        self.proc = None
        self.site_port = site_port

    def start(self):
        env = dict(os.environ, CU_DATA_DIR=self.dir, CU_CDP_PORT=str(self.cdp_port))
        if BROWSER:
            env["CU_BROWSER"] = BROWSER
        self.proc = subprocess.Popen(
            [self.binary, "start", "--port", str(self.port), "--data", self.dir],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.time() + 60
        while time.time() < deadline:
            try:
                self.token = json.load(open(os.path.join(self.dir, "server.json")))["token"]
                self.call("GET", "/v1/status")
                return
            except Exception:
                if self.proc.poll() is not None:
                    raise SystemExit("cu start exited early")
                time.sleep(0.005)
        raise SystemExit("cu never came up")

    def call(self, method, path, body=None, timeout=30):
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(
            "http://127.0.0.1:%d%s" % (self.port, path), data=data, method=method,
            headers={"Authorization": "Bearer " + self.token,
                     "Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.read()

    def cli(self, *args):
        env = dict(os.environ, CU_DATA_DIR=self.dir)
        subprocess.run([self.binary, *args], env=env, capture_output=True)

    def stop(self):
        if self.proc:
            self.proc.terminate()
            try:
                self.proc.wait(5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        shutil.rmtree(self.dir, ignore_errors=True)


def actions(daemon, page):
    """The tools to time, as (name, callable) pairs."""
    snapshot = json.loads(daemon.call("GET", "/v1/snapshot"))["snapshot"]
    refs = [line.split("ref=")[1].split()[0].split(",")[0] for line in snapshot.splitlines() if "ref=" in line]
    ref = refs[0]
    # The first ref is a link, and a link navigates: the click then waits for
    # the new page, which is the browser's work rather than cu's. The button
    # is the click that does not navigate.
    quiet = refs[2] if len(refs) > 2 else ref
    return [
        ("navigate", lambda: daemon.call("POST", "/v1/navigate", {"url": page})),
        ("snapshot", lambda: daemon.call("GET", "/v1/snapshot")),
        ("shot(jpeg)", lambda: daemon.call("GET", "/v1/screenshot")),
        ("shot(png)", lambda: daemon.call("GET", "/v1/screenshot?format=png")),
        ("click", lambda: daemon.call("POST", "/v1/click", {"ref": quiet})),
        ("click(link)", lambda: daemon.call("POST", "/v1/click", {"ref": ref})),
        ("text", lambda: daemon.call("GET", "/v1/text")),
        ("act(click)", lambda: daemon.call("POST", "/v1/act",
                                          {"actions": [{"do": "click", "ref": quiet}]})),
        ("cli snapshot", lambda: daemon.cli("snapshot")),
        ("cli navigate", lambda: daemon.cli("navigate", page)),
        ("cli click", lambda: daemon.cli("click", ref)),
    ]


def measure(daemon, page, iters):
    """Median and p95 of every tool, in milliseconds."""
    out = {}
    for name, fn in actions(daemon, page):
        for _ in range(min(3, iters)):
            fn()
        samples = []
        for _ in range(iters):
            t0 = time.perf_counter()
            fn()
            samples.append((time.perf_counter() - t0) * 1000)
        out[name] = {"median": round(statistics.median(samples), 2),
                     "min": round(min(samples), 2),
                     "p95": round(pct(samples, 95), 2)}
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--compare", default=None,
                    help="two binaries to compare, comma separated")
    ap.add_argument("--iters", type=int, default=20)
    ap.add_argument("--out")
    args = ap.parse_args()

    site = ThreadingHTTPServer(("127.0.0.1", 0), Site)
    site.disable_nagle_algorithm = True
    threading.Thread(target=site.serve_forever, daemon=True).start()
    page = "http://127.0.0.1:%d/" % site.server_address[1]
    binaries = args.compare.split(",") if args.compare else ["target/release/cu"]
    results = {"environment": {"browser": BROWSER, "iters": args.iters,
                               "date": time.strftime("%Y-%m-%dT%H:%M:%S%z")},
               "runs": []}
    try:
        if len(binaries) == 1:
            daemon = Daemon(binaries[0], site.server_address[1])
            daemon.start()
            try:
                for _ in range(200):
                    try:
                        daemon.call("POST", "/v1/navigate", {"url": page})
                        break
                    except Exception:
                        time.sleep(0.05)
                results["runs"].append({"binary": binaries[0],
                                        "actions": measure(daemon, page, args.iters)})
            finally:
                daemon.stop()
        else:
            # Alternating rounds: both binaries see the same machine load.
            daemons = []
            for binary in binaries:
                d = Daemon(binary, site.server_address[1])
                d.start()
                for _ in range(200):
                    try:
                        d.call("POST", "/v1/navigate", {"url": page})
                        break
                    except Exception:
                        time.sleep(0.05)
                daemons.append(d)
            try:
                samples = {binary: {} for binary in binaries}
                for binary, d in zip(binaries, daemons):
                    for name, _ in actions(d, page):
                        samples[binary][name] = []
                # Every sample of an action is taken right after the other
                # binary's sample of the same action, so a slow patch of
                # machine load falls on both alike.
                rounds = max(2, args.iters)
                for _ in range(rounds):
                    for name, _ in actions(daemons[0], page):
                        for binary, d in zip(binaries, daemons):
                            fn = dict(actions(d, page))[name]
                            fn()  # warm
                            t0 = time.perf_counter()
                            fn()
                            samples[binary][name].append((time.perf_counter() - t0) * 1000)
                for binary in binaries:
                    results["runs"].append({
                        "binary": binary,
                        "actions": {name: {"median": round(statistics.median(v), 2),
                                           "min": round(min(v), 2),
                                           "p95": round(pct(v, 95), 2)}
                                    for name, v in samples[binary].items()}})
            finally:
                for d in daemons:
                    d.stop()
    finally:
        site.shutdown()

    if len(results["runs"]) == 2:
        a, b = results["runs"]
        print("%-16s %9s %9s %8s %9s %9s %8s" % (
            "action", "A med", "B med", "ratio", "A min", "B min", "ratio"))
        for name in a["actions"]:
            ma, mb = a["actions"][name]["median"], b["actions"][name]["median"]
            na, nb = a["actions"][name]["min"], b["actions"][name]["min"]
            print("%-16s %9.2f %9.2f %7.2fx %9.2f %9.2f %7.2fx" % (
                name, ma, mb, ma / mb if mb else 0, na, nb, na / nb if nb else 0))
        results["ratio"] = {name: round(a["actions"][name]["median"] / b["actions"][name]["median"], 2)
                            for name in a["actions"]}
    print(json.dumps(results, indent=2))
    if args.out:
        json.dump(results, open(args.out, "w"), indent=2)


if __name__ == "__main__":
    sys.exit(main())
