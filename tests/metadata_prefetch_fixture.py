"""Generate metadata-rich COG from the synthetic pixel_admission_smoke fixture."""
import sys,pathlib,shutil,rasterio
from rasterio.shutil import copy
root=pathlib.Path(sys.argv[1]);shutil.copyfile(root/"float32.tif",root/"rich-base.tif")
with rasterio.open(root/"rich-base.tif","r+") as ds:
 for b in range(1,34):
  ds.set_band_description(b,"synthetic_population_band_"+str(b))
  ds.update_tags(b,STATISTICS_MINIMUM="0",STATISTICS_MAXIMUM="1000033",STATISTICS_MEAN="500000",STATISTICS_STDDEV="50000")
copy(root/"rich-base.tif",root/"rich.cog.tif",driver="COG",BLOCKSIZE=512,COMPRESS="DEFLATE",OVERVIEWS="NONE")

import numpy as np
from rasterio.transform import from_origin
with rasterio.open(root/"small.tif","w",driver="GTiff",width=2,height=2,count=1,dtype="uint32",crs="EPSG:3857",transform=from_origin(0,2,1,1)) as ds:ds.write(np.array([[1,2],[3,4]],dtype="uint32"),1)
