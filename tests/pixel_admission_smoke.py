"""Qualified 33-band PIXEL512 source admission regression; synthetic data only."""
import sys, tempfile, pathlib, json, resource, os
sys.path.insert(0,str(pathlib.Path(__file__).resolve().parents[1]/"bindings/python"))
from raster_engine_lab import Engine, EngineError
import rasterio, numpy as np
from rasterio.transform import from_origin
root=pathlib.Path(sys.argv[1]);root.mkdir(parents=True,exist_ok=True)
if "--generate" in sys.argv:
 for dtype in ["uint32","float32"]:
  a=np.random.default_rng(123).integers(1,1000000,size=(513,1025),dtype=np.uint32).astype(dtype)
  a[0,0]=0
  with rasterio.open(root/(dtype+".tif"),"w",driver="GTiff",width=1025,height=513,count=33,dtype=dtype,tiled=True,blockxsize=512,blockysize=512,compress="DEFLATE",predictor=1,interleave="pixel",nodata=0,crs="EPSG:4326",transform=from_origin(0,50,.01,.01)) as dst:
   for b in range(1,34):dst.write(a+b*(a!=0),b)
 print("generated");sys.exit()
records=[]
for dtype in ["uint32","float32"]:
 with Engine() as e:
  with e.infuse({"format":"geotiff","location":str(root/(dtype+".tif"))}) as source:
   for window in [[0,0,128,64],[384,128,128,64],[512,512,128,1],[900,5,124,32]]*3:
    result=source.read_window(window,[32,0,15],working_bytes=128<<20)
    x,y,w,h=window
    with rasterio.open(root/(dtype+".tif")) as oracle:
     expected=oracle.read([33,1,16],window=rasterio.windows.Window(x,y,w,h))
    for index,b in enumerate(result["bands"]):
     assert bytes(b["values"])==expected[index].tobytes()
     assert np.array_equal(np.asarray(b["mask"]).reshape(h,w),np.where(expected[index]==0,0,255))
    records.append({"type":dtype,"window":window,"reserved":result["reservedBytes"]})
   try:source.read_window([511,511,2,2],[32,0,15],working_bytes=128<<20)
   except EngineError as ex:assert "budget" in str(ex)
   else:raise AssertionError("crossing must retain conservative bound")
# Local generation mutation is rejected after registration; no original fixture changes.
import shutil
copy=root/"mutation.tif";shutil.copyfile(root/"float32.tif",copy)
with Engine() as e:
 with e.infuse({"format":"geotiff","location":str(copy)}) as source:
  with copy.open("ab") as out:out.write(b"changed")
  try:source.read_window([0,0,1,1],[0])
  except EngineError as ex:assert "changed" in str(ex)
  else:raise AssertionError("mutation accepted")
# A genuine independent/internal mask must not enter the tightened contract.
masked=root/"masked.tif";shutil.copyfile(root/"uint32.tif",masked)
with rasterio.Env(GDAL_TIFF_INTERNAL_MASK=True):
 with rasterio.open(masked,"r+") as ds:
  mask=np.full((ds.height,ds.width),255,dtype="uint8");mask[2,2]=0;ds.write_mask(mask)
with Engine() as e:
 with e.infuse({"format":"geotiff","location":str(masked)}) as source:
  assert "raw_window_admission" not in source.inspect()["access_layout"]
  try:source.read_window([0,0,1,1],[0])
  except EngineError as ex:assert "budget" in str(ex)
  else:raise AssertionError("masked layout unexpectedly tightened")
print(json.dumps({"checks":len(records),"records":records,"peak_rss_kib":resource.getrusage(resource.RUSAGE_SELF).ru_maxrss}))
