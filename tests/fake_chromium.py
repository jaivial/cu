#!/usr/bin/env python3
"""Stand-in for Chromium used by the integration tests.

Serves the DevTools HTTP discovery endpoint (`/json`) and answers the CDP
commands `cu` sends over a WebSocket, so navigate/screenshot can be tested
without launching a real browser. Both run on a single port, like Chromium's
`--remote-debugging-port`.
"""
import base64
import hashlib
import json
import os
import struct
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# Started by the tests with the port as the first argument, or by the daemon
# itself as `CU_BROWSER`, in which case it reads Chromium's own flag.
PORT = next(
    (int(a.split("=", 1)[1]) for a in sys.argv if a.startswith("--remote-debugging-port=")),
    None,
) or int(sys.argv[1])
# Where to record that the browser was told to close (`Browser.close`).
CLOSED_MARKER = os.environ.get("FAKE_CHROMIUM_CLOSED")
# Input commands received, in order; served at /input-log for the tests.
INPUT_LOG = []
# Every DevTools command received, in order; served at /command-log, so a
# test can see how many round trips a tool took.
COMMANDS = []
# The viewport the page is emulated at, as `Emulation.setDeviceMetricsOverride`
# last set it: (width, height, deviceScaleFactor), or None when the browser's
# own window size applies. Like Chromium, `clearDeviceMetricsOverride` puts it
# back to None and the metrics change back.
EMULATED = [None]
# The browser's own window size, in CSS pixels: what an un-emulated tab is
# shown at, and what a cleared override falls back to.
WINDOW = [1280, 720]
# Tabs opened in browser contexts: target id -> context id.
CONTEXT_TABS = {}
# Targets closed with Target.closeTarget: gone from discovery, like Chromium.
CLOSED = set()
# Every Browser.setDownloadBehavior call, as the params it arrived with, so a
# test can see whether the daemon gave a named context a directory of its own.
DOWNLOAD_BEHAVIOUR = []
DENY_ON = {"on": False}
# Big enough to force the 64-bit websocket length form that a real screenshot
# uses, so the frame reader cannot get away with the 16-bit one alone.
PNG = base64.b64encode(b"\x89PNG\r\n\x1a\n" + b"fake-png-bytes" * 8000).decode()
GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
# The in-page walk returns JSON; rendering it compactly is the daemon's job.
# Refs are what the agent acts on, so they must survive the round trip.
# When the page under test has an iframe, the frame tree gains a child and
# walks run in its isolated world (context id 7), like a real Chromium.
FRAMES_ON = {"on": False}
# Interstitial scenarios, driven by the URL the test navigates to:
#   cf://            a Cloudflare "Just a moment..." interstitial that clears
#                    itself after CLEAR_AFTER probes (an automatic pass);
#   cfhold://        the same interstitial that never clears: it stays an
#                    interstitial for ever, so a wait must end on a budget,
#                    not on a page that quietly turned ready.
# PROBES counts every challenge probe seen on the tab, so a test can assert
# both that the fake cleared (and when) and that a hold really did not.
INTERSTITIAL = {"kind": None, "probes": 0}
# Probes the self-clearing interstitial is shown before it becomes the real
# page. Small, so a test with the default 10 s budget sees it resolve.
CLEAR_AFTER = 2
CHILD_SNAPSHOT = json.dumps(
    {
        "url": "https://example.test/child",
        "title": "child",
        "nodes": [{"ref": "e9", "role": "textbox", "name": "inner"}],
        "headings": [],
        "next": 9,
    },
    separators=(",", ":"),
)
CHILD_WORDS = "words from inside the frame"
# When the page is Chromium's own "the site refused you" renderer error page
# (`chrome-error://chromewebdata/`), the probe still runs in it and the
# classification must say Blocked, not the "not an http(s) page" ready it
# used to read as. A navigate to any `deny://` URL makes the fake serve it.
DENY_URL = "chrome-error://chromewebdata/"
DENY_TEXT = (
    "access to x.com was denied you don't have authorization to view this "
    "page. http error 403"
)
# Execution context id of the iframe's isolated world.
CHILD_CONTEXT = 7
# The document generation: a navigation replaces the document, and with it
# every execution context. A context id from the previous generation is
# refused, like Chromium refuses one that names a world that is gone.
GENERATION = [0]
# (frame id, generation) -> execution context id.
CONTEXTS = {}
FAKE_SNAPSHOT = json.dumps(
    {
        "url": "https://example.test/",
        "title": "bench",
        "nodes": [
            {"ref": "e1", "role": "link", "name": "one"},
            {"ref": "e2", "role": "textbox", "name": "q"},
        ],
        "headings": ["Bench target"],
    },
    separators=(",", ":"),
)


