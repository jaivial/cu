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
# Tabs opened in browser contexts: target id -> context id.
CONTEXT_TABS = {}
# Big enough to force the 64-bit websocket length form that a real screenshot
# uses, so the frame reader cannot get away with the 16-bit one alone.
PNG = base64.b64encode(b"\x89PNG\r\n\x1a\n" + b"fake-png-bytes" * 8000).decode()
GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
# The in-page walk returns JSON; rendering it compactly is the daemon's job.
# Refs are what the agent acts on, so they must survive the round trip.
# When the page under test has an iframe, the frame tree gains a child and
# walks run in its isolated world (context id 7), like a real Chromium.
FRAMES_ON = {"on": False}
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


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path == "/json":
            self.send_discovery()
        elif self.path == "/input-log":
            self.send_json(INPUT_LOG)
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
        body = json.dumps(
            [
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
        ).encode()
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
            FRAMES_ON["on"] = "frames" in url
            return {"id": 1, "result": {"frameId": "f", "url": "https://example.test"}}
        if method == "Target.createBrowserContext":
            return {"result": {"browserContextId": "CTX%d" % (len(CONTEXT_TABS) + 1)}}
        if method == "Target.createTarget":
            target = "TAB%d" % (len(CONTEXT_TABS) + 1)
            CONTEXT_TABS[target] = (params or {}).get("browserContextId")
            return {"result": {"targetId": target}}
        if method == "Target.closeTarget":
            return {"result": {"success": True}}
        if method == "Target.disposeBrowserContext":
            ctx = (params or {}).get("browserContextId")
            for target in [t for t, c in CONTEXT_TABS.items() if c == ctx]:
                del CONTEXT_TABS[target]
            return {"result": {}}
        if method == "Page.getFrameTree":
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
            return {"result": {"executionContextId": 7}}
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
        if method == "Runtime.evaluate":
            expr = (params or {}).get("expression", "")
            if "cuAgentSnapshot" in expr:
                # The walk: in an iframe's isolated world it shows the frame,
                # in the main world the page itself. (The walk embeds the
                # registry, which mentions elementFromPoint, so this must be
                # checked before the click-point branch below.)
                if (params or {}).get("contextId") is not None:
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
                if (params or {}).get("contextId") is not None:
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


ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
