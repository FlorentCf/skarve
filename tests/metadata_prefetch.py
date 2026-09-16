"""Opt-in prefix seed mechanism/correctness, no timing claims."""
import sys,pathlib,json,threading,asyncio
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
sys.path.insert(0,str(pathlib.Path(__file__).resolve().parents[1]/"bindings/python"))
from raster_engine_lab import Engine,EngineError
path=pathlib.Path(sys.argv[1]);state={"etag":'"fixed"',"ranges":[],"reject_seed":False,"pause_seed":False}
seed_entered=threading.Event();seed_resume=threading.Event()
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def do_HEAD(self):
  self.send_response(200);self.send_header("ETag",state["etag"]);self.send_header("Content-Length",str(path.stat().st_size));self.send_header("Accept-Ranges","bytes");self.end_headers()
 def do_GET(self):
  lo,hi=map(int,self.headers["Range"][6:].split("-"));hi=min(hi,path.stat().st_size-1);state["ranges"].append([lo,hi])
  if (self.headers.get("If-Match") and self.headers["If-Match"]!=state["etag"]) or (state["reject_seed"] and lo==0 and hi==16383):
   self.send_response(412);self.send_header("Content-Length","0");self.end_headers();return
  if state["pause_seed"] and lo==0 and hi==16383:
   seed_entered.set();seed_resume.wait(5)
  self.send_response(206);self.send_header("ETag",state["etag"]);self.send_header("Content-Range",f"bytes {lo}-{hi}/{path.stat().st_size}");self.send_header("Content-Length",str(hi-lo+1));self.end_headers()
  with path.open("rb") as f:f.seek(lo);body=f.read(hi-lo+1)
  try:self.wfile.write(body)
  except (BrokenPipeError,ConnectionResetError):pass
server=ThreadingHTTPServer(("127.0.0.1",0),Handler);thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
def spec(seed=0,**limits):return {"format":"geotiff","location":f"http://127.0.0.1:{server.server_port}/source.tif","http":{"allow_http":True,"metadata_prefetch_bytes":seed,**limits}}
records=[];outputs=[]
try:
 for seed in [0,16384]:
  state["ranges"]=[]
  with Engine() as e:
   with e.infuse(spec(seed)) as s:
    opened=len(state["ranges"]);out=s.read_window([0,0,128,64],[32,0,15])
    outputs.append([(bytes(b["values"]),bytes(b["mask"])) for b in out["bands"]])
    diag=s.inspect()["diagnostics"]
    assert diag["metadata_prefix_seed_bytes"]==seed
    records.append({"seed":seed,"open_gets":opened,"ranges":state["ranges"][:],"diagnostics":diag})
 assert outputs[0]==outputs[1]
 assert records[1]["open_gets"] <= records[0]["open_gets"]
 if "--compact" not in sys.argv:assert records[1]["open_gets"]<records[0]["open_gets"]
 assert [r for r in records[0]["ranges"] if r[1]-r[0]+1>16384]==[r for r in records[1]["ranges"] if r[1]-r[0]+1>16384],"payload demand changed"
 assert records[1]["diagnostics"]["remote"]["cache_contained_hits"]>0
 # No silent cache/cap increase. Default-off cache-disabled opening remains valid.
 with Engine() as e:
  with e.infuse(spec(0,cache_bytes=0)) as s:assert s.inspect()["diagnostics"]["metadata_prefix_seed_bytes"]==0
 for options in [dict(cache_bytes=0),dict(max_download_bytes=8192),dict(max_range_bytes=8192)]:
  with Engine() as e:
   try:e.infuse(spec(16384,**options))
   except EngineError:pass
   else:raise AssertionError("insufficient configured budget accepted")
 state["reject_seed"]=True
 with Engine() as e:
  try:e.infuse(spec(16384))
  except EngineError:pass
  else:raise AssertionError("prefix validator failure accepted")
 state["reject_seed"]=False
 with Engine() as e:
  async def cancel():
   state["pause_seed"]=True
   task=asyncio.create_task(e.call_async({"op":"register_source","id":"cancelled","spec":spec(16384)}))
   assert await asyncio.to_thread(seed_entered.wait,3), "prefix read never entered"
   task.cancel();seed_resume.set()
   try:await task
   except asyncio.CancelledError:pass
   else:raise AssertionError("in-flight cancellation did not propagate")
   state["pause_seed"]=False
  asyncio.run(cancel())
  with e.infuse(spec(16384)) as drained:assert drained.inspect()["diagnostics"]["metadata_prefix_seed_bytes"]==16384
 small=path.parent/"small.tif"
 if small.exists():
  path=small;state["ranges"]=[]
  with Engine() as e:
   with e.infuse(spec(16384)) as s:
    out=s.read_window([0,0,1,1],[0]);assert out["bands"][0]["values"][0]==1
    assert s.inspect()["diagnostics"]["metadata_prefix_seed_bytes"]==small.stat().st_size
    assert state["ranges"][0]==[0,small.stat().st_size-1]
 print(json.dumps({"ok":True,"records":records,"small_object_checked":small.exists()}))
finally:server.shutdown();server.server_close();thread.join()
