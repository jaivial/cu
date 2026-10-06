#!/usr/bin/env python3
"""Real-site benchmark: cu agent flows against the deployed neural-dev app.

Unlike bench/parallel_tabs.py (local pages, synthetic flow), this drives the
real deployed app end to end: login, routes and subroutes, and a Sage chat turn
whose answer streams back over SSE -- which is why the flow waits for text and
page state instead of networkidle.

Modes:
  levels     sweep concurrency (--levels 1,10,25; one tab per flow)
  block      same flows with images/fonts/media blocked in the test tab (CDP)
  probe      text wait vs networkidle around the Sage SSE
  snapshots  repeated snapshot cost (baseline for incremental snapshots)
  ab         the same flows driven by agent-browser (default level 10)

Every flow: context open (isolated cookie jar, so the login is real) ->
navigate a deep route -> login if the app says so -> route via the mobile menu
-> subroute tab -> open the Sage drawer -> new conversation -> send a marker
message -> wait until the marker appears twice (echo + Sage's answer) -> close.
Steps are timed; the Sage fraction is sage_wait / flow total.

Usage: python3 bench/real_flows.py --mode levels --levels 1,10,25 --waves 5,2,1
Writes /tmp/real-bench-<mode>.json and prints the same summary.
"""
import argparse
import json
import os
import random
import re
import shutil
import statistics
import subprocess
import sys
import threading
import time
import urllib.request

HOME = os.path.expanduser("~")
DEFAULT_BASE = "https://neural-dev.menustudioai.com"
DEFAULT_ORG = "01a0ee1d-a360-737f-b6ba-9eeb22d337a5"


def qaspec_credentials():
    """base_url, user, password from ~/qaspec-neural/qaspec.toml + app-password."""
    base, user, pw_file = DEFAULT_BASE, "test@hotmail.com", "~/.config/qaspec/app-password"
    toml = os.path.join(HOME, "qaspec-neural", "qaspec.toml")
    if os.path.exists(toml):
        text = open(toml).read()
        m = re.search(r'base_url\s*=\s*"([^"]+)"', text)
        if m:
            base = m.group(1)
        m = re.search(r'username\s*=\s*"([^"]+)"', text)
        if m:
            user = m.group(1)
        m = re.search(r'password\s*=\s*\{\s*file\s*=\s*"([^"]+)"', text)
        if m:
            pw_file = m.group(1)
    password = open(os.path.expanduser(pw_file)).read().strip()
    return base.rstrip("/"), user, password


# ---------------------------------------------------------------- process data

def proc_cpu(pid):
    try:
        fields = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
        return int(fields[11]) + int(fields[12])  # utime + stime, ticks
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


def daemon_pid():
    """The shared `cu start` daemon (plain `cu start`, default port)."""
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            argv = open(f"/proc/{pid}/cmdline", "rb").read().decode().split("\0")
        except Exception:
            continue
        argv = [a for a in argv if a]
        if len(argv) == 2 and argv[0].endswith("/cu") and argv[1] == "start":
            return int(pid)
    return None


class Sampler:
    """CPU/RAM of the Chrome tree (sum of processes) and of the daemon."""

    def __init__(self, chrome_pattern, daemon):
        self.pattern = chrome_pattern
        self.daemon = daemon
        self.samples = []
        self.stop = False

    def run(self):
        hz = os.sysconf("SC_CLK_TCK")
        while not self.stop:
            chrome = pids_matching(self.pattern)
            st0, si0 = system_cpu()
            c0 = {p: proc_cpu(p) for p in chrome}
            d0 = proc_cpu(self.daemon) if self.daemon else 0
            time.sleep(0.5)
            chrome = pids_matching(self.pattern)
            st, si = system_cpu()
            chrome_ticks = sum(max(0, proc_cpu(p) - c0.get(p, 0)) for p in chrome)
            daemon_ticks = max(0, (proc_cpu(self.daemon) if self.daemon else 0) - d0)
            dt = (st - st0) or 1
            busy = 100.0 * (1 - (si - si0) / dt)
            self.samples.append({
                "t": time.time(),
                "busy_pct": round(busy, 1),
                "chrome_cores": round(chrome_ticks / dt, 2),
                "daemon_cores": round(daemon_ticks / dt, 2),
                "chrome_rss_mb": round(sum(proc_rss_kb(p) for p in chrome) / 1024, 1),
                "chrome_procs": len(chrome),
                "daemon_rss_mb": round(proc_rss_kb(self.daemon) / 1024, 1) if self.daemon else 0,
                "load1": os.getloadavg()[0],
            })

    def peak(self):
        if not self.samples:
            return {}
        keys = ["busy_pct", "chrome_cores", "daemon_cores", "chrome_rss_mb",
                "chrome_procs", "daemon_rss_mb", "load1"]
        out = {}
        for k in keys:
            vals = [s[k] for s in self.samples]
            out[k + "_max"] = max(vals)
            out[k + "_avg"] = round(sum(vals) / len(vals), 2)
        return out


