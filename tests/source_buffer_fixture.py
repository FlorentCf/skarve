#!/usr/bin/env python3
"""Generate the local analytical fixture consumed by source_buffer_node*.mjs."""
import argparse
from pathlib import Path
import numpy as np
import rasterio
from rasterio.transform import from_origin

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("output", type=Path)
parser.add_argument("--bands", type=int, choices=[3, 37, 40], default=40)
args = parser.parse_args()
if args.output.exists():
    parser.error("output already exists")
args.output.parent.mkdir(parents=True, exist_ok=True)
data = np.arange(args.bands * 19 * 17, dtype="float32").reshape(args.bands, 19, 17)
data[:, 1, 1] = 0
mask = np.full((19, 17), 255, dtype="uint8")
mask[2, 2] = 0
with rasterio.Env(GDAL_TIFF_INTERNAL_MASK=True):
    with rasterio.open(args.output, "w", driver="GTiff", width=17, height=19,
                       count=args.bands, dtype="float32", crs="EPSG:3857",
                       transform=from_origin(0, 19, 1, 1), nodata=0,
                       tiled=True, blockxsize=16, blockysize=16) as dataset:
        dataset.write(data)
        dataset.write_mask(mask)
        dataset.scales = (2.,) * args.bands
        dataset.offsets = (3.,) * args.bands
