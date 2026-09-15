#!/usr/bin/env python3
"""Independent bounded SKV v0 structural and typed-sample oracle.

This decoder follows docs/skv-format-v0.md and never loads Skarve or a Rust
parser. Test-only dependency: blake3==1.0.8; optional sample comparison uses
NumPy/Rasterio. Full verification intentionally reads every directory/payload.
It is not a selective serving implementation and is never a timed query lane.
"""
from __future__ import annotations
import argparse, hashlib, json, math, os, resource, struct, zlib
from dataclasses import dataclass
from collections import OrderedDict
from pathlib import Path
from blake3 import blake3

HEADER=16384
PAGE=8240
RECORD=128
MAX_FILE=8<<30
MAX_GROUP=3_276_800
GROUP_CACHE=256<<20
TYPES={'byte':('u1',1),'int8':('i1',1),'u_int16':('<u2',2),'int16':('<i2',2),
       'u_int32':('<u4',4),'int32':('<i4',4),'float32':('<f4',4),'float64':('<f8',8)}

def require(condition,message):
    if not condition:raise ValueError(message)

def inflate(data,expected,limit):
    require(0<=expected<=limit,'Decoded size exceeds bound')
    decoder=zlib.decompressobj();value=decoder.decompress(data,expected+1)
    require(len(value)==expected and decoder.eof and not decoder.unused_data and not decoder.unconsumed_tail,'Invalid/trailing/oversize zlib stream')
    return value

@dataclass(frozen=True)
class Entry:
    id:int
    offset:int
    encoded:int
    decoded:int
    sample_bytes:int
    codec:int
    flags:int
    checksum:bytes
    sum_main:float
    sum_correction:float
    count:int
    minimum:float
    maximum:float
    group_leader:int
    group_count:int

