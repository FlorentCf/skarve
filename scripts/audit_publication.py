#!/usr/bin/env python3
"""Read-only bounded publication audit of source, every reachable Git blob and archives.

This is a targeted secret/private-content audit, not a proof of arbitrary code safety.
Findings report rule and location only, never the matching secret text.
"""
import argparse, gzip, hashlib, io, json, re, subprocess, tarfile, zipfile
from pathlib import Path, PurePosixPath

RULES = {
    "private-user-path": re.compile(rb"/(?:home|Users)/[A-Za-z][A-Za-z0-9_.-]*/|[A-Za-z]:\\+Users\\+"),
    "private-key": re.compile(rb"-----BEGIN (?:RSA |EC |OPENSSH |DSA )?PRIVATE KEY-----"),
    "github-token": re.compile(rb"(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{40,})"),
    "aws-access-key": re.compile(rb"(?:AKIA|ASIA)[A-Z0-9]{16}"),
    "signed-credential": re.compile(rb"X-Amz-(?:Credential|Signature|Security-Token)=[^\s\"&<>]{12,}",re.I),
    "private-cloud-locator": re.compile(rb"horizonmapper(?:worldpop)|[a-z0-9]{20,}\.r2\.cloudflarestorage\.com",re.I),
}
SKIP = {'.git','target','dist','scratch','node_modules','.venv','__pycache__','.pytest_cache','.hypothesis'}
MAX_MEMBER = 128 << 20
MAX_EXPANDED = 256 << 20

def digest(data): return hashlib.sha256(data).hexdigest()
def git(root,*args): return subprocess.check_output(['git','-C',str(root),*args])
def safe_member(name):
    p=PurePosixPath(name)
    return not p.is_absolute() and '..' not in p.parts and '\\' not in name

class Audit:
    def __init__(self): self.findings=[];self.files=[];self.seen=set();self.expanded=0
    def finding(self,location,rule): self.findings.append({'location':location,'rule':rule})
    def scan(self,data,location,depth=0):
        if len(data)>MAX_MEMBER: self.finding(location,'member-size-limit'); return
        checksum=digest(data);self.files.append({'location':location,'bytes':len(data),'sha256':checksum})
        if checksum in self.seen:return
        self.seen.add(checksum)
        for rule,pattern in RULES.items():
            if pattern.search(data):self.finding(location,rule)
        if depth>4:self.finding(location,'archive-depth-limit');return
        # Inspect decompressed content, including archive metadata and nested reports.
        if data.startswith(b'\x1f\x8b'):
            try:
                with gzip.GzipFile(fileobj=io.BytesIO(data)) as stream:expanded=stream.read(MAX_MEMBER+1)
                self.expanded+=len(expanded)
                if self.expanded>MAX_EXPANDED:self.finding(location,'expanded-byte-limit');return
                self.scan(expanded,location+'!gzip',depth+1)
            except Exception:self.finding(location,'invalid-gzip')
        elif data.startswith(b'PK\x03\x04'):
            try:
                with zipfile.ZipFile(io.BytesIO(data)) as archive:
                    for member in archive.infolist():
                        if not safe_member(member.filename):self.finding(location,'unsafe-archive-path');continue
                        if member.is_dir():continue
                        if member.file_size>MAX_MEMBER:self.finding(location,'member-size-limit');continue
                        self.expanded+=member.file_size
                        if self.expanded>MAX_EXPANDED:self.finding(location,'expanded-byte-limit');break
                        self.scan(archive.read(member),location+'!'+member.filename,depth+1)
            except Exception:self.finding(location,'invalid-zip')
        elif len(data)>512 and data[257:262]==b'ustar':
            try:
                with tarfile.open(fileobj=io.BytesIO(data),mode='r:') as archive:
                    for member in archive:
                        if not safe_member(member.name):self.finding(location,'unsafe-archive-path');continue
                        if member.isdir():continue
                        if not member.isfile():self.finding(location,'nonregular-archive-member');continue
                        if member.size>MAX_MEMBER:self.finding(location,'member-size-limit');continue
                        stream=archive.extractfile(member)
                        if stream is not None:self.scan(stream.read(MAX_MEMBER+1),location+'!'+member.name,depth+1)
            except Exception:self.finding(location,'invalid-tar')

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root',type=Path,default=Path(__file__).resolve().parents[1])
    parser.add_argument('--artifact',type=Path,action='append',default=[])
    parser.add_argument('--history',action='store_true')
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args();root=args.root.resolve();audit=Audit()
    if args.artifact:
        for p in args.artifact:
            audit.expanded=0
            if p.is_symlink() or not p.is_file():audit.finding(p.name,'nonregular-input');continue
            audit.scan(p.read_bytes(),'artifact/'+p.name)
    else:
        for p in sorted(root.rglob('*')):
            rel=p.relative_to(root)
            if any(part in SKIP for part in rel.parts):continue
            if p.is_symlink():audit.finding(rel.as_posix(),'source-symlink');continue
            if p.is_file():
                if p.name=='.env' or p.name.startswith('.env.') or p.suffix in {'.so','.dll','.whl','.tgz'}:
                    audit.finding(rel.as_posix(),'unexpected-source-file')
                audit.expanded=0;audit.scan(p.read_bytes(),rel.as_posix())
    refs=[];commits=[]
    if args.history:
        refs=git(root,'for-each-ref','--format=%(refname) %(objectname)').decode().splitlines()
        commits=git(root,'rev-list','--all').decode().splitlines()
        for line in git(root,'rev-list','--objects','--all').decode().splitlines():
            oid,_,path=line.partition(' ')
            if git(root,'cat-file','-t',oid).strip()==b'blob':
                audit.expanded=0;audit.scan(git(root,'cat-file','blob',oid),'git/'+oid+'/'+path)
        for oid in commits:audit.scan(git(root,'cat-file','commit',oid),'commit/'+oid)
        for line in refs:
            ref,oid=line.split(' ',1)
            if git(root,'cat-file','-t',oid).strip()==b'tag':audit.scan(git(root,'cat-file','tag',oid),'tag/'+ref)
    result={'schema':'skarve_publication_content_audit_v1','passed':not audit.findings,
            'scope':'Targeted source/private-locator/known-secret scan, nested gzip/tar/zip inspection; all reachable refs/blobs and commit/tag metadata when --history. Does not claim a complete vulnerability audit or legal clearance.',
            'history_refs':refs,'history_commits':commits,'scanned_locations':len(audit.files),
            'unique_contents':len(audit.seen),'findings':audit.findings,'files':audit.files}
    if args.output.exists():raise SystemExit('Use a new immutable audit receipt path')
    args.output.parent.mkdir(parents=True,exist_ok=True);args.output.write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps({k:result[k] for k in ['passed','scanned_locations','unique_contents','findings']}))
    return 0 if result['passed'] else 1
if __name__=='__main__':raise SystemExit(main())
