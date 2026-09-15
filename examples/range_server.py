#!/usr/bin/env python3
"""Single-threaded bounded loopback range server for generated fixtures only."""
from contextlib import contextmanager
import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
import re
import threading


class RangeServer(HTTPServer):
    def __init__(self,path):
        self.path=Path(path).resolve(strict=True)
        self.length=self.path.stat().st_size
        assert self.length <= 16*1024**2
        with self.path.open('rb') as stream:
            self.etag='"'+hashlib.file_digest(stream,'sha256').hexdigest()+'"'
        self.records=[];self.lock=threading.Lock();self.body_bytes=0
        super().__init__(('127.0.0.1',0),Handler)
    @property
    def url(self):return f'http://127.0.0.1:{self.server_port}/generated.cog.tif'
    def snapshot(self):
        with self.lock:return [dict(row) for row in self.records]


class Handler(BaseHTTPRequestHandler):
    protocol_version='HTTP/1.1'
    def log_message(self,*_):pass
    def headers_only(self,status,length,content_range=None):
        self.send_response(status);self.send_header('Content-Length',str(length))
        self.send_header('Connection','close');self.send_header('ETag',self.server.etag)
        self.send_header('Accept-Ranges','bytes')
        if content_range:self.send_header('Content-Range',content_range)
        self.end_headers()
    def admitted(self):
        self.connection.settimeout(3)
        if self.path!='/generated.cog.tif' or len(self.server.records)>=256:
            self.headers_only(404,0);return False
        return True
    def do_HEAD(self):
        if not self.admitted():return
        with self.server.lock:self.server.records.append({'method':'HEAD','status':200,'body_bytes':0})
        self.headers_only(200,self.server.length)
    def do_GET(self):
        if not self.admitted():return
        match=re.fullmatch(r'bytes=(\d+)-(\d+)',self.headers.get('Range',''))
        if not match:self.headers_only(400,0);return
        start,end=map(int,match.groups());end=min(end,self.server.length-1)
        if end<start or end-start+1>1024**2 or self.server.body_bytes+end-start+1>8*1024**2:
            self.headers_only(416,0);return
        if self.headers.get('If-Match')!=self.server.etag:
            with self.server.lock:self.server.records.append({'method':'GET','status':412,'body_bytes':0})
            self.headers_only(412,0);return
        count=end-start+1
        with self.server.path.open('rb') as stream:stream.seek(start);body=stream.read(count)
        with self.server.lock:
            self.server.records.append({'method':'GET','status':206,'start':start,'end':end,'body_bytes':len(body)})
            self.server.body_bytes+=len(body)
        self.headers_only(206,count,f'bytes {start}-{end}/{self.server.length}')
        try:self.wfile.write(body)
        except (BrokenPipeError,ConnectionResetError):pass


@contextmanager
def served(path):
    server=RangeServer(path)
    thread=threading.Thread(target=lambda:server.serve_forever(poll_interval=.01),daemon=True)
    thread.start()
    try:yield server
    finally:server.shutdown();thread.join(timeout=3);server.server_close()