# --------------------------------------------------------------------- cu CLI

def cu(args, timeout=8):
    # a CDP command sent across a client-side navigation can hang (its reply is
    # lost when the page swaps execution context) and the daemon waits with no
    # deadline; an agent retries, so the harness retries before failing the flow
    for attempt in range(3):
        try:
            r = subprocess.run([os.path.join(HOME, ".local/bin", "cu")] + args,
                               capture_output=True, text=True, timeout=timeout)
            return r.stdout.strip(), r.returncode
        except subprocess.TimeoutExpired:
            log(f"hang (attempt {attempt + 1}): cu {' '.join(args[:2])}")
            time.sleep(0.5)
    raise RuntimeError(f"cu {' '.join(args[:2])} hung 3x")


def cu_json(args, timeout=60):
    out, rc = cu(args, timeout)
    if rc != 0:
        raise RuntimeError(f"cu {' '.join(args[:2])} failed: {out[:200]}")
    return json.loads(out) if out.startswith("{") else {}


def act_ok(ctx_arg, actions):
    """Run a batch and fail unless every action really happened."""
    out, rc = cu(["act", json.dumps(actions)] + ctx_arg)
    if rc != 0 or '"ok":false' in out.replace(" ", ""):
        raise RuntimeError(f"act failed: {out[:160]}")


def snapshot(ctx_arg):
    out, rc = cu(["snapshot"] + ctx_arg)
    if rc != 0:
        raise RuntimeError(f"snapshot failed: {out[:200]}")
    return json.loads(out).get("snapshot", out)


def page_text(ctx_arg):
    out, rc = cu(["text"] + ctx_arg)
    if rc != 0:
        raise RuntimeError(f"text failed: {out[:200]}")
    return json.loads(out).get("text", out)


def snap_url(snap):
    m = re.search(r"^- url: (.+)$", snap, re.M)
    return m.group(1) if m else ""


def snap_has(snap, role, name):
    """Presence match, including name-only lines (headings carry no ref)."""
    for line in snap.splitlines():
        m = re.match(r'- (?:ref=(e\d+) )?(\w+) "(.*)"$', line.strip())
        if m and m.group(2) == role and m.group(3).startswith(name):
            return True
    return False


def snap_ref(snap, role, name):
    """First ref whose role matches and whose name equals/prefixes `name`."""
    for line in snap.splitlines():
        m = re.match(r'- ref=(e\d+) (\w+) "(.*)"$', line.strip())
        if not m:
            continue
        ref, got_role, got_name = m.groups()
        if got_role == role and got_name.startswith(name):
            return ref
    return None


def snap_ref_re(snap, role, name_re):
    for line in snap.splitlines():
        m = re.match(r'- ref=(e\d+) (\w+) "(.*)"$', line.strip())
        if not m:
            continue
        ref, got_role, got_name = m.groups()
        if got_role == role and re.search(name_re, got_name):
            return ref
    return None


def log(msg):
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", file=sys.stderr, flush=True)


def click_ref(ctx_arg, role, name, tries=8, regex=False, settle=0.0):
    """Snapshot -> ref -> click, retrying when a re-render killed the ref.

    A React re-render can drop the page-side registry entry between the
    snapshot and the click ("could not locate eN"); the tool says "take a new
    snapshot", so the flow does exactly that. When the page re-renders on a
    timer (live dashboards), the ref is only clickable within one render
    window: read it, and click only if it survived a second read.
    """
    last = ""
    for attempt in range(tries):
        if settle and attempt == 0:
            time.sleep(settle)
        snap = snapshot(ctx_arg)
        ref = snap_ref_re(snap, role, name) if regex else snap_ref(snap, role, name)
        if not ref:
            time.sleep(0.3)
            continue
        out, rc = cu(["act", json.dumps([{"do": "click", "ref": ref}])] + ctx_arg)
        if rc == 0 and '"ok":true' in out.replace(" ", ""):
            return
        last = out[:120]
        time.sleep(0.25)
    raise RuntimeError(f"click {role} {name!r} failed: {last}")


def wait_until(fn, timeout, interval=0.0):
    """Poll fn() until it returns a truthy value; return (value, waited_s).

    Exceptions inside fn() count as "not yet": one flaky read must not fail a
    wait whose next read would have been fine.
    """
    t0 = time.time()
    val = None
    while time.time() - t0 < timeout:
        try:
            val = fn()
        except Exception as e:
            log(f"wait read failed ({type(e).__name__}: {str(e)[:80]})")
        if val:
            return val, time.time() - t0
        if interval:
            time.sleep(interval)
    return None, time.time() - t0


