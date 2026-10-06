#!/usr/bin/env python3
"""Parallel-tab benchmark: one Chrome, many leased tabs, real agent flows.

One `cu` daemon (one Chromium) is started fresh. Each simulated test runs the
real agent flow against a locally served page: tab open (leased) -> navigate ->
snapshot -> act (type + click by ref) -> text check -> tab close. Concurrency
levels are swept while /proc is sampled for CPU and RAM; then the headline run
(50 tests at once) is repeated and reported.

Usage: python3 bench/parallel_tabs.py [--cu BIN] [--levels 5,10,20,35,50]
Writes /tmp/cu-bench-results.json and prints the same summary.
"""
import argparse
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

CARDS = "".join(
    f'<div class="card"><h3>Item {i}</h3><p>{"lorem ipsum dolor sit amet " * 3}</p></div>'
    for i in range(300))
PAGE = (f"""<!doctype html><html><head><title>bench</title></head><body>
<h1>Order form</h1>
<label>Qty <input id="qty" name="qty"></label>
<button id="go">Place order</button>
<div id="out">idle</div>
<main>{CARDS}</main>
<script>
// a little ongoing work, like a real app
let tick = 0;
setInterval(() => {{ tick += 1; document.title = 'bench ' + tick; }}, 500);
document.getElementById('go').onclick = () => {{
  document.getElementById('out').textContent =
    'ordered ' + (document.getElementById('qty').value || '0') + ' items';
}};
</script></body></html>""").encode()


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(PAGE)))
        self.end_headers()
        self.wfile.write(PAGE)

    def log_message(self, *a):
        pass


def proc_cpu(pid):
    try:
        fields = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
        return int(fields[11]) + int(fields[12])  # utime +stime, ticks
    except Exception:
        return 0


def proc_rss_kb(pid):
    try:
        return int(open(f"/proc/{pid}/statm").read().split()[1]) * 4
    except Exception:
        return 0


def system_cpu():
    f = open("/proc/stat").readline().split()
    vals = list(map(int, f[1:8]))
    total = sum(vals)
    idle = vals[3] + (vals[4] if len(vals) > 4 else 0)
    return total, idle


def pids_matching(pattern):
    out = []
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            cmd = open(f"/proc/{pid}/cmdline", "rb").read().decode("utf-8", "replace")
        except Exception:
            continue
        if pattern in cmd:
            out.append(int(pid))
    return out


class Sampler:
    def __init__(self, group_pattern, cu_pid):
        self.pattern = group_pattern
        self.cu_pid = cu_pid
        self.samples = []
        self.stop = False

    def run(self):
        t0 = sys_cpu0 = None
        while not self.stop:
            chrome = [p for p in pids_matching(self.pattern) if p != self.cu_pid]
            st, si = system_cpu()
            c0 = {p: proc_cpu(p) for p in chrome}
            cu0 = proc_cpu(self.cu_pid)
            st0, si0 = st, si
            time.sleep(0.5)
            chrome = [p for p in pids_matching(self.pattern) if p != self.cu_pid]
            st, si = system_cpu()
            chrome_ticks = sum(max(0, proc_cpu(p) - c0.get(p, proc_cpu(p))) for p in chrome)
            cu_ticks = max(0, proc_cpu(self.cu_pid) - cu0)
            dt = (st - st0) or 1
            busy = 100.0 * (1 - (si - si0) / dt)
            hz = os.sysconf("SC_CLK_TCK")
            self.samples.append({
                "t": time.time(),
                "busy_pct": round(busy, 1),
                "chrome_cores": round(chrome_ticks / dt, 2),
                "cu_cores": round(cu_ticks / dt, 2),
                "chrome_rss_mb": round(sum(proc_rss_kb(p) for p in chrome) / 1024, 1),
                "chrome_procs": len(chrome),
                "cu_rss_mb": round(proc_rss_kb(self.cu_pid) / 1024, 1),
                "load1": os.getloadavg()[0],
            })

    def peak(self):
        if not self.samples:
            return {}
        keys = ["busy_pct", "chrome_cores", "cu_cores", "chrome_rss_mb", "chrome_procs", "cu_rss_mb", "load1"]
        out = {}
        for k in keys:
            vals = [s[k] for s in self.samples]
            out[k + "_max"] = max(vals)
            out[k + "_avg"] = round(sum(vals) / len(vals), 2)
        return out


def cu(cu_bin, data, args, timeout=60):
    env = dict(os.environ, CU_DATA_DIR=data)
    r = subprocess.run([cu_bin] + args, capture_output=True, text=True, timeout=timeout, env=env)
    return r.stdout.strip(), r.returncode


