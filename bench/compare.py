#!/usr/bin/env python3
"""Compare cu with agent-browser and Playwright MCP on the same agent tasks.

Each tool drives the same Chromium binary against the same local site, the way
an agent would call it:

  cu             the `cu` CLI, one process per action (a shell tool call), and
                 `cu act` for the batched variant
  agent-browser  its CLI, one process per action
  playwright-mcp @playwright/mcp over stdio JSON-RPC, one tools/call per action

Tasks (each a sequence of agent steps, refs taken from the tool's own snapshot):

  open       open the page
  snapshot   read the page (snapshot tokens are reported too)
  click      click a button that does not navigate
  form       fill two fields, pick an option, submit, read the result
  login      type user and password, submit, read the welcome page
  10 actions ten clicks in a row on the same page

cu and Playwright MCP wait for a navigation an action starts before they
answer; agent-browser does not, so its form and login tasks include the
`wait --url` and `wait --load` calls an agent has to add.

Timing is wall clock per task, median over --iters rounds. All tools are up
at once and every round runs each task on each tool in a rotating order, so
load on a shared machine falls on all of them alike. Tokens are counted
with tiktoken's o200k_base when it is installed (bytes/4 otherwise) on the
text the tool hands the model for a snapshot of the same page.

Usage: bench/compare.py [--iters N] [--tools cu,cu-batch,agent-browser,playwright-mcp]
                        [--out FILE]
"""
import argparse
import json
import os
import re
import shutil
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

HERE = os.path.dirname(os.path.abspath(__file__))
BROWSER = os.environ.get("CU_BROWSER") or shutil.which("chromium") \
    or shutil.which("chromium-browser") or shutil.which("google-chrome")

try:
    import tiktoken
    _ENC = tiktoken.get_encoding("o200k_base")

    def tokens(text):
        return len(_ENC.encode(text))
    TOKENIZER = "o200k_base"
except Exception:  # pragma: no cover - optional dependency
    def tokens(text):
        return (len(text.encode()) + 3) // 4
    TOKENIZER = "bytes/4"

# A page shaped like a real one: navigation, a content list, a form and a
# footer, so the snapshot has something to filter.
NAV = "".join('<li><a href="/p%d">Section %d</a></li>' % (i, i) for i in range(12))
ITEMS = "".join(
    '<article><h3>Item %d</h3><p>Description of item %d with some prose.</p>'
    '<button onclick="this.dataset.n=(+this.dataset.n||0)+1">Add %d</button></article>' % (i, i, i)
    for i in range(10))
HOME = ("""<!doctype html><html><head><title>Shop</title></head><body>
<header><nav><ul>%s</ul></nav><input type=search placeholder="Search products"></header>
<main><h1>Catalogue</h1><button id=go onclick="this.textContent='Clicked'">Go</button>
%s</main><footer><a href="/terms">Terms</a> <a href="/privacy">Privacy</a></footer>
</body></html>""" % (NAV, ITEMS)).encode()
FORM = b"""<!doctype html><title>Order</title><h1>Order</h1>
<form action="/done"><label>Name <input name=name></label>
<label>Email <input name=email type=email></label>
<label>Size <select name=size><option value=s>Small</option><option value=m>Medium</option></select></label>
<button>Send order</button></form>"""
LOGIN = b"""<!doctype html><title>Sign in</title><h1>Sign in</h1>
<form method=post action="/session"><label>User <input name=user></label>
<label>Password <input name=pw type=password></label><button>Sign in</button></form>"""