# ------------------------------------------------------------------ the flow

class Flow:
    """One simulated test against neural-dev, in its own tab/context."""

    def __init__(self, i, base, org, user, password, tab_mode="context",
                 block_media=False, sage_timeout=180):
        self.i = i
        self.base = base
        self.org = org
        self.user = user
        self.password = password
        self.tab_mode = tab_mode  # "context" (own cookie jar) or "tab"
        self.block_media = block_media
        self.sage_timeout = sage_timeout
        self.marker = "BN" + "".join(random.choice("ABCDEFGHJKLMNPQRSTUVWXYZ23456789") for _ in range(5))
        self.steps = {}
        self.error = None
        self.cdp = None

    def ctx(self):
        return ["--context", self.name] if self.tab_mode == "context" else ["--tab", self.tab]

    def open_page(self):
        if self.tab_mode == "context":
            self.name = f"bench{self.i}x{os.getpid() % 10000}"
            out = cu_json(["context", "open", self.name, "--lease", "900", "--label", "bench"])
            self.tab = out["tab"]
        else:
            self.name = None
            out = cu_json(["tab", "open", "about:blank", "--lease", "900", "--label", "bench"])
            self.tab = out["tab"]

    def close_page(self):
        if self.tab_mode == "context":
            cu(["context", "close", self.name], timeout=30)
        else:
            cu(["tab", "close", self.tab], timeout=30)

    def run(self):
        t_flow = time.time()
        try:
            t = time.time()
            self.open_page()
            self.steps["open"] = round(time.time() - t, 3)
            log(f"flow{self.i}: open {self.steps['open']}s")

            if self.block_media:
                self.cdp = Blocker(self.tab)

            # deep route: analytics of the org
            t = time.time()
            cu_json(["navigate", f"{self.base}/{self.org}/analytics"] + self.ctx(), timeout=60)
            time.sleep(0.5)  # the app then redirects client-side (to login if fresh)
            self.steps["navigate"] = round(time.time() - t, 3)
            log(f"flow{self.i}: navigate {self.steps['navigate']}s")

            # the SPA redirects to /auth/login a moment after settle; wait for
            # a definite state (login form or app content) before branching
            snap, _ = wait_until(
                lambda: (lambda s: s if ("/auth/login" in snap_url(s)
                         or snap_has(s, "heading", "Analytics")) else None)(snapshot(self.ctx())),
                60)
            if not snap:
                probe = ""
                try:
                    probe = snapshot(self.ctx())[:220].replace("\n", " | ")
                except Exception:
                    pass
                raise RuntimeError(f"page state never definite; last: {probe}")
            log(f"flow{self.i}: definite state url={snap_url(snap)[:70]}")

            # login if the app says we have no session (state: url != /auth/login)
            t = time.time()
            self.steps["login"] = 0.0
            if "/auth/login" in snap_url(snap):
                r = snap_ref(snap, "textbox", "Email") or snap_ref(snap, "textbox", "Enter your email")
                p = snap_ref(snap, "textbox", "Password")
                b = snap_ref(snap, "button", "Sign In")
                act_ok(self.ctx(), [
                    {"do": "type", "ref": r, "text": self.user},
                    {"do": "type", "ref": p, "text": self.password},
                    {"do": "click", "ref": b},
                ])
                log(f"flow{self.i}: login submitted")
                ok, _ = wait_until(lambda: "/auth/login" not in snap_url(snapshot(self.ctx())), 120)
                if not ok:
                    raise RuntimeError("login did not leave /auth/login in 120s")
                self.steps["login"] = round(time.time() - t, 3)
                log(f"flow{self.i}: logged in, url now {snap_url(snapshot(self.ctx()))[:70]}")

            # app ready: the analytics heading is on screen
            ok, _ = wait_until(
                lambda: snap_has(snapshot(self.ctx()), "heading", "Analytics"), 60)
            if not ok:
                raise RuntimeError(f"analytics did not render (last url {snap_url(snapshot(self.ctx()))[:70]})")
            self.steps["ready"] = round(time.time() - t, 3) - self.steps["login"]
            log(f"flow{self.i}: login {self.steps['login']}s ready {self.steps['ready']}s")

            # route: menu -> Articles
            t = time.time()
            click_ref(self.ctx(), "button", r"^$", regex=True)  # unnamed menu button
            ok, _ = wait_until(lambda: snap_ref(snapshot(self.ctx()), "link", "Articles"), 10)
            if not ok:
                raise RuntimeError("menu did not open")
            click_ref(self.ctx(), "link", "Articles", settle=0.6)
            ok, _ = wait_until(lambda: "/articles" in snap_url(snapshot(self.ctx())), 30)
            if not ok:
                raise RuntimeError("articles route did not load")
            self.steps["route"] = round(time.time() - t, 3)
            log(f"flow{self.i}: route {self.steps['route']}s")

            # subroute: the Drafts tab (?tab=drafts)
            t = time.time()
            click_ref(self.ctx(), "tab", "Drafts")
            ok, _ = wait_until(lambda: "tab=drafts" in snap_url(snapshot(self.ctx())), 30)
            if not ok:
                raise RuntimeError("drafts subroute did not load")
            self.steps["subroute"] = round(time.time() - t, 3)
            log(f"flow{self.i}: subroute {self.steps['subroute']}s")

            # Sage: menu -> SAGE -> composer
            t = time.time()
            click_ref(self.ctx(), "button", r"^$", regex=True)
            ok, _ = wait_until(lambda: snap_ref_re(snapshot(self.ctx()), "button", r"^SAGE$"), 10)
            if not ok:
                raise RuntimeError("menu did not offer SAGE")
            click_ref(self.ctx(), "button", r"^SAGE$", regex=True, settle=0.6)
            ok, _ = wait_until(lambda: snap_ref(snapshot(self.ctx()), "textbox", "Ask SAGE"), 30)
            if not ok:
                raise RuntimeError("Sage composer did not open")
            self.steps["sage_open"] = round(time.time() - t, 3)
            log(f"flow{self.i}: sage_open {self.steps['sage_open']}s")

            # new conversation, then send the marker message
            snap = snapshot(self.ctx())
            newc = snap_ref(snap, "button", "New conversation")
            if newc:
                cu(["act", json.dumps([{"do": "click", "ref": newc}])] + self.ctx())
                time.sleep(0.4)
                snap = snapshot(self.ctx())
            box = snap_ref(snap, "textbox", "Ask SAGE")
            msg = f"Say exactly: {self.marker} and nothing else."
            t = time.time()
            # type + Enter: clicking "Send" after typing hits a dead ref, the
            # composer re-renders on every keystroke and the click says
            # "could not locate". Enter submits from the textarea itself.
            act_ok(self.ctx(), [
                {"do": "type", "ref": box, "text": msg},
                {"do": "press", "key": "Enter"},
            ])
            self.steps["sage_send"] = round(time.time() - t, 3)
            log(f"flow{self.i}: sage_send {self.steps['sage_send']}s marker {self.marker}")

            # wait for the answer by TEXT/STATE: the marker in the page twice
            # (the echo of the user message + Sage's streamed reply).
            def answered():
                return page_text(self.ctx()).count(self.marker) >= 2

            ok, waited = wait_until(answered, self.sage_timeout)
            if not ok:
                raise RuntimeError(f"no Sage answer in {self.sage_timeout}s")
            self.steps["sage_wait"] = round(waited, 3)
            log(f"flow{self.i}: sage_wait {self.steps['sage_wait']}s")

            t = time.time()
            self.close_page()
            self.steps["close"] = round(time.time() - t, 3)
        except Exception as e:
            self.error = f"{type(e).__name__}: {e}"
            log(f"flow{self.i}: FAIL {self.error}")
            try:
                self.close_page()
            except Exception:
                pass
        finally:
            if self.cdp:
                self.cdp.close()
        self.steps["total"] = round(time.time() - t_flow, 3)
        if not self.error:
            self.steps["sage_fraction"] = round(self.steps.get("sage_wait", 0) / self.steps["total"], 3)
        return self


