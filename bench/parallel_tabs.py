#!/usr/bin/env python3
"""How many agent flows can run at once on one Chrome, and what they cost.

One daemon, one Chromium, one leased tab per test: the flow an agent test
is (open a tab, navigate, read the snapshot, act on a ref, read the text,
close the tab). The concurrency is swept and the machine is sampled while it
runs, so the number that saturates -- and what saturates it -- is measured
rather than guessed.

    python3 bench/parallel_tabs.py --levels 5,10,20,35,50 --waves 2

A note on the errors: a navigation that lands while the browser is busy with
several others comes back as `net::ERR_ABORTED` -- the browser drops the
load, and cu reports it as a failed navigation. That is what the browser
does under load (the same happens with the binary before the speed pass);
`first_error` in the output says what the failures were.
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

PAGE = b"""<!doctype html><title>parallel</title><h1>Parallel target</h1>
<form><input name=q><button>Search</button></form>
<ol><li>one</li><li>two</li><li>three</li><li>four</li><li>five</li>
<li>six</li><li>seven</li><li>eight</li><li>nine</li><li>ten</li></ol>
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
        # segment, or every navigation waits for the peer's delayed ACK.
        head = (
            "HTTP/1.1 200 OK\r\n"
            "Content-Type: text/html\r\n"
            f"Content-Length: {len(PAGE)}\r\n"
            "Connection: keep-alive\r\n\r\n"
        ).encode()
        self.wfile.write(head + PAGE)


class Daemon:
    def __init__(self, binary):
        self.binary = os.path.abspath(binary)
        self.dir = tempfile.mkdtemp(prefix="cu-par-")
        self.port, self.cdp = free_port(), free_port()
        self.token = None
        self.proc = None

    def start(self):
        env = dict(os.environ, CU_DATA_DIR=self.dir, CU_CDP_PORT=str(self.cdp))
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
                break
            except Exception:
                time.sleep(0.01)
        else:
            raise SystemExit("cu never came up")
        # The daemon answers before its browser is up; wait for the browser,
        # or the first wave would measure the start-up and call it an error.
        deadline = time.time() + 60
        while time.time() < deadline:
            try:
                self.call("POST", "/v1/navigate", {"url": "about:blank"})
                return
            except Exception:
                time.sleep(0.05)
        raise SystemExit("cu's browser never came up")

    def call(self, method, path, body=None, timeout=60):
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(
            "http://127.0.0.1:%d%s" % (self.port, path), data=data, method=method,
            headers={"Authorization": "Bearer " + self.token,
                     "Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.read()

    def stop(self):
        if self.proc:
            self.proc.terminate()
            try:
                self.proc.wait(10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        shutil.rmtree(self.dir, ignore_errors=True)


def tree_cpu(root):
    """CPU seconds used by a process tree, from /proc."""
    total = 0.0
    pids = [root]
    while pids:
        pid = pids.pop()
        try:
            with open("/proc/%d/stat" % pid) as f:
                fields = f.read().rsplit(")", 1)[1].split()
            total += (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")
            with open("/proc/%d/task/%d/children" % (pid, pid)) as f:
                pids.extend(int(c) for c in f.read().split())
        except (OSError, IndexError, ValueError):
            continue
    return total


def tree_rss(root):
    total = 0
    pids = [root]
    while pids:
        pid = pids.pop()
        try:
            with open("/proc/%d/statm" % pid) as f:
                total += int(f.read().split()[1]) * os.sysconf("SC_PAGE_SIZE")
            with open("/proc/%d/task/%d/children" % (pid, pid)) as f:
                pids.extend(int(c) for c in f.read().split())
        except (OSError, IndexError, ValueError):
            continue
    return total


def flow(daemon, page, label):
    """One agent test: a tab of its own, a page, a read, an action, a read."""
    started = time.perf_counter()
    # The tab opens blank and the flow waits a moment before navigating: a
    # navigation that lands while the tab's own first load is still in flight
    # aborts that load, and the browser reports the abort (ERR_ABORTED).
    tab = json.loads(daemon.call("POST", "/v1/tabs",
                                 {"lease": 120, "label": label}))["id"]
    try:
        time.sleep(0.05)
        daemon.call("POST", "/v1/navigate", {"url": page, "tab": tab})
        snapshot = json.loads(daemon.call("GET", "/v1/snapshot?tab=" + tab))["snapshot"]
        ref = None
        for line in snapshot.splitlines():
            if "ref=" in line:
                ref = line.split("ref=")[1].split()[0].split(",")[0]
                break
        if ref:
            daemon.call("POST", "/v1/act", {"actions": [{"do": "click", "ref": ref}],
                                         "tab": tab})
        text = json.loads(daemon.call("GET", "/v1/text?tab=" + tab))["text"]
        return (time.perf_counter() - started) * 1000, ("one" in text or "Parallel" in text)
    finally:
        try:
            daemon.call("DELETE", "/v1/tabs/" + tab)
        except Exception:
            pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default="target/release/cu")
    ap.add_argument("--levels", default="5,10,20,35,50")
    ap.add_argument("--waves", type=int, default=2)
    ap.add_argument("--out")
    args = ap.parse_args()

    site = ThreadingHTTPServer(("127.0.0.1", 0), Site)
    site.disable_nagle_algorithm = True
    threading.Thread(target=site.serve_forever, daemon=True).start()
    page = "http://127.0.0.1:%d/" % site.server_address[1]

    daemon = Daemon(args.bin)
    daemon.start()
    rows = []
    try:
        for level in [int(n) for n in args.levels.split(",")]:
            for wave in range(args.waves):
                results = []
                errors = []
                samples = []
                stop = threading.Event()

                def sampler():
                    while not stop.is_set():
                        samples.append((tree_cpu(daemon.proc.pid), tree_rss(daemon.proc.pid)))
                        time.sleep(0.25)

                watcher = threading.Thread(target=sampler, daemon=True)
                watcher.start()
                started = time.perf_counter()
                threads = []
                for i in range(level):
                    def one(i=i):
                        try:
                            results.append(flow(daemon, page, "parallel-%d" % i))
                        except Exception as e:
                            errors.append(str(e))
                    t = threading.Thread(target=one)
                    threads.append(t)
                for t in threads:
                    t.start()
                for t in threads:
                    t.join()
                wall = time.perf_counter() - started
                stop.set()
                watcher.join()
                times = [r[0] for r in results]
                rows.append({
                    "level": level, "wave": wave + 1,
                    "tests": len(results), "errors": len(errors),
                    "first_error": errors[0] if errors else None,
                    "tests_per_s": round(len(results) / wall, 1),
                    "p50": round(statistics.median(times), 3) if times else None,
                    "p95": round(sorted(times)[int(len(times) * 0.95) - 1], 3) if times else None,
                    "cu_cpu": round(max((c for c, _ in samples), default=0.0), 2),
                    "cu_rss_mb": round(max((r for _, r in samples), default=0) / 1e6, 1),
                })
                print(json.dumps(rows[-1]), flush=True)
    finally:
        daemon.stop()
        site.shutdown()
    print(json.dumps({"rows": rows}, indent=2))
    if args.out:
        json.dump({"rows": rows}, open(args.out, "w"), indent=2)


if __name__ == "__main__":
    sys.exit(main())
