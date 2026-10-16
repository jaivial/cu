#!/usr/bin/env python3
"""Stand-in for Chromium used by the integration tests.

Serves the DevTools HTTP discovery endpoint (`/json`) and answers the CDP
commands `cu` sends over a WebSocket, so navigate/screenshot can be tested
without launching a real browser. Both run on a single port, like Chromium's
`--remote-debugging-port`.
"""
import base64
import hashlib
import itertools
import json
import os
import socket
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
# Tabs opened in browser contexts: target id -> context id.
CONTEXT_TABS = {}
# --- CDP `Fetch`: the domain a basic-auth responder lives in ------------
# Every `Fetch.*` command the daemon sent, in order, with the connection it
# arrived on and the hosts it covers, served at /fetch-log. A test reads it to
# see what was armed, answered and released.
FETCH_LOG = []
# A `Fetch.enable` that armed a host another live connection already had
# armed. Two interceptors on one host is the state the responder must never
# reach, so the fake counts it rather than modelling it as harmless.
DOUBLE_ARMED = []
# The open CDP websockets, so a test can push a challenge into the connection
# the responder holds: a challenge that arrives with no command in between is
# the whole reason that responder exists.
SESSIONS = []
SESSION_IDS = itertools.count(1)
SESSION_LOCK = threading.Lock()
# Request ids the fake hands out to challenges it pushes itself.
REQUEST_IDS = itertools.count(1)
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
# Whether the page is Chromium's own renderer refuse page (see DENY_URL).
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


def host_of(url):
    """The host of a URL, without scheme, userinfo, port or path."""
    rest = url.split("://", 1)[-1]
    rest = rest.split("/", 1)[0]
    return rest.rsplit("@", 1)[-1].rsplit(":", 1)[0].lower()


def pattern_host(pattern):
    """The host an `Fetch.enable` `urlPattern` covers, and whether it was a
    `*.` wildcard (which covers subdomains too)."""
    rest = pattern.split("://", 1)[-1]
    host = rest.split("/", 1)[0]
    if host.startswith("*."):
        return host[2:].rsplit(":", 1)[0].lower(), True
    return host.rsplit(":", 1)[0].lower(), False


