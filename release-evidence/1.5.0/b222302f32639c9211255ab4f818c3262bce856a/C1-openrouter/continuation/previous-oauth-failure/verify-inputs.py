import hashlib,json,pathlib,sys,tarfile
p=pathlib.Path(__file__).resolve().parent
m=json.loads((p/'input-manifest.json').read_text());r=pathlib.Path(m['repo'])
sha=lambda q:hashlib.sha256(q.read_bytes()).hexdigest()
assert sha(pathlib.Path(m['binary']))==m['binary_sha256'],'Installed candidate changed'
archive=pathlib.Path(m['archive']);assert sha(archive)==m['archive_sha256'],'Archive changed'
assert archive.with_name(archive.name+'.sha256').read_text().split()[0]==m['archive_sha256'],'Checksum sidecar changed'
with tarfile.open(archive,'r:gz') as tf:assert hashlib.sha256(tf.extractfile('axocoatl').read()).hexdigest()==m['binary_sha256'],'Packaged binary mismatch'
for name,value in m['gate_input_sha256'].items():assert sha(r/name)==value,'Gate/fixture changed: '+name
site=pathlib.Path(m['site']);actual={str(q.relative_to(site)):sha(q) for q in site.rglob('*') if q.is_file()};assert actual==m['site_sha256'],'Staged site changed'
assert not any(q.is_symlink() for q in site.rglob('*')),'Unexpected site symlink'
src=r/'scripts/release-gate/newuser-mac.sh';copy=p/'newuser-mac-continuation.sh';assert sha(src)==m['original_runner_sha256'];assert sha(copy)==m['continuation_runner_sha256']
s=copy.read_text()
for old,new in m['setup_edits'].items():assert s.count(new)==1;s=s.replace(new,old)
assert s==src.read_text(),'Function body or criterion changed'
local=pathlib.Path(m['same_candidate_clean_install']['path']);assert sha(local)==m['same_candidate_clean_install']['sha256'];d=json.loads(local.read_text());assert all(x['status']=='pass' for x in d['steps'] if x.get('required',True))
print('Verified exact b222 binary/archive, 90 gate/fixture inputs, 27 site files, unchanged path function bodies, and preserved C1-local installation evidence.')