class Blocker:
    """Block images/fonts/media in one tab over CDP (a lever cu could expose)."""

    PATTERNS = ["*.png*", "*.jpg*", "*.jpeg*", "*.gif*", "*.webp*", "*.avif*",
                "*.svg*", "*.ico*", "*.woff*", "*.ttf*", "*.otf*", "*.eot*",
                "*.mp4*", "*.webm*", "*.mp3*", "*.ogg*", "*.wav*", "*.m4a*"]

    def __init__(self, tab_id):
        import websockets.sync.client as wc
        ts = json.load(urllib.request.urlopen("http://127.0.0.1:9222/json"))
        t = next(x for x in ts if x["id"] == tab_id)
        self.ws = wc.connect(t["webSocketDebuggerUrl"], open_timeout=10)
        self.mid = 0
        self.send("Network.enable")
        self.send("Network.setBlockedURLs", {"urls": self.PATTERNS})

    def send(self, method, params=None):
        self.mid += 1
        self.ws.send(json.dumps({"id": self.mid, "method": method, "params": params or {}}))
        while True:
            msg = json.loads(self.ws.recv())
            if msg.get("id") == self.mid:
                return msg

    def close(self):
        try:
            self.ws.close()
        except Exception:
            pass


# ------------------------------------------------------------------ stats

def pct(vals, q):
    if not vals:
        return None
    s = sorted(vals)
    return round(s[min(len(s) - 1, int(q * len(s)))], 2)


