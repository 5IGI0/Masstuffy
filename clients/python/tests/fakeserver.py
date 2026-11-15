"""A threaded HTTP/1.1 fake server with scripted routes, for the unit tests."""

import json
import re
import socket
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Req:
    def __init__(self, method, path, query, headers, body, complete):
        self.method = method
        self.path = path
        self.query = query          # dict of lists
        self.headers = headers
        self.body = body
        self.complete = complete    # False when the client cut the body short
        self.raw_path = ""

    def q(self, name, default=None):
        v = self.query.get(name)
        return v[0] if v else default

    def json(self):
        return json.loads(self.body)


class Resp:
    def __init__(self, status=200, headers=None, body=b"", close=False, chunked=False, read_body=True):
        self.status = status
        self.headers = dict(headers or {})
        self.body = body
        self.close = close
        self.chunked = chunked
        self.read_body = read_body


class Drop:
    """Close the socket without answering (after reading the request body)."""


class Hang:
    def __init__(self, seconds):
        self.seconds = seconds


def json_resp(status, obj, headers=None):
    h = {"Content-Type": "application/json"}
    h.update(headers or {})
    return Resp(status, h, json.dumps(obj).encode())


def error_resp(status, kind, code, message="m", details=None, retry_after=None):
    err = {"kind": kind, "code": code, "message": message}
    if details is not None:
        err["details"] = details
    h = {}
    if retry_after is not None:
        h["Retry-After"] = str(retry_after)
    return json_resp(status, {"error": err}, h)


class _Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def setup(self):
        super().setup()
        self.server.fake._count_conn()

    def _read_body(self):
        h = self.headers
        if "chunked" in h.get("Transfer-Encoding", "").lower():
            out = bytearray()
            while True:
                line = self.rfile.readline()
                if not line:
                    return bytes(out), False
                try:
                    n = int(line.strip().split(b";")[0], 16)
                except ValueError:
                    return bytes(out), False
                if n == 0:
                    self.rfile.readline()
                    return bytes(out), True
                data = self.rfile.read(n)
                out += data
                if len(data) < n:
                    return bytes(out), False
                self.rfile.read(2)
        n = int(h.get("Content-Length") or 0)
        data = self.rfile.read(n) if n else b""
        return data, len(data) == n

    def _handle(self):
        fake = self.server.fake
        split = urllib.parse.urlsplit(self.path)
        query = urllib.parse.parse_qs(split.query, keep_blank_values=True)
        route = fake._find(self.command, urllib.parse.unquote(split.path))
        early = route is not None and isinstance(route[2], Resp) and not route[2].read_body
        if early:
            body, complete = b"", True
        else:
            body, complete = self._read_body()
        req = Req(self.command, urllib.parse.unquote(split.path), query, self.headers, body, complete)
        req.raw_path = self.path
        with fake.lock:
            fake.requests.append(req)
        if route is None:
            resp = error_resp(404, "not_found", "no_route")
        else:
            resp = fake._respond(route, req)
        if isinstance(resp, Drop):
            self.close_connection = True
            try:
                self.connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            return
        if isinstance(resp, Hang):
            time.sleep(resp.seconds)
            self.close_connection = True
            return
        if not complete:
            self.close_connection = True
            return
        self.send_response(resp.status)
        for k, v in resp.headers.items():
            self.send_header(k, v)
        body = resp.body
        if isinstance(body, (bytes, bytearray)) and not resp.chunked:
            self.send_header("Content-Length", str(len(body)))
        else:
            self.send_header("Transfer-Encoding", "chunked")
        if resp.close or early:
            self.close_connection = True
            if early:
                self.send_header("Connection", "close")
        self.end_headers()
        try:
            if isinstance(body, (bytes, bytearray)) and not resp.chunked:
                self.wfile.write(body)
            else:
                chunks = [body] if isinstance(body, (bytes, bytearray)) else body
                for c in chunks:
                    if c:
                        self.wfile.write(b"%x\r\n" % len(c) + c + b"\r\n")
                self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        except OSError:
            self.close_connection = True

    do_GET = do_POST = do_PATCH = do_DELETE = do_PUT = do_HEAD = _handle


class _Server(ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 64

    def handle_error(self, request, client_address):
        pass


class FakeServer:
    def __init__(self):
        self.requests = []
        self.connections = 0
        self.lock = threading.Lock()
        self._routes = []
        self._httpd = _Server(("127.0.0.1", 0), _Handler)
        self._httpd.fake = self
        self._thread = threading.Thread(target=self._httpd.serve_forever, kwargs={"poll_interval": 0.05},
                                        daemon=True)

    @property
    def url(self):
        return "http://127.0.0.1:%d" % self._httpd.server_address[1]

    def _count_conn(self):
        with self.lock:
            self.connections += 1

    def route(self, method, path_regex, responder):
        """responder: callable(Req) -> Resp|Drop|Hang, or a Resp, or a list of those (consumed in order,
        the last one repeats)."""
        state = {"i": 0}
        self._routes.append((method, re.compile(path_regex), responder, state))

    def _find(self, method, path):
        for m, rx, resp, state in self._routes:
            if m == method and rx.fullmatch(path):
                return (m, rx, resp, state)
        return None

    def _respond(self, route, req):
        _, rx, responder, state = route
        if isinstance(responder, list):
            with self.lock:
                i = min(state["i"], len(responder) - 1)
                state["i"] += 1
            responder = responder[i]
        if callable(responder):
            return responder(req)
        return responder

    def start(self):
        self._thread.start()
        return self

    def stop(self):
        self._httpd.shutdown()
        self._httpd.server_close()
        self._thread.join(5)

    def __enter__(self):
        return self.start()

    def __exit__(self, *a):
        self.stop()