class Site(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def send(self, body):
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        u = urlparse(self.path)
        if u.path == "/done":
            q = parse_qs(u.query)
            return self.send(("<title>Done</title><h1>Thanks %s, size %s</h1>" % (
                q.get("name", [""])[0], q.get("size", [""])[0])).encode())
        self.send({"/form": FORM, "/login": LOGIN}.get(u.path, HOME))

    def do_POST(self):
        n = int(self.headers.get("Content-Length", "0"))
        user = parse_qs(self.rfile.read(n).decode()).get("user", [""])[0]
        self.send(("<title>Welcome</title><h1>Welcome %s</h1><a href=/>Home</a>" % user).encode())


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def find_ref(snapshot, *words):
    """The ref of the first snapshot line containing all of `words`."""
    for line in snapshot.splitlines():
        if all(w in line for w in words):
            m = re.search(r"ref=(\w*e\d+)", line)
            if m:
                return m.group(1)
    raise RuntimeError("no element %r in snapshot:\n%s" % (words, snapshot[:2000]))


def run(cmd, env=None, stdin=None):
    r = subprocess.run(cmd, env=env, input=stdin, capture_output=True, text=True, timeout=60)
    if r.returncode != 0:
        raise RuntimeError("%s failed: %s %s" % (cmd[:3], r.stdout[-500:], r.stderr[-500:]))
    return r.stdout


# --- tools --------------------------------------------------------------------

class Cu:
    name = "cu"

    def __init__(self, binary):
        self.bin = os.path.abspath(binary)
        self.dir = tempfile.mkdtemp(prefix="cu-cmp-")
        self.env = dict(os.environ, CU_DATA_DIR=self.dir, CU_BROWSER=BROWSER,
                        CU_CDP_PORT=str(free_port()))
        self.proc = subprocess.Popen([self.bin, "start", "--port", str(free_port()),
                                      "--data", self.dir], env=self.env,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        while not os.path.exists(os.path.join(self.dir, "server.json")):
            time.sleep(0.005)

    def cu(self, *args, stdin=None):
        return run([self.bin, *args], env=self.env, stdin=stdin)

    def open(self, url):
        self.cu("navigate", url)

    def snapshot(self):
        return json.loads(self.cu("snapshot"))["snapshot"]

    def click(self, ref):
        out = json.loads(self.cu("click", ref))
        assert out["ok"], out

    def type(self, ref, text, submit=False):
        out = json.loads(self.cu("type", ref, text, *(["--submit"] if submit else [])))
        assert out["ok"], out

    def select(self, ref, value):
        out = json.loads(self.cu("act", json.dumps([{"do": "select", "ref": ref, "value": value}])))
        assert out["ok"], out

    def close(self):
        self.proc.terminate()
        self.proc.wait(10)
        shutil.rmtree(self.dir, ignore_errors=True)


class CuBatch(Cu):
    """cu with the batch endpoint: the steps of a task in one call."""
    name = "cu (batch)"

    def act(self, actions):
        out = json.loads(self.cu("act", json.dumps(actions)))
        assert out["ok"], out
        return out.get("snapshot", "")


class AgentBrowser:
    name = "agent-browser"

    def __init__(self):
        self.session = "cmp%d" % os.getpid()
        self.env = dict(os.environ, AGENT_BROWSER_SESSION=self.session,
                        AGENT_BROWSER_EXECUTABLE_PATH=BROWSER)
        self.ab("open", "about:blank")

    def ab(self, *args):
        return run(["agent-browser", *args], env=self.env)

    def open(self, url):
        self.ab("open", url)

    def snapshot(self):
        # -i (interactive only) is its compact mode, the closest match to cu.
        return self.ab("snapshot", "-i")

    def snapshot_full(self):
        return self.ab("snapshot")

    def click(self, ref):
        self.ab("click", "@" + ref)

    def type(self, ref, text, submit=False):
        self.ab("fill", "@" + ref, text)
        if submit:
            self.ab("press", "Enter")

    def select(self, ref, value):
        self.ab("select", "@" + ref, value)

    def after_submit(self, url_glob):
        # `click`/`press` return before a form submit navigates, and `wait
        # --url` returns once the URL changes, before the new document can be
        # read: an agent has to wait for both itself.
        self.ab("wait", "--url", url_glob)
        self.ab("wait", "--load", "domcontentloaded")

    def close(self):
        try:
            self.ab("close")
        except RuntimeError:
            pass


class PlaywrightMcp:
    name = "playwright-mcp"
    # Refs are re-minted after each action: a ref read before a click no
    # longer resolves after it.
    refs_expire = True

    def __init__(self):
        self.dir = tempfile.mkdtemp(prefix="pwmcp-")
        self.proc = subprocess.Popen(
            ["npx", "-y", "@playwright/mcp@0.0.83", "--headless", "--isolated",
             "--browser", "chromium", "--executable-path", BROWSER, "--output-dir", self.dir],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            text=True, cwd=self.dir)
        self.n = 0
        self.rpc("initialize", {"protocolVersion": "2025-03-26", "capabilities": {},
                                "clientInfo": {"name": "cu-compare", "version": "1"}})
        self.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
        self.proc.stdin.flush()

    def rpc(self, method, params):
        self.n += 1
        self.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.n, "method": method,
                                          "params": params}) + "\n")
        self.proc.stdin.flush()
        while True:
            reply = json.loads(self.proc.stdout.readline())
            if reply.get("id") == self.n:
                if "error" in reply:
                    raise RuntimeError(reply["error"])
                return reply["result"]

    def tool(self, name, **args):
        result = self.rpc("tools/call", {"name": name, "arguments": args})
        text = "".join(c.get("text", "") for c in result.get("content", []))
        if result.get("isError"):
            raise RuntimeError(text)
        return text

    def open(self, url):
        self.tool("browser_navigate", url=url)

    def snapshot(self):
        return self.tool("browser_snapshot")

    def click(self, ref):
        self.tool("browser_click", element=ref, target=ref)

    def type(self, ref, text, submit=False):
        self.tool("browser_type", element=ref, target=ref, text=text, submit=submit)

    def select(self, ref, value):
        self.tool("browser_select_option", element=ref, target=ref, values=[value])

    def close(self):
        self.proc.terminate()
        self.proc.wait(10)
        shutil.rmtree(self.dir, ignore_errors=True)