def burst(base, org, user, password, level, waves, tab_mode="context",
          block_media=False, sage_timeout=180):
    flows = []
    for w in range(waves):
        results = [None] * level

        def worker(i, w=w):
            results[i] = Flow(w * level + i, base, org, user, password,
                              tab_mode=tab_mode, block_media=block_media,
                              sage_timeout=sage_timeout).run()

        ts = [threading.Thread(target=worker, args=(i,)) for i in range(level)]
        t0 = time.time()
        for t in ts:
            t.start()
        for t in ts:
            t.join()
        flows.extend(results)
        time.sleep(1.0)
    return flows


def summarize(flows):
    ok = [f for f in flows if not f.error]
    errs = [f for f in flows if f.error]
    out = {
        "flows": len(flows),
        "ok": len(ok),
        "errors": len(errs),
        "error_samples": [f.error for f in errs[:5]],
    }
    for step in ["open", "navigate", "login", "ready", "route", "subroute",
                 "sage_open", "sage_send", "sage_wait", "close", "total", "sage_fraction"]:
        vals = [f.steps[step] for f in ok if step in f.steps]
        if vals:
            out[step + "_p50"] = pct(vals, 0.5)
            out[step + "_p95"] = pct(vals, 0.95)
    return out


# ------------------------------------------------------- probe (text vs idle)

class NetProbe:
    """Track in-flight network requests of one tab over CDP, SSE included."""

    def __init__(self, tab_id):
        import websockets.sync.client as wc
        ts = json.load(urllib.request.urlopen("http://127.0.0.1:9222/json"))
        t = next(x for x in ts if x["id"] == tab_id)
        self.ws = wc.connect(t["webSocketDebuggerUrl"], open_timeout=10)
        self.mid = 0
        self.inflight = {}
        self.lock = threading.Lock()
        self.samples = []  # (t, n_inflight)
        self.send("Network.enable")
        self.stop = False
        threading.Thread(target=self.reader, daemon=True).start()
        threading.Thread(target=self.sampler, daemon=True).start()

    def send(self, method, params=None):
        self.mid += 1
        self.ws.send(json.dumps({"id": self.mid, "method": method, "params": params or {}}))
        while True:
            msg = json.loads(self.ws.recv())
            if msg.get("id") == self.mid:
                return msg

    def reader(self):
        import websockets.exceptions
        try:
            while not self.stop:
                msg = json.loads(self.ws.recv())
                m = msg.get("method", "")
                p = msg.get("params", {})
                with self.lock:
                    if m == "Network.requestWillBeSent":
                        url = p.get("request", {}).get("url", "")
                        if url.startswith(("data:", "blob:")):
                            continue
                        self.inflight[p["requestId"]] = (time.time(), p.get("type", ""), url)
                    elif m in ("Network.loadingFinished", "Network.loadingFailed"):
                        self.inflight.pop(p.get("requestId"), None)
        except Exception:
            pass

    def sampler(self):
        while not self.stop:
            with self.lock:
                self.samples.append((time.time(), len(self.inflight)))
            time.sleep(0.1)

    def idle_after(self, t_from, hold=0.5, t_to=None):
        """First moment after t_from with zero in-flight for `hold` seconds."""
        with self.lock:
            samples = list(self.samples)
        need = int(hold / 0.1)
        for i, (t, n) in enumerate(samples):
            if t < t_from or (t_to and t > t_to):
                continue
            window = samples[i:i + need]
            if len(window) == need and all(n2 == 0 for _, n2 in window):
                return round(t - t_from, 2)
        return None

    def still_open(self):
        with self.lock:
            return [(ttype, url[:110]) for _, ttype, url in self.inflight.values()]

    def close(self):
        self.stop = True
        try:
            self.ws.close()
        except Exception:
            pass


def open_sage_panel(fl):
    """menu -> SAGE -> composer visible."""
    click_ref(fl.ctx(), "button", r"^$", regex=True)
    ok, _ = wait_until(lambda: snap_ref_re(snapshot(fl.ctx()), "button", r"^SAGE$"), 15)
    if not ok:
        raise RuntimeError("menu did not offer SAGE")
    click_ref(fl.ctx(), "button", r"^SAGE$", regex=True, settle=0.6)
    ok, _ = wait_until(lambda: snap_ref(snapshot(fl.ctx()), "textbox", "Ask SAGE"), 30)
    if not ok:
        raise RuntimeError("Sage composer did not open")


