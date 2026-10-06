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
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1])
# Big enough to force the 64-bit websocket length form that a real screenshot
# uses, so the frame reader cannot get away with the 16-bit one alone.
PNG = base64.b64encode(b"\x89PNG\r\n\x1a\n" + b"fake-png-bytes" * 8000).decode()
GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
# The in-page walk returns JSON; rendering it compactly is the daemon's job.
# Refs are what the agent acts on, so they must survive the round trip.
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
            self.reply(
                self.result_for(message.get("method", ""), message.get("params"))
            )

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
            return {"id": 1, "result": {"frameId": "f", "url": "https://example.test"}}
        if method == "Page.captureScreenshot":
            return {"id": 1, "result": {"data": PNG}}
        if method == "Runtime.evaluate":
            expr = (params or {}).get("expression", "")
            if "cuAgentSnapshot" in expr:
                return {
                    "id": 1,
                    "result": {
                        "result": {"type": "string", "value": FAKE_SNAPSHOT}
                    },
                }
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
