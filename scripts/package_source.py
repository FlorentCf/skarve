#!/usr/bin/env python3
"""Create an audited source-only candidate from clean Git; never publishes or bundles native binaries."""
import argparse,gzip,io,json,tarfile
from pathlib import Path
from package_release import ROOT,VERSION,source_snapshot,sha

def main():
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--output',type=Path,required=True);args=parser.parse_args()
    _,head,files,hashes=source_snapshot()
    if args.output.exists():raise SystemExit('Use a new immutable artifact directory')
    args.output.mkdir(parents=True)
    target=args.output/f'skarve-{VERSION}-source.tar.gz'
    with target.open('xb') as stream,gzip.GzipFile(fileobj=stream,mode='wb',mtime=0,filename='') as compressed,tarfile.open(fileobj=compressed,mode='w') as tar:
        def normalize(info):info.uid=info.gid=0;info.uname=info.gname='';info.mtime=0;return info
        for file in files:tar.add(file,arcname=f'skarve-{VERSION}/'+file.relative_to(ROOT).as_posix(),filter=normalize)
        payload=(json.dumps({'source_commit':head,'source_files_sha256':hashes},indent=2)+'\n').encode()
        info=tarfile.TarInfo(f'skarve-{VERSION}/SOURCE_MANIFEST.json');info.size=len(payload);info.mode=0o644;tar.addfile(info,io.BytesIO(payload))
    assert hashes=={f.relative_to(ROOT).as_posix():sha(f) for f in files},'Source changed during packaging'
    manifest={'schema':'skarve_source_only_candidate_v1','version':VERSION,'source_commit':head,'native_binaries':False,
              'publication':'Not performed; owner approval required','file':target.name,'bytes':target.stat().st_size,'sha256':sha(target)}
    (args.output/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
    (args.output/'SHA256SUMS').write_text(f"{manifest['sha256']}  {target.name}\n")
    print(json.dumps(manifest))
if __name__=='__main__':main()