def login_if_needed(fl):
    snap = snapshot(fl.ctx())
    if "/auth/login" in snap_url(snap):
        r = snap_ref(snap, "textbox", "Email") or snap_ref(snap, "textbox", "Enter your email")
        p = snap_ref(snap, "textbox", "Password")
        b = snap_ref(snap, "button", "Sign In")
        cu_json(["act", json.dumps([
            {"do": "type", "ref": r, "text": fl.user},
            {"do": "type", "ref": p, "text": fl.password},
            {"do": "click", "ref": b},
        ])] + fl.ctx())
        ok, _ = wait_until(lambda: "/auth/login" not in snap_url(snapshot(fl.ctx())), 40)
        if not ok:
            raise RuntimeError("login did not leave /auth/login")


def mode_probe(base, org, user, password, out_file):
    """One instrumented flow: what a networkidle wait would cost around the SSE."""
    res = {"probe": {}}
    fl = Flow(990, base, org, user, password, tab_mode="tab")
    t = time.time()
    fl.open_page()
    cu_json(["navigate", f"{base}/{org}/analytics"] + fl.ctx())
    login_if_needed(fl)
    wait_until(lambda: snap_has(snapshot(fl.ctx()), "heading", "Analytics"), 30)
    res["probe"]["open_and_login_s"] = round(time.time() - t, 2)

    probe = NetProbe(fl.tab)
    try:
        # plain navigation: smart settle vs networkidle (reference numbers)
        t_nav = time.time()
        out = cu_json(["navigate", f"{base}/{org}/articles"] + fl.ctx(), timeout=60)
        res["probe"]["navigate_settled_s"] = out.get("settled_ms", 0) / 1000
        res["probe"]["navigate_idle_after_s"] = probe.idle_after(t_nav, t_to=t_nav + 20)

        # open the Sage drawer: the conversations SSE starts and never leaves
        open_sage_panel(fl)
        t_panel = time.time()
        res["probe"]["panel_idle_after_s"] = probe.idle_after(t_panel, t_to=t_panel + 15)

        # send a message and race the two waits
        snap = snapshot(fl.ctx())
        newc = snap_ref(snap, "button", "New conversation")
        if newc:
            cu(["act", json.dumps([{"do": "click", "ref": newc}])] + fl.ctx())
            time.sleep(0.4)
            snap = snapshot(fl.ctx())
        box = snap_ref(snap, "textbox", "Ask SAGE")
        send = snap_ref(snap, "button", "Send")
        fl.marker = "BNPROBE1"
        t_send = time.time()
        act_ok(fl.ctx(), [
            {"do": "type", "ref": box, "text": f"Say exactly: {fl.marker} and nothing else."},
            {"do": "press", "key": "Enter"},
        ])
        ok, waited = wait_until(lambda: page_text(fl.ctx()).count(fl.marker) >= 2, 120)
        t_text = time.time()
        res["probe"]["send_to_text_s"] = round(waited, 2) if ok else None
        # does the network ever go idle after the send? wait 15 s past the text
        res["probe"]["send_idle_after_s"] = probe.idle_after(t_send, t_to=t_text + 15)
        res["probe"]["still_in_flight"] = probe.still_open()
    finally:
        probe.close()
        fl.close_page()
    open(out_file, "w").write(json.dumps(res, indent=1))
    print(json.dumps(res, indent=1))
    return res


# ------------------------------------------------------------ snapshots mode

def mode_snapshots(base, org, user, password, out_file):
    """Repeated snapshots on a heavy page: cost + stability (incremental baseline)."""
    fl = Flow(991, base, org, user, password, tab_mode="tab")
    fl.open_page()
    cu_json(["navigate", f"{base}/{org}/articles"] + fl.ctx(), timeout=60)
    login_if_needed(fl)
    ok, _ = wait_until(
        lambda: (lambda s: s if "/articles" in snap_url(s)
                 and snap_has(s, "heading", "Articles") else None)(snapshot(fl.ctx())), 60)
    if not ok:
        raise RuntimeError("articles page never rendered")
    runs = []
    prev = None
    for i in range(8):
        t = time.time()
        snap = snapshot(fl.ctx())
        ms = round((time.time() - t) * 1000, 1)
        refs = len(re.findall(r"ref=e\d+", snap))
        runs.append({"i": i, "ms": ms, "bytes": len(snap), "refs": refs,
                     "same_as_prev": snap == prev})
        prev = snap
    fl.close_page()
    res = {"snapshots": runs,
           "note": "cu rebuilds the snapshot wholesale; no incremental/delta endpoint exists"}
    open(out_file, "w").write(json.dumps(res, indent=1))
    print(json.dumps(res, indent=1))
    return res


# ------------------------------------------------------- agent-browser mode

AB = os.path.join(HOME, ".local/bin", "agent-browser")


def ab(session, args, timeout=120):
    # --restore abseed: flows share the seeded login state, like the cu tab
    # mode shares one browser profile. Cold logins at 10 parallel are their own
    # (failed) experiment: see the docs.
    r = subprocess.run([AB, "--session", session, "--restore", "abseed"] + args,
                       capture_output=True, text=True, timeout=timeout)
    return r.stdout.strip() + r.stderr.strip(), r.returncode


