"""Qualified HTTP raw windows, validator mutation, and drained cancellation."""
import sys,pathlib,json,threading,asyncio
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
sys.path.insert(0,str(pathlib.Path(__file__).resolve().parents[1]/"bindings/python"))
from raster_engine_lab import Engine,EngineError
path=pathlib.Path(sys.argv[1]);state={"etag":'"fixed"',"requests":0,"bytes":0}
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def do_HEAD(self):
  self.send_response(200);self.send_header("ETag",state["etag"]);self.send_header("Content-Length",str(path.stat().st_size));self.send_header("Accept-Ranges","bytes");self.end_headers()
 def do_GET(self):
  state["requests"]+=1
  if self.headers.get("If-Match")!=state["etag"]:
   self.send_response(412);self.send_header("Content-Length","0");self.end_headers();return
  lo,hi=map(int,self.headers["Range"][6:].split("-"));hi=min(hi,path.stat().st_size-1)
  self.send_response(206);self.send_header("ETag",state["etag"]);self.send_header("Content-Range",f"bytes {lo}-{hi}/{path.stat().st_size}");self.send_header("Content-Length",str(hi-lo+1));self.end_headers()
  with path.open("rb") as f:f.seek(lo);body=f.read(hi-lo+1)
  state["bytes"]+=len(body)
  try:self.wfile.write(body)
  except (BrokenPipeError,ConnectionResetError):pass
server=ThreadingHTTPServer(("127.0.0.1",0),Handler);thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
try:
 with Engine() as e:
  with e.infuse({"format":"geotiff","location":f"http://127.0.0.1:{server.server_port}/raster.tif","http":{"allow_http":True,"max_requests":128,"max_download_bytes":67108864,"max_range_bytes":4194304}}) as s:
   r=s.read_window([0,0,128,64],[32,0,15]);assert len(r["bands"])==3
   for b in r["bands"]:assert b["values"][0]==0 and b["mask"][0]==0
   async def cancel():
    task=asyncio.create_task(s.read_window_async([512,0,128,64],[32,0,15]));await asyncio.sleep(0);task.cancel()
    try:await task
    except asyncio.CancelledError:pass
    else:raise AssertionError("cancel did not propagate")
   asyncio.run(cancel())
   s.read_window([0,0,1,1],[0])
   state["etag"]='"changed"'
   try:s.read_window([0,0,1,1],[0])
   except EngineError:pass
   else:raise AssertionError("changed source accepted")
 print(json.dumps(state))
finally:server.shutdown();server.server_close();thread.join()