# --- tasks --------------------------------------------------------------------

def after_submit(tool, url_glob):
    """Wait for a submit to land, for the tools whose actions do not."""
    if hasattr(tool, "after_submit"):
        tool.after_submit(url_glob)


def task_open(t, base):
    t.open(base + "/")


def task_snapshot(t, base):
    t.snapshot()


def task_click(t, base):
    t.click(t.go_ref)


def task_form(t, base):
    t.open(base + "/form")
    s = t.snapshot()
    if isinstance(t, CuBatch):
        out = t.act([
            {"do": "type", "ref": find_ref(s, "Name"), "text": "Ada"},
            {"do": "type", "ref": find_ref(s, "Email"), "text": "ada@example.test"},
            {"do": "select", "ref": find_ref(s, "combobox"), "value": "Medium"},
            {"do": "click", "ref": find_ref(s, "Send order")},
        ])
    else:
        t.type(find_ref(s, "Name"), "Ada")
        t.type(find_ref(s, "Email"), "ada@example.test")
        t.select(find_ref(s, "combobox"), "m")
        t.click(find_ref(s, "Send order"))
        after_submit(t, "**/done*")
        out = t.snapshot()
    assert "Thanks Ada" in out, out


def task_login(t, base):
    t.open(base + "/login")
    s = t.snapshot()
    if isinstance(t, CuBatch):
        out = t.act([
            {"do": "type", "ref": find_ref(s, "User"), "text": "jaime"},
            {"do": "type", "ref": find_ref(s, "Password"), "text": "hunter2", "submit": True},
        ])
    else:
        t.type(find_ref(s, "User"), "jaime")
        t.type(find_ref(s, "Password"), "hunter2", submit=True)
        after_submit(t, "**/session")
        out = t.snapshot()
    assert "Welcome jaime" in out, out