def ab_ref(snap, role, name):
    """agent-browser snapshot: `- role "name" [ref=eN]`; names may carry icon glyphs."""
    for line in snap.splitlines():
        m = re.match(r'\s*- (\w+) "((?:[^"\\]|\\.)*)" \[[^\]]*ref=(e\d+)\]', line)
        if not m:
            continue
        got_role, got_name, ref = m.groups()
        if got_role == role and re.search(name, got_name):
            return ref
    return None


def ab_click(session, role, name_re, tries=8, settle=0.0):
    """snapshot -> ref -> click, retrying: Playwright refuses clicks while a
    drawer/animation covers the element, so keep re-reading and re-clicking."""
    last = ""
    for attempt in range(tries):
        if settle and attempt == 0:
            time.sleep(settle)
        out, _ = ab(session, ["snapshot", "-i"], timeout=60)
        ref = ab_ref(out, role, name_re)
        if not ref:
            time.sleep(0.3)
            continue
        res, rc = ab(session, ["click", f"@{ref}"])
        if rc == 0 and "Done" in res and "covered" not in res:
            return
        last = res[:120]
        time.sleep(0.35)
    raise RuntimeError(f"ab click {role} {name_re!r} failed: {last}")


def ab_flow(i, base, org, user, password, sage_timeout=180):
    """The same flow as Flow.run, driven through the agent-browser CLI."""
    session = f"benchab{i}x{os.getpid() % 10000}"
    steps = {}
    error = None
    t_flow = time.time()
    try:
        ab(session, ["set", "viewport", "780", "493"], timeout=60)
        t = time.time()
        ab(session, ["open", f"{base}/{org}/analytics"], timeout=120)
        steps["open"] = round(time.time() - t, 3)

        t = time.time()
        def on_login():
            out, _ = ab(session, ["get", "url"], timeout=30)
            return "/auth/login" in out
        ok, _ = wait_until(on_login, 30)
        steps["login"] = 0.0
        if ok:
            ab(session, ["fill", 'input[name="login"]', user])
            ab(session, ["fill", 'input[name="password"]', password])
            ab(session, ["press", "Enter"])  # the Sign In button is covered by .auth-aurora
            def logged_in():
                out, _ = ab(session, ["get", "url"], timeout=30)
                return "/auth/login" not in out
            ok, _ = wait_until(logged_in, 120)
            if not ok:
                raise RuntimeError("login did not leave /auth/login")
            steps["login"] = round(time.time() - t, 3)

        def snap():
            out, _ = ab(session, ["snapshot", "-i"], timeout=60)
            return out

        ok, _ = wait_until(lambda: ab_ref(snap(), "heading", "Analytics"), 60)
        if not ok:
            raise RuntimeError("analytics did not render")
        steps["ready"] = round(time.time() - t, 3) - steps["login"]

        # route: burger (icon-only button) -> Articles
        t = time.time()
        ab_click(session, "button", r"[\ue000-\uf8ff]")
        ok, _ = wait_until(lambda: ab_ref(snap(), "link", r"Articles"), 10)
        if not ok:
            raise RuntimeError("menu did not open")
        ab_click(session, "link", r"Articles$", settle=0.6)
        def at_articles():
            out, _ = ab(session, ["get", "url"], timeout=30)
            return "/articles" in out
        ok, _ = wait_until(at_articles, 20)
        if not ok:
            raise RuntimeError("articles route did not load")
        steps["route"] = round(time.time() - t, 3)

        # subroute: Drafts tab
        t = time.time()
        ab_click(session, "tab", r"Drafts$")
        def at_drafts():
            out, _ = ab(session, ["get", "url"], timeout=30)
            return "tab=drafts" in out
        ok, _ = wait_until(at_drafts, 20)
        if not ok:
            raise RuntimeError("drafts subroute did not load")
        steps["subroute"] = round(time.time() - t, 3)

        # Sage drawer
        t = time.time()
        ab_click(session, "button", r"[\ue000-\uf8ff]")
        ok, _ = wait_until(lambda: ab_ref(snap(), "button", r"SAGE$"), 10)
        if not ok:
            raise RuntimeError("menu did not offer SAGE")
        ab_click(session, "button", r"SAGE$", settle=0.6)
        ok, _ = wait_until(lambda: ab_ref(snap(), "textbox", r"Ask SAGE"), 15)
        if not ok:
            raise RuntimeError("Sage composer did not open")
        steps["sage_open"] = round(time.time() - t, 3)

        try:
            ab_click(session, "button", r"New conversation", tries=3)
            time.sleep(0.4)
        except Exception:
            pass
        marker = "BA" + "".join(random.choice("ABCDEFGHJKLMNPQRSTUVWXYZ23456789") for _ in range(5))
        box = ab_ref(snap(), "textbox", r"Ask SAGE")
        t = time.time()
        ab(session, ["fill", f"@{box}", f"Say exactly: {marker} and nothing else."])
        ab(session, ["press", "Enter"])
        steps["sage_send"] = round(time.time() - t, 3)

        def answered():
            # `get text` is blind to the side drawer on this app (72 chars,
            # no chat); the snapshot is the state primitive that sees it
            out, _ = ab(session, ["snapshot"], timeout=60)
            return out.count(marker) >= 2
        ok, waited = wait_until(answered, sage_timeout)
        if not ok:
            raise RuntimeError(f"no Sage answer in {sage_timeout}s")
        steps["sage_wait"] = round(waited, 3)

        t = time.time()
        ab(session, ["close"], timeout=60)
        steps["close"] = round(time.time() - t, 3)
    except Exception as e:
        error = f"{type(e).__name__}: {e}"
        try:
            ab(session, ["close"], timeout=60)
        except Exception:
            pass
    steps["total"] = round(time.time() - t_flow, 3)
    if not error:
        steps["sage_fraction"] = round(steps.get("sage_wait", 0) / steps["total"], 3)
    fl = Flow(i, base, org, user, password)
    fl.steps = steps
    fl.error = error
    return fl


