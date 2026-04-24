#!/usr/bin/env python3
"""
Jellio/Stremio compatibility shim.

Problem: Jellio returns absolute URLs for posters and stream URLs that may use
`http://<host>` even when the addon is being accessed over HTTPS (e.g. Tailscale Funnel).
Some clients (Samsung Tizen Stremio) won't follow `http -> https` redirects for media
URLs, resulting in empty catalogs and failed playback.

Solution: Proxy the Jellio JSON endpoints and rewrite `http://{Host}` to
`https://{Host}` in JSON responses.

This shim is intentionally narrow: it only rewrites JSON payloads, and it avoids
requesting compressed upstream bodies to keep rewriting simple.
"""

from __future__ import annotations

import http.client
import os
import socket
import sys
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


UPSTREAM = os.environ.get("JELLIO_SHIM_UPSTREAM", "http://127.0.0.1:8096").rstrip("/")
LISTEN = os.environ.get("JELLIO_SHIM_LISTEN", "0.0.0.0:8098")


def _split_listen(value: str) -> tuple[str, int]:
    host, _, port = value.rpartition(":")
    if not host:
        host = "0.0.0.0"
    try:
        return host, int(port)
    except Exception:
        return "0.0.0.0", 8098


def _hop_by_hop_header(name: str) -> bool:
    n = name.lower()
    return n in {
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailers",
        "transfer-encoding",
        "upgrade",
    }


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt: str, *args) -> None:
        # Keep logs simple and stderr-based so they show up in `docker logs`.
        sys.stderr.write("%s - - [%s] %s\n" % (self.client_address[0], self.log_date_time_string(), fmt % args))

    def _proxy(self) -> None:
        upstream = urllib.parse.urlparse(UPSTREAM)
        if upstream.scheme not in ("http", "https"):
            self.send_error(500, "invalid upstream scheme")
            return

        # Read request body (if any)
        body = b""
        try:
            length = int(self.headers.get("Content-Length") or "0")
        except ValueError:
            length = 0
        if length > 0:
            body = self.rfile.read(length)

        # Build upstream request
        path = self.path
        if not path.startswith("/"):
            path = "/" + path

        conn: http.client.HTTPConnection | http.client.HTTPSConnection
        timeout = 20
        if upstream.scheme == "https":
            conn = http.client.HTTPSConnection(upstream.hostname, upstream.port or 443, timeout=timeout)
        else:
            conn = http.client.HTTPConnection(upstream.hostname, upstream.port or 80, timeout=timeout)

        headers = {}
        for k, v in self.headers.items():
            if _hop_by_hop_header(k):
                continue
            # The shim only exists to rewrite JSON; avoid upstream gzip/br so we can safely edit bodies.
            if k.lower() == "accept-encoding":
                continue
            headers[k] = v

        headers["Accept-Encoding"] = "identity"

        try:
            conn.request(self.command, path, body=body if body else None, headers=headers)
            resp = conn.getresponse()
            status = resp.status
            reason = resp.reason
            resp_headers = resp.getheaders()
            resp_body = resp.read()
        except socket.timeout:
            self.send_error(504, "upstream timeout")
            return
        except Exception:
            self.send_error(502, "upstream error")
            return
        finally:
            try:
                conn.close()
            except Exception:
                pass

        # Rewrite JSON bodies: http://<host> -> https://<host>
        content_type = ""
        for k, v in resp_headers:
            if k.lower() == "content-type":
                content_type = v or ""
                break

        forwarded_host = (self.headers.get("X-Forwarded-Host") or "").strip()
        host = forwarded_host or (self.headers.get("Host") or "").strip()
        # Strip any port so replacements match URLs like http://example.com/...
        if host.startswith("[") and "]" in host:
            host = host.split("]", 1)[0] + "]"
        elif ":" in host:
            host = host.split(":", 1)[0]
        if host and "application/json" in content_type.lower() and resp_body:
            try:
                text = resp_body.decode("utf-8")
                needle = f"http://{host}"
                repl = f"https://{host}"
                if needle in text:
                    text = text.replace(needle, repl)
                # Also fix escaped variant if present (paranoia for double-encoded payloads)
                needle2 = needle.replace("/", "\\/")
                repl2 = repl.replace("/", "\\/")
                if needle2 in text:
                    text = text.replace(needle2, repl2)
                resp_body = text.encode("utf-8")
            except Exception:
                # If rewriting fails for any reason, just return original bytes.
                pass

        # Respond
        self.send_response(status, reason)
        for k, v in resp_headers:
            if _hop_by_hop_header(k):
                continue
            # We may have changed the body, so drop upstream length/encoding.
            if k.lower() in ("content-length", "content-encoding"):
                continue
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(resp_body)))
        # Be permissive for TV clients that are picky about CORS.
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Headers", "*")
        self.send_header("Access-Control-Allow-Methods", "GET,HEAD,OPTIONS")
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(resp_body)

    def do_OPTIONS(self) -> None:
        self.send_response(204)
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Headers", "*")
        self.send_header("Access-Control-Allow-Methods", "GET,HEAD,OPTIONS")
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self) -> None:
        self._proxy()

    def do_HEAD(self) -> None:
        self._proxy()


def main() -> int:
    host, port = _split_listen(LISTEN)
    httpd = ThreadingHTTPServer((host, port), Handler)
    sys.stderr.write(f"jellio-shim: listening on http://{host}:{port}, upstream={UPSTREAM}\n")
    httpd.serve_forever()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
