#!/usr/bin/env python3
"""Controlled localhost-only immutable range server for tests, never production data."""
from __future__ import annotations
import argparse
import hashlib
import json
import re
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


class RangeServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, path: Path, mode: str = "ok", port: int = 0):
        self.path = path.resolve(strict=True)
        self.mode = mode
        self.length = self.path.stat().st_size
        with self.path.open("rb") as stream:
            self.etag = '"' + hashlib.file_digest(stream, "sha256").hexdigest() + '"'
        self.records: list[dict] = []
        self.record_lock = threading.Lock()
        super().__init__(("127.0.0.1", port), Handler)

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server_address[1]}/source.tif"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def send_headers(self, status, length, *, content_range=None, etag=None):
        self.send_response(status)
        self.send_header("Content-Length", str(length))
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("ETag", etag or self.server.etag)
        self.send_header("Connection", "close")
        if content_range:
            self.send_header("Content-Range", content_range)
        self.end_headers()

    def record(self, **fields):
        with self.server.record_lock:
            self.server.records.append({"method": self.command, **fields})

    def do_HEAD(self):
        if self.path != "/source.tif":
            self.send_headers(404, 0)
            return
        self.record(status=200, sent_bytes=0)
        self.send_headers(200, self.server.length)

    def do_GET(self):
        if self.path != "/source.tif":
            self.send_headers(404, 0)
            return
        matched = re.fullmatch(r"bytes=(\d+)-(\d+)", self.headers.get("Range", ""))
        if not matched:
            self.record(status=400, sent_bytes=0, error="non-range GET")
            self.send_headers(400, 0)
            return
        start, end = map(int, matched.groups())
        end = min(end, self.server.length - 1)
        if end < start or end - start + 1 > 4 * 1024 * 1024:
            self.record(status=416, sent_bytes=0)
            self.send_headers(416, 0)
            return
        if self.headers.get("If-Match") != self.server.etag:
            self.record(status=412, sent_bytes=0)
            self.send_headers(412, 0)
            return
        mode = self.server.mode
        if mode == "ignored":
            # Intentionally advertises a huge ignored-Range body, sends no body.
            # The client must reject headers immediately without trying to download.
            self.record(status=200, start=start, end=end, sent_bytes=0)
            self.send_headers(200, self.server.length)
            return
        if mode == "missing":
            self.record(status=503, start=start, end=end, sent_bytes=0)
            self.send_headers(503, 0)
            return
        size = end - start + 1
        content_range = f"bytes {start}-{end}/{self.server.length}"
        if mode == "wrong_range":
            content_range = f"bytes {start + 1}-{end + 1}/{self.server.length}"
        self.send_headers(206, size, content_range=content_range,
                          etag='"changed"' if mode == "changed" else None)
        sent = 0
        try:
            with self.server.path.open("rb") as stream:
                stream.seek(start)
                remaining = size // 2 if mode == "truncated" else size
                while remaining:
                    chunk = stream.read(min(65536, remaining))
                    if not chunk:
                        break
                    self.wfile.write(chunk)
                    sent += len(chunk)
                    remaining -= len(chunk)
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            self.record(status=206, start=start, end=end, sent_bytes=sent, mode=mode)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("file", type=Path)
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--mode", choices=["ok", "ignored", "changed", "missing", "wrong_range", "truncated"], default="ok")
    args = parser.parse_args()
    server = RangeServer(args.file, args.mode, args.port)
    print(json.dumps({"url": server.url, "length": server.length, "mode": args.mode}), flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
        print(json.dumps({"requests": server.records}), flush=True)


if __name__ == "__main__":
    main()
