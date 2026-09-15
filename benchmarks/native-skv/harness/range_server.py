"""Instrumented loopback object server for controlled source-cold trials.

It serves only an explicit alias map. Each request records relative start/end,
actual transferred body bytes, conditional header presence and byte interval.
Limits and simulated delay/bandwidth apply to all lanes, including upstream.
"""
from contextlib import contextmanager
import hashlib, re, threading, time
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

class Server(HTTPServer):
    def __init__(self, objects, delay_ms=0, mib_s=0, forbidden=(), max_requests=8192, max_body_bytes=1<<30, verified_sha256=None):
        self.objects={key:Path(value).resolve(strict=True) for key,value in objects.items()}
        self.identities={key:'"'+(verified_sha256[key] if verified_sha256 is not None else hashlib.sha256(path.read_bytes()).hexdigest())+'"' for key,path in self.objects.items()}
        self.generations={key:(path.stat().st_dev,path.stat().st_ino,path.stat().st_size,path.stat().st_mtime_ns) for key,path in self.objects.items()}
        self.delay_ms=delay_ms;self.mib_s=mib_s;self.forbidden=tuple(forbidden)
        self.max_requests=max_requests;self.max_body_bytes=max_body_bytes
        self.started=time.perf_counter();self.records=[];self.body_bytes=0;self.lock=threading.Lock()
        super().__init__(('127.0.0.1',0),Handler)
    def url(self,key):return f'http://127.0.0.1:{self.server_port}/{key}'
    def snapshot(self):
        with self.lock:return [dict(record) for record in self.records]

class Handler(BaseHTTPRequestHandler):
    protocol_version='HTTP/1.1'
    def log_message(self,*args):pass
    def do_HEAD(self):self.perform(True)
    def do_GET(self):self.perform(False)
    def perform(self,head):
        server=self.server;key=self.path[1:];begin=time.perf_counter()
        record={'method':'HEAD' if head else 'GET','object':key,'start_ms':(begin-server.started)*1000,'start_monotonic_ns':time.monotonic_ns(),'body_bytes':0,'if_match':bool(self.headers.get('If-Match'))}
        status=200;first=last=None;body=None
        if len(server.records)>=server.max_requests:status=429
        elif key not in server.objects:status=404
        else:
            path=server.objects[key];length=path.stat().st_size;etag=server.identities[key]
            stat=path.stat()
            if (stat.st_dev,stat.st_ino,stat.st_size,stat.st_mtime_ns)!=server.generations[key]:status=412
            elif self.headers.get('If-Match') not in (None,etag):status=412
            elif not head:
                match=re.fullmatch(r'bytes=(\d+)-(\d*)',self.headers.get('Range',''))
                if not match:status=400
                else:
                    first=int(match.group(1));last=min(int(match.group(2)) if match.group(2) else length-1,length-1)
                    record.update({'first':first,'last':last})
                    if first>last or last-first+1>64<<20 or server.body_bytes+last-first+1>server.max_body_bytes:status=416
                    elif any(k==key and first<=b and last>=a for k,a,b in server.forbidden):status=403
                    else:
                        with path.open('rb') as stream:stream.seek(first);body=stream.read(last-first+1)
                        assert len(body)==last-first+1
                        status=206
        time.sleep(server.delay_ms/1000)
        self.send_response(status);self.send_header('Connection','close')
        if key in server.objects:self.send_header('ETag',server.identities[key]);self.send_header('Accept-Ranges','bytes')
        if status==206:self.send_header('Content-Range',f'bytes {first}-{last}/{length}')
        self.send_header('Content-Length',str(length if head and status==200 else len(body) if body else 0));self.end_headers()
        if body:
            try:
                for offset in range(0,len(body),65536):
                    piece=body[offset:offset+65536]
                    if server.mib_s:time.sleep(len(piece)/(server.mib_s*(1<<20)))
                    self.wfile.write(piece);self.wfile.flush();record['body_bytes']+=len(piece)
            except (BrokenPipeError,ConnectionResetError):record['peer_closed']=True
        record.update({'status':status,'end_ms':(time.perf_counter()-server.started)*1000,'end_monotonic_ns':time.monotonic_ns()})
        with server.lock:server.body_bytes+=record['body_bytes'];server.records.append(record)

@contextmanager
def served(objects,**options):
    server=Server(objects,**options);thread=threading.Thread(target=lambda:server.serve_forever(poll_interval=.005),daemon=True);thread.start()
    try:yield server
    finally:server.shutdown();thread.join(5);server.server_close()