# ------------------------------------------------------------------- main

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--mode", default="levels",
                    choices=["levels", "block", "probe", "snapshots", "ab"])
    ap.add_argument("--levels", default="1,10,25")
    ap.add_argument("--waves", default="5,2,1", help="waves per level, same count")
    ap.add_argument("--block-media", action="store_true")
    ap.add_argument("--chrome-pattern", default="cu/profiles/default")
    ap.add_argument("--tab-mode", default="context", choices=["context", "tab"],
                    help="context = real login per flow (isolated cookies, sessions "
                         "kick each other); tab = shared session, one tab per flow")
    ap.add_argument("--out", default=None)
    args = ap.parse_args()

    base, user, password = qaspec_credentials()
    org = DEFAULT_ORG
    out_file = args.out or f"/tmp/real-bench-{args.mode}.json"

    if args.mode == "probe":
        mode_probe(base, org, user, password, out_file)
        return
    if args.mode == "snapshots":
        mode_snapshots(base, org, user, password, out_file)
        return

    levels = [int(x) for x in args.levels.split(",")]
    waves = [int(x) for x in args.waves.split(",")]

    if args.mode == "ab":
        args.chrome_pattern = "ms-playwright"
        def ab_daemon_pid():
            for pid in os.listdir("/proc"):
                if not pid.isdigit():
                    continue
                try:
                    cmd = open(f"/proc/{pid}/cmdline", "rb").read().decode("utf-8", "replace")
                except OSError:
                    continue  # the process exited while we were looking
                if "agent-browser-linux-x64" in cmd:
                    return int(pid)
            return None
        dpid = ab_daemon_pid()
        if dpid is None:
            ab("benchwarm", ["open", "about:blank"], timeout=120)  # spawn its daemon
            time.sleep(1)
            dpid = ab_daemon_pid()
    else:
        dpid = daemon_pid()
    if dpid is None and args.mode != "ab":
        sys.exit("daemon process not found")

    sampler = Sampler(args.chrome_pattern, dpid)
    threading.Thread(target=sampler.run, daemon=True).start()
    time.sleep(1.0)

    results = {"tool": args.mode, "mode": args.mode, "block_media": args.block_media,
               "base": base, "levels": [], "machine": {
                   "cpus": os.cpu_count(),
                   "load1_before": os.getloadavg()[0],
                   "mem_total_mb": round(os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES") / 1e6),
               }}

    for level, nwaves in zip(levels, waves):
        sampler.samples.clear()
        t0 = time.time()
        if args.mode == "ab":
            flows = []
            for w in range(nwaves):
                results_w = [None] * level

                def worker(i, w=w):
                    results_w[i] = ab_flow(w * level + i, base, org, user, password)

                ts = [threading.Thread(target=worker, args=(i,)) for i in range(level)]
                for t in ts:
                    t.start()
                for t in ts:
                    t.join()
                flows.extend(results_w)
                time.sleep(1.0)
        else:
            flows = burst(base, org, user, password, level, nwaves,
                          tab_mode=args.tab_mode, block_media=args.block_media)
        wall = round(time.time() - t0, 1)
        results["levels"].append({"level": level, "waves": nwaves, "wall_s": wall,
                                  "summary": summarize(flows), **sampler.peak()})

    results["machine"]["load1_after"] = os.getloadavg()[0]
    open(out_file, "w").write(json.dumps(results, indent=1))
    print(json.dumps(results, indent=1))


if __name__ == "__main__":
    main()