def interstitial_url():
    """The address the challenge page rewrites itself to.

    Cloudflare puts its tokens in the query, and cu cuts URLs to origin and
    path before reporting them, so the query is what proves the redaction
    too -- but the frame tree still serves the full address, like a browser.
    """
    return "https://example.test/cf-chl/verify?__cf_chl_tk=secret-token"


def interstitial_probe(holding):
    """What `PROBE_JS` sees on the interstitial page.

    Held: the language-independent markers of a Cloudflare managed challenge
    -- `_cf_chl_opt` and the error-text element -- under a 403 and the
    `cf-mitigated: challenge` hint. Cleared: the real page, 200, no marker,
    which is the only shape classified `ready`.
    """
    if holding:
        return {
            "url": interstitial_url(),
            "title": "Just a moment...",
            "ready": "complete",
            "status": 403,
            "text": "checking your browser before accessing example.test",
            "markers": ["cf-chl-opt", "cf-challenge", "cf-error-text"],
            "frames": [],
        }
    return {
        "url": "https://example.test/account",
        "title": "Example",
        "ready": "complete",
        "status": 200,
        "text": "your account page, loaded",
        "markers": [],
        "frames": [],
    }


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path == "/json":
            self.send_discovery()
        elif self.path == "/input-log":
            self.send_json(INPUT_LOG)
        elif self.path == "/command-log":
            self.send_json(COMMANDS)
        elif self.path == "/download-behaviour":
            self.send_json(DOWNLOAD_BEHAVIOUR)
        elif self.path == "/viewport":
            self.send_raw(json.dumps(EMULATED[0], separators=(",", ":")))
        elif self.path.startswith("/emulate"):
            # Seed an override the way another tool would, before the daemon
            # runs: GET /emulate?width=390&height=844&scale=2
            q = dict(
                kv.split("=", 1) for kv in self.path.split("?", 1)[-1].split("&") if "=" in kv
            ) if "?" in self.path else {}
            EMULATED[0] = [
                float(q.get("width", 1280)),
                float(q.get("height", 720)),
                float(q.get("scale", 1)),
            ]
            self.send_raw(json.dumps(EMULATED[0], separators=(",", ":")))
        elif self.path == "/interstitial":
            self.send_json(dict(INTERSTITIAL, clear_after=CLEAR_AFTER))
        elif self.path == "/json/version":
            self.send_json(
                {
                    "Browser": "FakeChromium/1.0",
                    "webSocketDebuggerUrl": "ws://127.0.0.1:%d/devtools/browser/B1" % PORT,
                }
            )
        elif self.path.startswith("/devtools/"):
            self.serve_websocket()
        else:
            self.send_empty(404)

    def send_discovery(self):
        # Chromium pretty-prints this list and puts non-page targets (extension
        # background pages, service workers) in it too, sometimes ahead of the
        # page. Reproduce all of it so target selection is exercised.
        targets = [
                {
                    "description": "",
                    "devtoolsFrontendUrl": "devtools://devtools/inspector.html",
                    "id": "EXT1",
                    "title": "Extension",
                    "type": "background_page",
                    "url": "chrome-extension://abc/background.html",
                    "webSocketDebuggerUrl": "ws://127.0.0.1:%d/devtools/page/EXT1" % PORT,
                },
                *[
                    {
                        "id": target,
                        "type": "page",
                        "url": "about:blank",
                        "webSocketDebuggerUrl": "ws://127.0.0.1:%d/devtools/page/%s" % (PORT, target),
                    }
                    for target in CONTEXT_TABS
                ],
                {
                    "description": "",
                    "devtoolsFrontendUrl": "devtools://devtools/inspector.html",
                    "id": "PAGE1",
                    "title": "about:blank",
                    "type": "page",
                    "url": "about:blank",
                    "webSocketDebuggerUrl": "ws://127.0.0.1:%d/devtools/page/PAGE1" % PORT,
                },
            ]
        body = json.dumps([t for t in targets if t["id"] not in CLOSED]).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def send_json(self, obj):
        body = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def send_raw(self, body: str):
        """Send a body verbatim: for the endpoints whose exact spelling other
        tests assert on."""
        body = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def send_empty(self, code):
        self.send_response(code)
        self.send_header("Content-Length", "0")
        self.end_headers()

    # --- WebSocket -----------------------------------------------------
    def serve_websocket(self):
        if not self.handshake():
            return
        while True:
            frame = self.read_frame()
            if frame is None:
                break
            try:
                message = json.loads(frame.decode())
            except ValueError:
                break
            method = message.get("method", "")
            COMMANDS.append(method)
            reply = self.result_for(method, message.get("params"))
            reply["id"] = message.get("id", 1)
            self.reply(reply)
            if method == "Browser.close":
                if CLOSED_MARKER:
                    open(CLOSED_MARKER, "w").write("closed\n")
                os._exit(0)

    def handshake(self):
        key = (self.headers.get("Sec-WebSocket-Key") or "").strip()
        if not key:
            return False
        accept = base64.b64encode(hashlib.sha1((key + GUID).encode()).digest()).decode()
        self.connection.sendall(
            (
                "HTTP/1.1 101 Switching Protocols\r\n"
                "Upgrade: websocket\r\n"
                "Connection: Upgrade\r\n"
                "Sec-WebSocket-Accept: %s\r\n\r\n" % accept
            ).encode()
        )
        return True

    def read_exact(self, count):
        data = b""
        while len(data) < count:
            chunk = self.rfile.read(count - len(data))
            if not chunk:
                return None
            data += chunk
        return data

    def read_frame(self):
        head = self.rfile.read(2)
        if len(head) < 2:
            return None
        length = head[1] & 0x7F
        if length == 126:
            extended = self.read_exact(2)
            if extended is None:
                return None
            length = int.from_bytes(extended, "big")
        elif length == 127:
            return None
        mask = self.read_exact(4)
        if mask is None:
            return None
        data = self.read_exact(length)
        if data is None:
            return None
        return bytes(data[i] ^ mask[i % 4] for i in range(length))

    @staticmethod
    def result_for(method, params=None):
        if method == "Page.navigate":
            # A URL the browser cannot load answers with `errorText`, like a
            # real Chromium against a name that does not resolve.
            url = (params or {}).get("url", "")
            if url.startswith("fail://"):
                return {
                    "result": {
                        "frameId": "f",
                        "errorText": "net::ERR_NAME_NOT_RESOLVED",
                    }
                }
            # A refusal at the HTTP level: the origin answered (403), Chromium
            # refuses to render it and leaves its own error page on the tab.
            # The navigate call itself reports the failure, like Chromium.
            if url.startswith("httpfail://"):
                DENY_ON["on"] = True
                return {
                    "result": {
                        "frameId": "f",
                        "errorText": "net::ERR_HTTP_RESPONSE_CODE_FAILURE",
                    }
                }
            # A site that refuses the client outright leaves Chromium on its
            # own renderer error page (`chrome-error://chromewebdata/`), with
            # the refusal wording inside it. The navigation itself answered 0
            # (the error is renderer-internal); the wording is what the probe
            # has to work with.
            DENY_ON["on"] = url.startswith("deny://")
            FRAMES_ON["on"] = "frames" in url
            # A new document: the worlds of the old one are gone.
            GENERATION[0] += 1
            # cf:// is an interstitial that clears by itself after a few
            # probes; cfhold:// is the same page that never clears.
            INTERSTITIAL["kind"] = (
                "clear" if url.startswith("cf://") else "hold"
            ) if url.startswith(("cf://", "cfhold://")) else None
            INTERSTITIAL["probes"] = 0
            return {"id": 1, "result": {"frameId": "f", "url": "https://example.test"}}
        if method == "Browser.setDownloadBehavior":
            DOWNLOAD_BEHAVIOUR.append(params or {})
            return {"result": {}}
        if method in ("Page.bringToFront", "Target.activateTarget"):
            # How the hand-off proves it delivered the window to the person:
            # both the page-level call and the browser-level activation must
            # arrive, in that order.
            INPUT_LOG.append(method)
            return {"result": {}}
        if method == "Target.createBrowserContext":
            return {"result": {"browserContextId": "CTX%d" % (len(CONTEXT_TABS) + 1)}}
        if method == "Target.createTarget":
            target = "TAB%d" % (len(CONTEXT_TABS) + 1)
            CONTEXT_TABS[target] = (params or {}).get("browserContextId")
            return {"result": {"targetId": target}}
        if method == "Target.closeTarget":
            # Leaves discovery, as in Chromium; a fake that kept listing it
            # made the daemon poll `/json` for its whole two-second wait.
            target = (params or {}).get("targetId")
            CLOSED.add(target)
            CONTEXT_TABS.pop(target, None)
            return {"result": {"success": True}}
        if method == "Target.disposeBrowserContext":
            ctx = (params or {}).get("browserContextId")
            for target in [t for t, c in CONTEXT_TABS.items() if c == ctx]:
                del CONTEXT_TABS[target]
            return {"result": {}}
        if method == "Page.getFrameTree":
            if DENY_ON["on"]:
                return {"result": {"frameTree": {"frame": {"id": "MAINFRAME", "url": DENY_URL}}}}
            if INTERSTITIAL["kind"]:
                return {
                    "result": {
                        "frameTree": {
                            "frame": {
                                "id": "MAINFRAME",
                                "url": interstitial_url(),
                            }
                        }
                    }
                }
            if FRAMES_ON["on"]:
                return {
                    "result": {
                        "frameTree": {
                            "frame": {"id": "MAINFRAME", "url": "https://example.test/frames"},
                            "childFrames": [
                                {"frame": {"id": "CHILD1", "url": "https://example.test/child"}}
                            ],
                        }
                    }
                }
            return {"result": {"frameTree": {"frame": {"id": "MAINFRAME", "url": "about:blank"}}}}
        if method == "Page.createIsolatedWorld":
            # One isolated world per frame per document, like Chromium: cu
            # reads the main document in its own isolated world too, so "has a
            # contextId" does not mean "is the iframe" -- the id says which
            # frame it is. A navigation replaces the document, and with it the
            # worlds: the ids of the previous generation are refused.
            frame = (params or {}).get("frameId")
            key = (frame, GENERATION[0])
            if key not in CONTEXTS:
                CONTEXTS[key] = (
                    CHILD_CONTEXT + 100 * GENERATION[0]
                    if frame == "CHILD1"
                    else 5 + 100 * GENERATION[0]
                )
            return {"result": {"executionContextId": CONTEXTS[key]}}
        if method == "DOM.getFrameOwner":
            return {"result": {"backendNodeId": 42}}
        if method == "DOM.resolveNode":
            return {"result": {"object": {"objectId": "frame-owner-42"}}}
        if method == "Runtime.callFunctionOn":
            # scrollIntoView + getBoundingClientRect, as the daemon asks for.
            return {
                "result": {
                    "result": {"type": "string", "value": '{"x":10,"y":20}'}
                }
            }
        if method in ("Input.dispatchMouseEvent", "Input.insertText", "Input.dispatchKeyEvent"):
            INPUT_LOG.append(method)
            return {"result": {}}
        if method == "Page.captureScreenshot":
            return {"id": 1, "result": {"data": PNG}}
        if method == "Emulation.setDeviceMetricsOverride":
            p = params or {}
            EMULATED[0] = [
                p.get("width"),
                p.get("height"),
                p.get("deviceScaleFactor"),
            ]
            return {"result": {}}
        if method == "Emulation.clearDeviceMetricsOverride":
            EMULATED[0] = None
            return {"result": {}}
        if method == "Runtime.evaluate":
            expr = (params or {}).get("expression", "")
            context = (params or {}).get("contextId")
            if context is not None and context not in CONTEXTS.values():
                # The world is gone: the document behind it was replaced.
                return {
                    "id": 1,
                    "error": {
                        "code": -32000,
                        "message": "Cannot find context with specified id",
                    },
                }
            if "challenge" in expr and "location.href" in expr and INTERSTITIAL["kind"]:
                INTERSTITIAL["probes"] += 1
                holding = INTERSTITIAL["kind"] == "hold" or INTERSTITIAL[
                    "probes"
                ] <= CLEAR_AFTER
                value = json.dumps(
                    interstitial_probe(holding),
                    separators=(",", ":"),
                )
                return {"id": 1, "result": {"result": {"type": "string", "value": value}}}
            if "challenge" in expr and "location.href" in expr and DENY_ON["on"]:
                # The challenge probe (PROBE_JS) on Chromium's error page: the
                # URL is the renderer's, the title the site's name and the
                # body its wording. No vendor marker anywhere.
                value = json.dumps(
                    {
                        "url": DENY_URL,
                        "title": "x.com",
                        "ready": "complete",
                        "status": 200,
                        "text": DENY_TEXT,
                        "markers": [],
                        "frames": [],
                    },
                    separators=(",", ":"),
                )
                return {"id": 1, "result": {"result": {"type": "string", "value": value}}}
            if "__cuSnapshot" in expr:
                # The walk: in an iframe's isolated world it shows the frame,
                # in the main world the page itself. (The walk embeds the
                # registry, which mentions elementFromPoint, so this must be
                # checked before the click-point branch below.)
                if (params or {}).get("contextId") == CHILD_CONTEXT + 100 * GENERATION[0]:
                    value = CHILD_SNAPSHOT
                else:
                    value = FAKE_SNAPSHOT
                return {"id": 1, "result": {"result": {"type": "string", "value": value}}}
            if "elementFromPoint" in expr:
                # The top document sees the iframe at the click point.
                return {"id": 1, "result": {"result": {"type": "string", "value": "IFRAME"}}}
            if "innerText" in expr:
                # In an iframe's isolated world the words are the frame's, so a
                # test can tell the two documents apart.
                if (params or {}).get("contextId") == CHILD_CONTEXT + 100 * GENERATION[0]:
                    words = CHILD_WORDS
                else:
                    words = "the quick brown fox jumps over the lazy dog"
                return {
                    "id": 1,
                    "result": {
                        "result": {
                            "type": "string",
                            "value": json.dumps(
                                {"n": len(words), "full": len(words), "text": words},
                                separators=(",", ":"),
                            ),
                        }
                    },
                }
            if "document.readyState" in expr:
                # The smart wait asks whether the page is usable; answer that it
                # is, so navigate is not held for the whole settle budget.
                return {
                    "id": 1,
                    "result": {
                        "result": {
                            "type": "string",
                            "value": '{"ready":"complete","mutating":0}',
                        }
                    },
                }
            if "__cu.locate(" in expr:
                ref = expr.split("__cu.locate(")[1].split('"')[1]
                if ref == "e404":
                    value = '{"error":"e404 is not on the page any more; take a new snapshot"}'
                else:
                    value = '{"x":10.5,"y":20}'
                return {"result": {"result": {"type": "string", "value": value}}}
            if "__cu.focus(" in expr or "__cu.select(" in expr:
                return {"result": {"result": {"type": "string", "value": "{}"}}}
            if "innerWidth" in expr and "innerHeight" in expr and "devicePixelRatio" not in expr:
                # The measure step of a capture: the viewport in CSS pixels.
                w, h, _ = EMULATED[0] or [WINDOW[0], WINDOW[1], 1]
                value = json.dumps(
                    {"x": 0, "y": 0, "w": 100, "h": 20, "vw": w, "vh": h},
                    separators=(",", ":"),
                )
                return {"result": {"result": {"type": "string", "value": value}}}
            if "devicePixelRatio" in expr and "innerWidth" in expr:
                # The viewport probe: an emulated tab reports the emulated
                # metrics, an un-emulated one the window's.
                w, h, d = EMULATED[0] or [WINDOW[0], WINDOW[1], 1]
                value = json.dumps({"w": w, "h": h, "d": d})
                return {"result": {"result": {"type": "string", "value": value}}}
            return {"id": 1, "result": {"result": {"type": "undefined"}}}
        return {"id": 1, "result": {}}

    def reply(self, obj):
        payload = json.dumps(obj, separators=(",", ":")).encode()
        frame = bytearray([0x81])
        if len(payload) < 126:
            frame.append(len(payload))
        elif len(payload) < 1 << 16:
            frame.append(126)
            frame += len(payload).to_bytes(2, "big")
        else:
            # Chromium answers a screenshot with a 64-bit length frame.
            frame.append(127)
            frame += len(payload).to_bytes(8, "big")
        frame += payload
        self.connection.sendall(bytes(frame))


class Server(ThreadingHTTPServer):
    # Chromium's DevTools socket queues far more than socketserver's default
    # backlog of 5. With the tests running in parallel and every websocket
    # holding a handler thread, a sixth pending connect was dropped and the
    # daemon's bounded connect reported "connection timed out" on a browser
    # that was up. A real listener's backlog keeps the fake honest.
    request_queue_size = 128
    daemon_threads = True


server = Server(("127.0.0.1", PORT), Handler)
if PORT == 0:
    # Asked for any free port: the tests pass 0 so no other process can take
    # the port between choosing it and binding it. Report the one bound, and
    # use it in the websocket URLs discovery hands out.
    PORT = server.server_address[1]
    print(PORT, flush=True)
server.serve_forever()