class Oracle:
    def __init__(self,path):
        self.path=Path(path);self.size=self.path.stat().st_size
        require(HEADER<=self.size<=MAX_FILE,'Invalid object size')
        self.stream=self.path.open('rb');header=self.read(0,HEADER)
        require(header[:8]==b'SKVRAST\0','Invalid magic')
        major,flags,bootstrap,codec,encoded,decoded=struct.unpack_from('<6I',header,8)
        require(major==0 and flags&~15==0 and bootstrap==HEADER and codec==1,'Unsupported bootstrap fields')
        require(0<encoded<=HEADER-96 and 0<decoded<=65536,'Invalid metadata length')
        require(blake3(header[:-32]).digest()==header[-32:],'Bootstrap checksum failure')
        require(not any(header[64+encoded:-32]),'Nonzero bootstrap padding')
        length,directory,pages,payload=struct.unpack_from('<4Q',header,32)
        require(length==self.size and directory==HEADER,'Incorrect object/directory length')
        require(0<pages<=4096 and payload==HEADER+pages*PAGE and payload<=length,'Invalid page count/first payload or bounded oracle limit (262144 records)')
        self.metadata=json.loads(inflate(header[64:64+encoded],decoded,65536))
        require(isinstance(self.metadata,dict),'Metadata must be an object')
        m=self.metadata;grid=m['grid'];raw=m['raw_metadata'];self.bands=raw['bands']
        require(set(m)-{'predictor','payload_layout'}=={'version','grid','raw_metadata','chunk_edge','band_group','codec','compression_level','summaries','hierarchy','numerical_schema','build_id','original_source_id','original_identity','logical_digest','directory_digest','summary_disabled_reason'},'Unknown/missing metadata fields')
        self.predictor=m.get('predictor','none')
        require(self.predictor in ('none','byte_delta_v1'),'Unsupported predictor')
        require(bool(flags&2)==(self.predictor=='byte_delta_v1'),'Conflicting predictor capability')
        self.payload_layout=m.get('payload_layout','band')
        require(self.payload_layout in ('band','row_group_v1'),'Unsupported payload layout')
        require(bool(flags&8)==(self.payload_layout=='row_group_v1'),'Conflicting row group capability')
        self.group_cache=OrderedDict();self.group_cache_bytes=0;self.group_cache_peak_bytes=0
        overview=raw.get('source_overview')
        require(overview is None or type(overview) is int and 0<=overview<64,'Unsupported source overview provenance')
        require(bool(flags&4)==(overview is not None),'Conflicting source overview capability')
        require(set(raw)-{'source_overview'}=={'bands','pixel_convention','source_band_count'} and set(grid)=={'width','height','transform','crs'},'Unknown/missing raw metadata or grid fields')
        require(m['version']==0 and m['chunk_edge'] in (64,128,256),'Metadata version/chunk edge')
        require(1<=len(self.bands)<=64 and 1<=m['band_group']<=64,'Band/group limits')
        require(m['summaries']==bool(flags&1),'Conflicting summary capability')
        require(all(b['scalar_type'] in TYPES for b in self.bands),'Unsupported raw sample type')
        require(0<grid['width'] and 0<grid['height'] and grid['width']*grid['height']<=1<<31,'Invalid dimensions')
        transform=grid['transform'];require(len(transform)==6 and all(math.isfinite(v) for v in transform) and transform[1]>0 and transform[5]<0 and transform[2]==transform[4]==0,'Unsupported transform')
        require(m['hierarchy']['tile_edge']==m['chunk_edge'],'Hierarchy/chunk edge mismatch')
        edge=m['chunk_edge'];nx=(grid['width']+edge-1)//edge;ny=(grid['height']+edge-1)//edge;offset=0;levels=[]
        while True:
            levels.append([nx,ny,offset]);offset+=nx*ny
            if nx==ny==1:break
            nx=(nx+1)//2;ny=(ny+1)//2
        require(m['hierarchy']['levels']==levels,'Inconsistent hierarchy dimensions')
        self.levels=levels;self.expected_records=offset*len(self.bands)
        require(pages==(self.expected_records+63)//64,'Page count/hierarchy mismatch')
        self.flags=flags;self.first_payload=payload;self.page_count=pages
        self.records=[];self.directory_digest=blake3()
        for page_id in range(pages):
            page=self.read(HEADER+page_id*PAGE,PAGE);self.directory_digest.update(page)
            require(page[:4]==b'SKVP','Invalid page magic')
            version,occupied,identity=struct.unpack_from('<HHQ',page,4)
            require(version==0 and identity==page_id and 1<=occupied<=64,'Invalid directory prefix')
            require(occupied==64 or page_id==pages-1,'Partially occupied nonfinal page')
            require(blake3(page[:-32]).digest()==page[-32:],'Directory checksum failure')
            require(not any(page[16+occupied*RECORD:-32]),'Nonzero final directory slots')
            for slot in range(occupied):
                record=page[16+slot*RECORD:16+(slot+1)*RECORD]
                offset,encoded,decoded,samples,codec,flags=struct.unpack_from('<QIIQII',record)
                identity=struct.unpack_from('<Q',record,104)[0]
                require(identity==len(self.records),'Wrong record identity/order')
                leader,group_count=struct.unpack_from('<QI',record,112)
                require(not any(record[124:]) and flags&~3==0,'Record flags/reserved bytes')
                if self.payload_layout=='band' or not flags&1:require(leader==group_count==0,'Unexpected group descriptor')
                main,correction,count,minimum,maximum=struct.unpack_from('<ddQdd',record,64)
                require(all(math.isfinite(x) for x in (main,correction,minimum,maximum)),'Nonfinite summary')
                if flags&2:
                    require(count>0 and minimum<=maximum or count==0 and main==correction==minimum==maximum==0,'Malformed summary state')
                else:require(main==correction==count==minimum==maximum==0,'Summary bytes present without summary flag')
                if flags&1:
                    encoded_limit=MAX_GROUP if self.payload_layout=='row_group_v1' else 1<<20
                    require(codec in (0,1) and self.first_payload<=offset<=self.size and 0<encoded<=min(encoded_limit,self.size-offset),'Invalid leaf interval')
                    if self.payload_layout=='row_group_v1':
                        require(0<samples<decoded<=MAX_GROUP and 1<=group_count<=64,'Invalid grouped leaf decoded sizes')
                        require(encoded<=decoded and (codec!=0 or encoded==decoded),'Invalid grouped codec extent')
                    else:require(0<samples<decoded<=256*256*9 and 0<decoded-samples<=256*256,'Invalid leaf decoded sizes')
                else:require(offset==encoded==decoded==samples==codec==0 and record[32:64]==bytes(32),'Parent contains raw payload')
                self.records.append(Entry(identity,offset,encoded,decoded,samples,codec,flags,record[32:64],main,correction,count,minimum,maximum,leader,group_count))
        intervals=sorted((r.offset,r.offset+r.encoded,r.id) for r in self.records if r.flags&1 and (self.payload_layout=='band' or r.id==r.group_leader))
        require(bool(intervals),'No leaf payloads')
        require(intervals[0][0]==self.first_payload and intervals[-1][1]==self.size,'Payload extent mismatch')
        require(all(a[1]==b[0] for a,b in zip(intervals,intervals[1:])),'Overlapping or unaccounted payload intervals')
        require(len(self.records)==self.expected_records,'Directory record count mismatch')
        require(self.directory_digest.hexdigest()==m['directory_digest'],'Whole directory digest mismatch')
        leaf_records=self.levels[0][0]*self.levels[0][1]*len(self.bands)
        groups={};band=0
        while band<len(self.bands):
            itemsize=TYPES[self.bands[band]['scalar_type']][1]
            cap=min(m['band_group'],MAX_GROUP//(edge*edge*(itemsize+1)))
            require(cap>0,'Full-chunk group cannot fit bound')
            stop=band+1
            while stop<len(self.bands) and stop-band<cap and TYPES[self.bands[stop]['scalar_type']][1]==itemsize:stop+=1
            for member in range(band,stop):groups[member]=(band,stop-band)
            band=stop
        for r in self.records:
            require(bool(r.flags&1)==(r.id<leaf_records),'Leaf flag/hierarchy mismatch')
            if r.flags&1:
                _,_,width,height=self.chunk(r.id//len(self.bands));cells=width*height
                require(r.sample_bytes==cells*TYPES[self.bands[r.id%len(self.bands)]['scalar_type']][1],'Wrong typed edge-chunk sample shape')
                if self.payload_layout=='row_group_v1':
                    node,band=divmod(r.id,len(self.bands));first,count=groups[band];expected=node*len(self.bands)+first
                    require(r.group_leader==expected and r.group_count==count,'Wrong canonical row group membership')
                    require(r.decoded==count*(r.sample_bytes+cells),'Wrong grouped edge-chunk shape')
                    canonical=self.records[expected]
                    require((r.offset,r.encoded,r.decoded,r.sample_bytes,r.codec,r.checksum,r.group_leader,r.group_count)==
                            (canonical.offset,canonical.encoded,canonical.decoded,canonical.sample_bytes,canonical.codec,canonical.checksum,canonical.group_leader,canonical.group_count),'Alias differs from canonical group descriptor')
                else:require(r.decoded==r.sample_bytes+cells,'Wrong typed edge-chunk shape')
                require(r.count<=cells,'Leaf summary count exceeds shape')
            require(not self.flags&1 or r.flags&2,'Header-declared summary capability missing on a record')
        # Parent count and extrema are independent integer/order checks, with
        # no dependency on the writer's compensated summation implementation.
        for level,(nx,ny,first) in enumerate(self.levels[1:],1):
                cnx,cny,cfirst=self.levels[level-1]
                for y in range(ny):
                    for x in range(nx):
                        children=[cfirst+cy*cnx+cx for cy in range(y*2,min(y*2+2,cny)) for cx in range(x*2,min(x*2+2,cnx))]
                        for band in range(len(self.bands)):
                            parent=self.records[(first+y*nx+x)*len(self.bands)+band]
                            if not parent.flags&2:continue
                            states=[self.records[child*len(self.bands)+band] for child in children]
                            require(all(child.flags&2 for child in states),'Parent state depends on absent child state')
                            require(parent.count==sum(child.count for child in states),'Parent summary count differs from child counts')
                            populated=[child for child in states if child.count]
                            require(parent.minimum==(min(c.minimum for c in populated) if populated else 0) and parent.maximum==(max(c.maximum for c in populated) if populated else 0),'Parent summary extrema differ from child extrema')

    def chunk(self,node):
        nx=self.levels[0][0];edge=self.metadata['chunk_edge'];x=(node%nx)*edge;y=(node//nx)*edge
        return x,y,min(edge,self.metadata['grid']['width']-x),min(edge,self.metadata['grid']['height']-y)

    def read(self,offset,length):
        require(0<=offset<=self.size and 0<=length<=self.size-offset,'Read outside object')
        self.stream.seek(offset);value=self.stream.read(length);require(len(value)==length,'Truncated read');return value

    def payload(self,record):
        require(record.flags&1,'Parent has no samples')
        if self.payload_layout=='row_group_v1':return self.group_payload(record)
        encoded=self.read(record.offset,record.encoded)
        require(blake3(struct.pack('<Q',record.id)+encoded).digest()==record.checksum,'Payload checksum/identity failure')
        decoded=encoded if record.codec==0 else inflate(encoded,record.decoded,256*256*9)
        require(len(decoded)==record.decoded,'Payload decoded size mismatch')
        sample=decoded[:record.sample_bytes]
        if self.predictor=='byte_delta_v1':
            # Independent vector prefix sums invert the documented per-plane,
            # per-actual-row byte differences, then restore sample interleaving.
            import numpy as np
            _,_,width,height=self.chunk(record.id//len(self.bands))
            itemsize=TYPES[self.bands[record.id%len(self.bands)]['scalar_type']][1]
            planes=np.frombuffer(sample,dtype='u1').reshape(itemsize,height,width)
            sample=np.cumsum(planes,axis=2,dtype=np.uint32).astype('u1').transpose(1,2,0).tobytes()
        return sample,decoded[record.sample_bytes:]

    def group_payload(self,record):
        """Independent row/plane/x/member decoding, with bounded verifier-only cache."""
        import numpy as np
        leader=record.group_leader
        cached=self.group_cache.pop(leader,None)
        if cached is None:
            encoded=self.read(record.offset,record.encoded)
            prefix=b'SKV-row-group-v1\0'+struct.pack('<QI',leader,record.group_count)+struct.pack('<IQ',record.decoded,record.sample_bytes)
            require(blake3(prefix+encoded).digest()==record.checksum,'Grouped payload checksum/identity failure')
            decoded=encoded if record.codec==0 else inflate(encoded,record.decoded,MAX_GROUP)
            require(len(decoded)==record.decoded,'Grouped payload decoded size mismatch')
            _,_,width,height=self.chunk(leader//len(self.bands));members=record.group_count
            itemsize=TYPES[self.bands[leader%len(self.bands)]['scalar_type']][1]
            split=record.sample_bytes*members
            planes=np.frombuffer(decoded[:split],dtype='u1').reshape(height,itemsize,width,members)
            if self.predictor=='byte_delta_v1':planes=np.cumsum(planes,axis=2,dtype=np.uint32).astype('u1')
            # Restored bytes become member→row→x→scalar-byte, independent of
            # Rust's loop structure. Masks are an unmodified row→x→member tail.
            samples=planes.transpose(3,0,2,1).copy().tobytes()
            masks=np.frombuffer(decoded[split:],dtype='u1').reshape(height,width,members).transpose(2,0,1).copy().tobytes()
            cached=(samples,masks)
            needed=len(samples)+len(masks)
            while self.group_cache and self.group_cache_bytes+needed>GROUP_CACHE:
                _,(old_samples,old_masks)=self.group_cache.popitem(last=False)
                self.group_cache_bytes-=len(old_samples)+len(old_masks)
            self.group_cache_bytes+=needed
            self.group_cache_peak_bytes=max(self.group_cache_peak_bytes,self.group_cache_bytes)
        self.group_cache[leader]=cached
        samples,masks=cached;member=record.id-leader;cells=record.sample_bytes//TYPES[self.bands[record.id%len(self.bands)]['scalar_type']][1]
        return samples[member*record.sample_bytes:(member+1)*record.sample_bytes],masks[member*cells:(member+1)*cells]

    def verify_payloads(self):
        sample_bytes=mask_bytes=0;logical=blake3()
        for record in self.records:
            if record.flags&1:
                values,mask=self.payload(record);sample_bytes+=len(values);mask_bytes+=len(mask)
                logical.update(struct.pack('<Q',record.id));logical.update(values);logical.update(mask)
        require(logical.hexdigest()==self.metadata['logical_digest'],'Logical payload receipt mismatch')
        return {'records':len(self.records),'leaves':sum(bool(r.flags&1) for r in self.records),
                'physical_payloads':len({(r.offset,r.encoded) for r in self.records if r.flags&1}),
                'payload_layout':self.payload_layout,'verifier_group_cache_peak_bytes':self.group_cache_peak_bytes,'verifier_group_cache_limit_bytes':GROUP_CACHE,
                'directory_pages':self.page_count,'directory_blake3':self.directory_digest.hexdigest(),'sample_bytes':sample_bytes,'mask_bytes':mask_bytes,'bytes':self.size}

    def band(self,index):
        """Reconstruct one band only, bounded separately from whole-object parsing."""
        import numpy as np
        info=self.bands[index];grid=self.metadata['grid'];dtype=np.dtype(TYPES[info['scalar_type']][0]);cells=grid['width']*grid['height']
        require(cells*(dtype.itemsize+1)<=768<<20,'Independent reconstruction exceeds 768 MiB band budget')
        values=np.empty((grid['height'],grid['width']),dtype=dtype);masks=np.empty(values.shape,dtype='u1')
        nodes=self.levels[0][0]*self.levels[0][1]
        for node in range(nodes):
            r=self.records[node*len(self.bands)+index];sample,mask=self.payload(r);x,y,width,height=self.chunk(node)
            values[y:y+height,x:x+width]=np.frombuffer(sample,dtype=dtype).reshape(height,width)
            masks[y:y+height,x:x+width]=np.frombuffer(mask,dtype='u1').reshape(height,width)
        return values,masks

    def compare_source(self,path):
        import numpy as np
        import rasterio
        checks=[]
        def bits(value):return struct.unpack('<Q',struct.pack('<d',value))[0]
        overview=self.metadata['raw_metadata'].get('source_overview')
        options={} if overview is None else {'overview_level':overview}
        with rasterio.open(path,**options) as source:
            grid=self.metadata['grid'];require(source.width==grid['width'] and source.height==grid['height'],'Source grid shape differs')
            require(list(source.transform.to_gdal())==grid['transform'],'Source affine differs')
            require(source.crs==rasterio.crs.CRS.from_user_input(grid['crs']),'Source CRS differs')
            require(source.tags().get('AREA_OR_POINT','unspecified')==self.metadata['raw_metadata']['pixel_convention'],'Pixel convention differs')
            for index,info in enumerate(self.bands):
                native=info['original_band_index'];values,masks=self.band(index)
                require(native<source.count,'Original band mapping outside source')
                require(bits(source.scales[native])==info['scale_f64_bits'] and bits(source.offsets[native])==info['offset_f64_bits'],'Scale/offset bits differ')
                no_data=source.nodatavals[native];require((None if no_data is None else bits(no_data))==info['nodata_f64_bits'],'NoData bits differ')
                require((source.units[native] or None)==info['unit'] and (source.descriptions[native] or '')==info['description'],'Band units/description differ')
                for y in range(0,source.height,64):
                    height=min(64,source.height-y);window=rasterio.windows.Window(0,y,source.width,height)
                    raw=source.read(native+1,window=window);mask=source.read_masks(native+1,window=window)
                    require(raw.dtype.newbyteorder('<')==values.dtype,'Scalar type differs')
                    require(raw.astype(values.dtype,copy=False).tobytes()==values[y:y+height].tobytes(),'Raw typed sample payload differs')
                    require(mask.tobytes()==masks[y:y+height].tobytes(),'Independent mask differs')
                checks.append({'band':index,'original_band':native,'samples':values.size,'raw_sha256':hashlib.sha256(values.tobytes()).hexdigest(),'mask_sha256':hashlib.sha256(masks.tobytes()).hexdigest()})
                del values,masks
        return checks

    def close(self):self.stream.close();self.group_cache.clear();self.group_cache_bytes=0
    def __enter__(self):return self
    def __exit__(self,*args):self.close()

def main():
    resource.setrlimit(resource.RLIMIT_AS,(2<<30,2<<30))
    for name in ('OMP_NUM_THREADS','OPENBLAS_NUM_THREADS','MKL_NUM_THREADS'):os.environ[name]='1'
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('path',type=Path);p.add_argument('--source',type=Path);p.add_argument('--output',type=Path);args=p.parse_args()
    with Oracle(args.path) as oracle:
        result={'schema':'skarve_skv_independent_oracle_v1','passed':True,'structure':oracle.verify_payloads(),'metadata':oracle.metadata}
        if args.source:result['source_comparison']=oracle.compare_source(args.source)
    result['peak_rss_bytes']=resource.getrusage(resource.RUSAGE_SELF).ru_maxrss*1024
    result['address_space_limit_bytes']=2<<30
    if args.output:
        require(not args.output.exists(),'No overwrite');args.output.write_text(json.dumps(result,indent=2,allow_nan=False)+'\n')
    print(json.dumps({'passed':True,'structure':result['structure'],'source_bands_verified':len(result.get('source_comparison',[])),'output':str(args.output)} if args.output else result,indent=2,allow_nan=False))

if __name__=='__main__':main()
