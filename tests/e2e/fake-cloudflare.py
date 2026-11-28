#!/usr/bin/env python3
"""The few calls of Cloudflare's API that `moi ssh up` makes, for a machine that has no account:
one tunnel with a remote configuration, one zone, DNS records. State is kept in memory and printed
on request (GET /state), so the test can see what moi sent.

    fake-cloudflare.py PORT ACCOUNT TUNNEL ZONE_NAME
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import parse_qs, urlparse

PORT, ACCOUNT, TUNNEL, ZONE = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
state = {"config": {"ingress": [{"service": "http_status:404"}]}, "records": [], "requests": []}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def answer(self, status, result=None, error=None):
        body = json.dumps({"success": error is None, "errors": [] if error is None else [{"code": 1000, "message": error}], "result": result}).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def handle_any(self):
        url = urlparse(self.path)
        path, query = url.path.removeprefix("/client/v4"), parse_qs(url.query)
        length = int(self.headers.get("Content-Length") or 0)
        body = json.loads(self.rfile.read(length)) if length else None
        state["requests"].append(f"{self.command} {path}")
        tunnel = f"/accounts/{ACCOUNT}/cfd_tunnel/{TUNNEL}"
        if path == "/state":
            return self.answer(200, state)
        if not self.headers.get("Authorization", "").startswith("Bearer "):
            return self.answer(401, error="no token")
        if self.command == "GET" and path == tunnel:
            return self.answer(200, {"id": TUNNEL, "name": "e2e"})
        if path == tunnel + "/configurations":
            if self.command == "PUT":
                state["config"] = body["config"]
            return self.answer(200, {"tunnel_id": TUNNEL, "config": state["config"]})
        if self.command == "GET" and path == "/zones":
            name = query.get("name", [""])[0]
            return self.answer(200, [{"id": "zone1", "name": ZONE}] if name == ZONE else [])
        if path == "/zones/zone1/dns_records":
            if self.command == "POST":
                record = dict(body, id=f"record{len(state['records']) + 1}")
                state["records"].append(record)
                return self.answer(200, record)
            name = query.get("name", [""])[0]
            return self.answer(200, [r for r in state["records"] if r["name"] == name])
        if self.command == "DELETE" and path.startswith("/zones/zone1/dns_records/"):
            state["records"] = [r for r in state["records"] if r["id"] != path.rsplit("/", 1)[1]]
            return self.answer(200, {})
        self.answer(404, error="no such route")

    do_GET = do_PUT = do_POST = do_DELETE = handle_any


HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