def one_flow(cu_bin, data, base):
    t0 = time.time()
    steps = []
    out, rc = cu(cu_bin, data, ["tab", "open", f"{base}/", "--lease", "120", "--label", "bench"])
    if rc != 0:
        return time.time() - t0, f"tab open failed: {out}"
    tab = json.loads(out)["tab"]
    try:
        # tab open returns before the page loads; navigate waits for it to settle
        out, rc = cu(cu_bin, data, ["navigate", f"{base}/", "--tab", tab])
        if rc != 0:
            return time.time() - t0, f"navigate failed: {out[:120]}"
        out, rc = cu(cu_bin, data, ["snapshot", "--tab", tab])
        if rc != 0:
            return time.time() - t0, f"snapshot failed: {out}"
        try:
            out = json.loads(out).get("snapshot", out)
        except Exception:
            pass
        ref_in = ref_btn = None
        for line in out.splitlines():
            m = re.search(r"ref=(e\d+)", line)
            if not m:
                continue
            if "textbox" in line and ref_in is None:
                ref_in = m.group(1)
            elif "button" in line and ref_btn is None:
                ref_btn = m.group(1)
        if not (ref_in and ref_btn):
            return time.time() - t0, f"no refs in snapshot: {out[:120]}"
        out, rc = cu(cu_bin, data, ["act", json.dumps([
            {"do": "type", "ref": ref_in, "text": "7"},
            {"do": "click", "ref": ref_btn},
        ]), "--tab", tab])
        if rc != 0:
            return time.time() - t0, f"act failed: {out[:120]}"
        out, rc = cu(cu_bin, data, ["text", "--tab", tab])
        if rc != 0 or "ordered 7 items" not in out:
            return time.time() - t0, f"text check failed: {out[:120]}"
    finally:
        cu(cu_bin, data, ["tab", "close", tab])
    return time.time() - t0, None


def burst(cu_bin, data, base, concurrency, repeats, wave=0):
    """Launch `concurrency` flows at once, `repeats` waves; return latencies/errors.

    Each flow navigates to its own loopback host (127.0.0.x) so every tab gets
    its own renderer process -- what per-test tabs cost on a real multi-site
    run. Chromium keys its process model on the site, not the tab.
    """
    lat, errs = [], []
    for r in range(repeats):
        results = [None] * concurrency

        def worker(i):
            host = f"127.0.0.{2 + (wave * concurrency + r * concurrency + i) % 250}"
            results[i] = one_flow(cu_bin, data, base.replace("127.0.0.1", host))

        ts = [threading.Thread(target=worker, args=(i,)) for i in range(concurrency)]
        t0 = time.time()
        for t in ts:
            t.start()
        for t in ts:
            t.join()
        wall = time.time() - t0
        for r in results:
            if r is None:
                continue
            if r[1]:
                errs.append(r[1])
            else:
                lat.append(r[0])
        time.sleep(0.5)
    return lat, errs


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cu", default=os.path.expanduser("~/.local/bin/cu"))
    ap.add_argument("--levels", default="5,10,20,35,50")
    ap.add_argument("--waves", type=int, default=2)
    ap.add_argument("--http-port", type=int, default=8899)
    ap.add_argument("--cu-port", type=int, default=8795)
    ap.add_argument("--cdp-port", type=int, default=43997)
    ap.add_argument("--out", default="/tmp/cu-bench-results.json")
    args = ap.parse_args()

    data = "/tmp/cu-bench-data"
    shutil.rmtree(data, ignore_errors=True)
    os.makedirs(data)

    httpd = ThreadingHTTPServer(("0.0.0.0", args.http_port), Handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()

    env = dict(os.environ, CU_DATA_DIR=data, CU_BROWSER="/snap/bin/chromium",
               CU_CDP_PORT=str(args.cdp_port))
    daemon = subprocess.Popen(
        [args.cu, "start", "--port", str(args.cu_port), "--data", data],
        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    base = f"http://127.0.0.1:{args.http_port}"
    for _ in range(60):
        out, rc = cu(args.cu, data, ["status"])
        if rc == 0 and '"running":true' in out:
            break
        time.sleep(0.5)
    else:
        sys.exit("daemon did not come up")

    group = f"--user-data-dir={data}"
    sampler = Sampler(group, daemon.pid)
    threading.Thread(target=sampler.run, daemon=True).start()
    time.sleep(1.5)

    # Warmup: one flow so Chrome is fully up before measuring.
    one_flow(args.cu, data, base)

    results = {"levels": [], "headline": None, "machine": {
        "cpus": os.cpu_count(), "load1_before": os.getloadavg()[0],
        "mem_total_mb": round(os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES") / 1e6),
    }}

    for wave_index, level in enumerate([int(x) for x in args.levels.split(",")]):
        sampler.samples.clear()
        t0 = time.time()
        lat, errs = burst(args.cu, data, base, level, args.waves, wave=wave_index)
        wall = time.time() - t0
        lat_sorted = sorted(lat)
        p = lambda q: round(lat_sorted[min(len(lat_sorted) - 1, int(q * len(lat_sorted)))], 2) if lat_sorted else None
        results["levels"].append({
            "concurrency": level,
            "tests": len(lat),
            "errors": len(errs),
            "wall_s": round(wall, 1),
            "lat_p50_s": p(0.5), "lat_p95_s": p(0.95),
            "tests_per_s": round(len(lat) / wall, 2) if wall else None,
            **sampler.peak(),
        })

    # Headline: 50 tests in parallel, one Chrome, distinct tabs.
    sampler.samples.clear()
    t0 = time.time()
    lat, errs = burst(args.cu, data, base, 50, 1, wave=9)
    wall = time.time() - t0
    lat_sorted = sorted(lat)
    results["headline"] = {
        "tests": len(lat), "errors": len(errs), "wall_s": round(wall, 1),
        "lat_p50_s": round(lat_sorted[len(lat_sorted) // 2], 2) if lat_sorted else None,
        "lat_max_s": round(lat_sorted[-1], 2) if lat_sorted else None,
        **sampler.peak(),
    }
    results["machine"]["load1_after"] = os.getloadavg()[0]

    open(args.out, "w").write(json.dumps(results, indent=1))
    print(json.dumps(results, indent=1))
    daemon.send_signal(signal.SIGTERM)
    time.sleep(2)
    shutil.rmtree(data, ignore_errors=True)


if __name__ == "__main__":
    main()