def task_ten(t, base):
    refs = t.item_refs
    if isinstance(t, CuBatch):
        t.act([{"do": "click", "ref": r} for r in refs])
    elif getattr(t, "refs_expire", False):
        # Every Playwright MCP action replaces the page's refs, so an agent
        # has to read a fresh snapshot before the next click.
        for i in range(10):
            t.click(find_ref(t.snapshot(), '"Add %d"' % i))
    else:
        for r in refs:
            t.click(r)


TASKS = [("open", task_open), ("snapshot", task_snapshot), ("click", task_click),
         ("form", task_form), ("login", task_login), ("10 actions", task_ten)]


def prepare(t, base):
    """Load the home page and learn the refs the click tasks use."""
    t.open(base + "/")
    s = t.snapshot()
    t.go_ref = find_ref(s, '"Go"')
    t.item_refs = [find_ref(s, '"Add %d"' % i) for i in range(10)]


def time_task(tool, name, fn, base):
    if name in ("snapshot", "click", "10 actions"):
        prepare(tool, base)
    t = time.perf_counter()
    fn(tool, base)
    return (time.perf_counter() - t) * 1000


def bench(makers, base, iters):
    """All tools up at once; each round runs every task on every tool, in an
    order that rotates per round, so machine load lands on all of them alike.
    """
    tools, out = [], []
    try:
        for make in makers:
            t0 = time.perf_counter()
            tool = make()
            tool.open(base + "/")
            entry = {"tool": tool.name, "cold_open_ms": round((time.perf_counter() - t0) * 1000, 1)}
            tool.open(base + "/")
            snap = tool.snapshot()
            entry["snapshot_tokens"] = tokens(snap)
            entry["snapshot_bytes"] = len(snap.encode())
            if hasattr(tool, "snapshot_full"):
                entry["snapshot_full_tokens"] = tokens(tool.snapshot_full())
            entry["samples"] = {name: [] for name, _ in TASKS}
            tools.append(tool)
            out.append(entry)
        for round_ in range(iters):
            order = list(range(len(tools)))
            order = order[round_ % len(order):] + order[:round_ % len(order)]
            for name, fn in TASKS:
                for i in order:
                    out[i]["samples"][name].append(time_task(tools[i], name, fn, base))
            print("round %d/%d" % (round_ + 1, iters), file=sys.stderr, flush=True)
    finally:
        for tool in tools:
            tool.close()
    for entry in out:
        samples = entry.pop("samples")
        entry["tasks"] = {name: {"median": round(statistics.median(v), 1),
                                 "min": round(min(v), 1)} for name, v in samples.items()}
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=os.path.join(HERE, "..", "target", "release", "cu"))
    ap.add_argument("--iters", type=int, default=5)
    ap.add_argument("--tools", default="cu,cu-batch,agent-browser,playwright-mcp")
    ap.add_argument("--out")
    args = ap.parse_args()

    site = ThreadingHTTPServer(("127.0.0.1", 0), Site)
    threading.Thread(target=site.serve_forever, daemon=True).start()
    base = "http://127.0.0.1:%d" % site.server_address[1]
    makers = {"cu": lambda: Cu(args.bin), "cu-batch": lambda: CuBatch(args.bin),
              "agent-browser": AgentBrowser, "playwright-mcp": PlaywrightMcp}
    results = {"environment": {"browser": BROWSER, "iters": args.iters, "tokenizer": TOKENIZER,
                               "date": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
                               "home_page_bytes": len(HOME)},
               "tools": []}
    results["tools"] = bench([makers[n] for n in args.tools.split(",")], base, args.iters)
    for t in results["tools"]:
        print("%-15s %s" % (t["tool"], "  ".join("%s %.0f" % (k, v["median"])
                                                for k, v in t["tasks"].items())), file=sys.stderr)
    site.shutdown()
    print(json.dumps(results, indent=2))
    if args.out:
        with open(args.out, "w") as f:
            json.dump(results, f, indent=2)


if __name__ == "__main__":
    main()