class Session:
    """One CDP websocket, with the `Fetch` interception it has armed.

    Two threads write to this socket: the connection's own read loop, which
    answers commands, and whichever HTTP request a test fires to push a
    challenge in -- so writes are serialised.
    """

    def __init__(self, sock, path):
        self.id = next(SESSION_IDS)
        self.sock = sock
        self.path = path
        self.lock = threading.Lock()
        # The hosts this connection intercepts, and its `*.` wildcards.
        self.hosts = set()
        self.wilds = set()
        # A navigation whose reply is held until its challenge is answered:
        # Chromium does not answer `Page.navigate` while the document request
        # sits paused waiting for credentials.
        self.pending_nav = None  # type: dict | None
        self.dead = False

    def covers(self, host):
        return host in self.hosts or any(host.endswith("." + w) for w in self.wilds)

    def arm(self, hosts, wilds):
        """Arm this connection, recording any host another live connection
        already had armed -- two interceptors over one host."""
        overlap = set()
        with SESSION_LOCK:
            for other in SESSIONS:
                if other is self or other.dead or not (other.hosts | other.wilds):
                    continue
                overlap |= (hosts & other.hosts) | (hosts & other.wilds) | (wilds & other.hosts)
            self.hosts |= hosts
            self.wilds |= wilds
        if overlap:
            DOUBLE_ARMED.append(
                {
                    "session": self.id,
                    "hosts": sorted(overlap),
                    "with": sorted(
                        s["id"]
                        for s in SESSIONS
                        if s is not self
                        and not s.dead
                        and (s.hosts | s.wilds) & overlap
                    ),
                }
            )

    def disarm(self):
        self.hosts = set()
        self.wilds = set()
        self.pending_nav = None

    def close(self):
        """The target went away: Chromium drops the socket, and anything
        reading it sees the stream end."""
        with self.lock:
            self.dead = True
            self.hosts = set()
            self.wilds = set()
            try:
                self.sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass

    def frame(self, obj):
        payload = json.dumps(obj, separators=(",", ":")).encode()
        out = bytearray([0x81])
        if len(payload) < 126:
            out.append(len(payload))
        elif len(payload) < 1 << 16:
            out.append(126)
            out += len(payload).to_bytes(2, "big")
        else:
            # Chromium answers a screenshot with a 64-bit length frame.
            out.append(127)
            out += len(payload).to_bytes(8, "big")
        out += payload
        with self.lock:
            if self.dead:
                return
            try:
                self.sock.sendall(bytes(out))
            except OSError:
                # The daemon dropped the connection: its interception died
                # with it, and no challenge can be pushed in any more.
                self.dead = True

    def event(self, method, params):
        self.frame({"method": method, "params": params})

    def reply(self, message_id, result):
        self.frame({"id": message_id, "result": result})

    def document(self, request_id, url, status, frame="f"):
        """`Network.responseReceived` for the main document: what the daemon
        reads the navigation's status out of."""
        self.event(
            "Network.responseReceived",
            {
                "requestId": request_id,
                "frameId": frame,
                "loaderId": request_id,
                "type": "Document",
                "response": {
                    "url": url,
                    "status": status,
                    "statusText": "",
                    "headers": {"content-type": "text/html"},
                },
            },
        )

    def challenge(self, request_id, url, request_paused=True, document_status=401):
        """What a host behind basic auth does to an intercepted request: it
        pauses, answers 401 with `WWW-Authenticate`, and waits for
        credentials."""
        if request_paused:
            self.event(
                "Fetch.requestPaused",
                {"requestId": request_id, "frameId": "f", "request": {"url": url}},
            )
        if document_status:
            self.document(request_id, url, document_status)
        self.event(
            "Fetch.authRequired",
            {
                "requestId": request_id,
                "frameId": "f",
                "request": {"url": url},
                "authChallenge": {
                    "source": "Server",
                    "origin": "http://%s" % host_of(url),
                    "realm": "fake",
                },
            },
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
        elif self.path == "/download-behaviour":
            self.send_json(DOWNLOAD_BEHAVIOUR)
        elif self.path == "/fetch-log":
            # Every `Fetch.*` command the daemon sent, in order.
            self.send_json(FETCH_LOG)
        elif self.path == "/fetch-overlaps":
            # A `Fetch.enable` that landed while another live connection had
            # the same host armed: two interceptors over one host.
            self.send_json(DOUBLE_ARMED)
        elif self.path == "/fetch-armed":
            # Which connections are intercepting what, right now.
            with SESSION_LOCK:
                self.send_json(
                    [
                        {
                            "session": s.id,
                            "hosts": sorted(s.hosts),
                            "wildcards": sorted(s.wilds),
                        }
                        for s in SESSIONS
                        if not s.dead and (s.hosts or s.wilds)
                    ]
                )
        elif self.path.startswith("/challenge?"):
            self.send_json(self.challenge_now())
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

    def challenge_now(self):
        """Push a challenge to whoever is intercepting `host`, the way a page's
        own request does once the document has settled: no command from the
        daemon in between, so only a background responder can answer it.

        `?host=` picks the host (required), `?request=` names the request id
        (default: a counter) and `?paused=0` leaves out `Fetch.requestPaused`.
        """
        query = self.path.split("?", 1)[1]
        args = dict(p.split("=", 1) for p in query.split("&") if "=" in p)
        host = (args.get("host") or "").lower()
        request_id = args.get("request") or "late-%d" % next(REQUEST_IDS)
        paused = args.get("paused", "1") != "0"
        with SESSION_LOCK:
            targets = [s for s in SESSIONS if not s.dead and s.covers(host)]
        for session in targets:
            session.challenge(request_id, "https://%s/late" % host, paused)
        return {"host": host, "request": request_id, "challenged": len(targets)}

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

    def send_empty(self, code):
        self.send_response(code)
        self.send_header("Content-Length", "0")
        self.end_headers()

    # --- WebSocket -----------------------------------------------------
    def serve_websocket(self):
        if not self.handshake():
            return
        # One `Session` per websocket: it is what holds this connection's
        # `Fetch` interception, and a test pushes challenges into it by host.
        session = Session(self.connection, self.path)
        self.session = session
        with SESSION_LOCK:
            SESSIONS.append(session)
        try:
            while True:
                frame = self.read_frame()
                if frame is None:
                    break
                try:
                    message = json.loads(frame.decode())
                except ValueError:
                    break
                method = message.get("method", "")
                self.after = []
                reply = self.result_for(method, message.get("params"), message.get("id", 1))
                # `None` means the reply is held back: an intercepted
                # navigation is not answered while its document request sits
                # paused waiting for credentials.
                if reply is not None:
                    reply["id"] = message.get("id", 1)
                    self.reply(reply)
                # What the command provoked goes out after its own reply, the
                # way a browser orders it. It matters here: the daemon reads
                # the last `id`-bearing message as the navigation's reply, so
                # a navigation answered late must be the last thing written.
                for extra in self.after:
                    self.session.frame(extra)
                self.after = []
                if method == "Browser.close":
                    if CLOSED_MARKER:
                        open(CLOSED_MARKER, "w").write("closed\n")
                    os._exit(0)
        finally:
            session.dead = True
            session.disarm()
            with SESSION_LOCK:
                if session in SESSIONS:
                    SESSIONS.remove(session)

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

    def result_for(self, method, params=None, message_id=1):
        # `Fetch` first: an interception armed on this connection changes what
        # a navigation to a covered host does.
        if method.startswith("Fetch."):
            return self.fetch_for(method, params or {})
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
            # A host this connection intercepts answers a challenge instead of
            # the document straight away, and the reply waits for the
            # credentials. Chromium holds the navigation open exactly this
            # way, and the daemon's auth window depends on it.
            if self.session.covers(host_of(url)):
                request_id = "fake-nav-%d" % next(REQUEST_IDS)
                self.session.challenge(request_id, url)
                self.session.pending_nav = {"id": message_id, "request": request_id, "url": url}
                return None
            return {"id": 1, "result": {"frameId": "f", "url": "https://example.test"}}
        if method == "Browser.setDownloadBehavior":
            DOWNLOAD_BEHAVIOUR.append(params or {})
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
            # Closing a target drops its DevTools sockets, as in Chromium --
            # nothing is sent to say so, the connections simply die and any
            # interception on them goes with them. It is not a stand-in for
            # `Fetch.disable`: a responder that gives its interception back
            # before the close is visible in the log, one that never does is
            # not.
            with SESSION_LOCK:
                doomed = [
                    s for s in SESSIONS if s.path.endswith("/%s" % target) and not s.dead
                ]
            for session in doomed:
                session.close()
            return {"result": {"success": True}}
        if method == "Target.disposeBrowserContext":
            ctx = (params or {}).get("browserContextId")
            for target in [t for t, c in CONTEXT_TABS.items() if c == ctx]:
                del CONTEXT_TABS[target]
            return {"result": {}}
        if method == "Page.getFrameTree":
            if DENY_ON["on"]:
                return {"result": {"frameTree": {"frame": {"id": "MAINFRAME", "url": DENY_URL}}}}
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
            # One isolated world per frame, like Chromium: cu reads the main
            # document in its own isolated world too, so "has a contextId"
            # does not mean "is the iframe" -- the id says which frame it is.
            frame = (params or {}).get("frameId")
            return {"result": {"executionContextId": CHILD_CONTEXT if frame == "CHILD1" else 5}}
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
            if "cuAgentSnapshot" in expr:
                # The walk: in an iframe's isolated world it shows the frame,
                # in the main world the page itself. (The walk embeds the
                # registry, which mentions elementFromPoint, so this must be
                # checked before the click-point branch below.)
                if (params or {}).get("contextId") == CHILD_CONTEXT:
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
                if (params or {}).get("contextId") == CHILD_CONTEXT:
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

    def fetch_for(self, method, params):
        """The `Fetch` domain: the basic-auth responder's whole world.

        `Fetch.enable` arms this connection for the hosts its patterns cover
        (`handleAuthRequests` is what makes Chromium report challenges at
        all), `Fetch.disable` gives the interception back, and the two answer
        commands record what was sent and release whatever the fake is holding
        open on the client's behalf.
        """
        session = self.session
        if method == "Fetch.enable":
            hosts, wilds = set(), set()
            for pattern in params.get("patterns") or []:
                host, wild = pattern_host(pattern.get("urlPattern", ""))
                if not host:
                    continue
                (wilds if wild else hosts).add(host)
            session.arm(hosts, wilds)
            FETCH_LOG.append(
                {
                    "session": session.id,
                    "method": method,
                    "handleAuthRequests": bool(params.get("handleAuthRequests")),
                    "hosts": sorted(hosts),
                    "wildcards": sorted(wilds),
                }
            )
            return {"result": {}}
        if method == "Fetch.disable":
            session.disarm()
            FETCH_LOG.append({"session": session.id, "method": method})
            return {"result": {}}
        if method == "Fetch.continueWithAuth":
            answer = params.get("authChallengeResponse") or {}
            FETCH_LOG.append(
                {
                    "session": session.id,
                    "method": method,
                    "requestId": params.get("requestId", ""),
                    "response": answer.get("response", ""),
                    "username": answer.get("username", ""),
                    "password": answer.get("password", ""),
                }
            )
            # Credentials in hand: the request is replayed, the document comes
            # back 200, and the navigation this connection was holding open is
            # answered -- both after this command's own reply, which is what
            # the daemon reads the navigation's reply out of.
            nav = session.pending_nav
            if nav and nav["request"] == params.get("requestId"):
                session.pending_nav = None
                self.after.append(
                    {
                        "method": "Network.responseReceived",
                        "params": {
                            "requestId": nav["request"],
                            "frameId": "f",
                            "loaderId": nav["request"],
                            "type": "Document",
                            "response": {
                                "url": nav["url"],
                                "status": 200,
                                "statusText": "",
                                "headers": {"content-type": "text/html"},
                            },
                        },
                    }
                )
                self.after.append({"id": nav["id"], "result": {"frameId": "f", "url": nav["url"]}})
            return {"result": {}}
        if method == "Fetch.continueRequest":
            FETCH_LOG.append(
                {
                    "session": session.id,
                    "method": method,
                    "requestId": params.get("requestId", ""),
                }
            )
            return {"result": {}}
        # An interception command this fake does not model.
        return {"error": {"code": -32601, "message": "%s not implemented" % method}}

    def reply(self, obj):
        self.session.frame(obj)


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
