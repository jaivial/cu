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
PNG = base64.b64encode(b"\x89PNG\r\n\x1a\nfake-png-bytes").decode()
GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


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
        # Chromium emits compact JSON, and `cu` parses it that way.
        body = json.dumps(
            [{"webSocketDebuggerUrl": "ws://127.0.0.1:%d/devtools/page/1" % PORT}],
            separators=(",", ":"),
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
            self.reply(self.result_for(message.get("method", "")))

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
    def result_for(method):
        if method == "Page.navigate":
            return {"id": 1, "result": {"frameId": "f", "url": "https://example.test"}}
        if method == "Page.captureScreenshot":
            return {"id": 1, "result": {"data": PNG}}
        return {"id": 1, "result": {}}

    def reply(self, obj):
        payload = json.dumps(obj, separators=(",", ":")).encode()
        frame = bytearray([0x81])
        if len(payload) < 126:
            frame.append(len(payload))
        else:
            frame.append(126)
            frame += len(payload).to_bytes(2, "big")
        frame += payload
        self.connection.sendall(bytes(frame))


ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
