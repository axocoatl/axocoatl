#!/usr/bin/env python3
"""Stage reviewed completed gate paths without signing or modifying any input.
This validates hashes, signatures, exact candidate criteria and preservation. It
never derives success from supplemental checks or decides whether an observation
was genuine: each replacement needs a separately reviewed, hash-pinned record.
"""
import argparse, hashlib, json, pathlib, re, subprocess, sys
from datetime import datetime, timezone
C='b222302f32639c9211255ab4f818c3262bce856a'
V='1.5.0'
NODE='/Users/erick/.local/share/fnm/node-versions/v22.23.2/installation/bin/node'
ALLOWED={'D1','C1-codex','C1-openrouter'}
def need(ok,msg):
 if not ok: raise ValueError(msg)
def sha(b): return hashlib.sha256(b).hexdigest()
def encode(v): return (json.dumps(v,indent=2,ensure_ascii=False)+'\n').encode()
def safe(p):
 need(isinstance(p,str) and p and not p.startswith('/') and '\\' not in p and all(x not in ('','.','..') for x in p.split('/')),'unsafe relative path')
 return p
def inventory(root):
 need(root.is_dir(),'missing source directory')
 files={}
 for p in sorted(root.rglob('*')):
  need(not p.is_symlink(),'symlink refused: '+str(p))
  if p.is_file():
   need(p.stat().st_size<=64*1024*1024,'file too large')
   files[safe(str(p.relative_to(root)))]=p.read_bytes()
 return files
def validate_inventory(files,pin):
 need('manifest.json' in files and sha(files['manifest.json'])==pin,'manifest pin mismatch')
 m=json.loads(files['manifest.json']);need(m.get('schema')=='axocoatl.release-evidence/1' and m.get('version')==V and m.get('commit')==C,'manifest candidate/schema mismatch')
 listed=set()
 for e in m.get('files',[]):
  name=safe(e['path']);need(name not in listed and name not in ('manifest.json','manifest.dsse.json'),'duplicate/reserved manifest entry');listed.add(name)
  need(name in files and sha(files[name])==e['sha256'] and len(files[name])==e['bytes'],'file hash/length mismatch: '+name)
 need(set(files)-{'manifest.json','manifest.dsse.json'}==listed,'unlisted or missing files')
 return m
def run(argv):
 r=subprocess.run(argv,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
 need(r.returncode==0,r.stderr.decode() or r.stdout.decode());return r.stdout

def main():
 ap=argparse.ArgumentParser();ap.add_argument('--repo',required=True);ap.add_argument('--plan',required=True);ap.add_argument('--out');ap.add_argument('--check-only',action='store_true');args=ap.parse_args()
 repo=pathlib.Path(args.repo).resolve();plan_path=pathlib.Path(args.plan).resolve();plan=json.loads(plan_path.read_text())
 need(plan.get('schema')=='axocoatl.recovery.reviewed-completions-plan/1' and plan.get('candidate_commit')==C and plan.get('version')==V,'plan candidate/schema mismatch')
 src=pathlib.Path(plan['source_bundle']).resolve();old=inventory(src);m=validate_inventory(old,plan['source_manifest_sha256'])
 need('manifest.dsse.json' in old and sha(old['manifest.dsse.json'])==plan['source_dsse_sha256'],'signature pin mismatch')
 module=repo/'scripts/release-gate/evidence.mjs';docs=repo/'docs/RELEASE_GATE.md';keys=repo/'.axocoatl/gate-keys'
 for rel in ['scripts/release-gate/evidence.mjs','docs/RELEASE_GATE.md','scripts/release-gate/collect-evidence.sh']:
  need(run(['git','-C',str(repo),'show',C+':'+rel])==(repo/rel).read_bytes(),'candidate contract source changed: '+rel)
 run(['git','-C',str(repo),'merge-base','--is-ancestor',C,'HEAD'])
 diff=run(['git','-C',str(repo),'diff','--name-only',C,'HEAD']).decode().splitlines()
 need(all(p.startswith('release-evidence/') or p in ('.axocoatl/gate-keys','.axocoatl/gate-override-actors') for p in diff),'non-evidence source changed after candidate')
 verifier="""import{readFileSync}from'node:fs';import{openEnvelope,parseKeys,verifyBundle}from MOD;
 const[d,k]=process.argv.slice(1);const keys=parseKeys(readFileSync(k,'utf8'));const x=verifyBundle(d,{keys,version:'1.5.0',commit:COMMIT});if(x.problems.length)throw Error(JSON.stringify(x.problems));console.log(JSON.stringify({signature_valid:true,signer:x.key.shortId,paths:Object.fromEntries([...x.paths].map(([k,v])=>[k,v.pass]))}));""".replace('MOD',json.dumps(module.as_uri())).replace('COMMIT',json.dumps(C))
 verified=json.loads(run([NODE,'--input-type=module','-e',verifier,str(src),str(keys)]))
 additions=plan.get('completions',[]);need(isinstance(additions,list),'completions must be a list')
 names=[];snapshots=[]
 for item in additions:
  name=item['path'];need(name in ALLOWED and name not in names,'unsupported/duplicate replacement path');names.append(name)
  root=pathlib.Path(item['source_bundle']).resolve();files=inventory(root);nm=validate_inventory(files,item['manifest_sha256'])
  need('manifest.dsse.json' not in files,'completion input must be unsigned')
  need(set(nm.get('paths',{}))=={name},'completion must contain exactly its declared path')
  need(nm['paths'][name].get('file')==name+'.json' and nm['paths'][name].get('pass') is True,'completion manifest must declare that exact passing record')
  need(all(p in ('manifest.json',name+'.json') or p.startswith(name+'/') for p in files),'completion crosses another path namespace')
  need(sha(files[name+'.json'])==item['record_sha256'],'completion record pin mismatch')
  review_path=pathlib.Path(item['review_file']).resolve();review_bytes=review_path.read_bytes();need(sha(review_bytes)==item['review_sha256'],'review pin mismatch');review=json.loads(review_bytes)
  need(review.get('schema')=='axocoatl.recovery.completion-review/1' and review.get('candidate_commit')==C and review.get('path')==name and review.get('record_sha256')==item['record_sha256'] and review.get('manifest_sha256')==item['manifest_sha256'],'review identity mismatch')
  need(review.get('decision')=='approved_for_evidence_integration' and review.get('actual_observations_reviewed') is True and review.get('unchanged_pass_criteria') is True and review.get('failed_attempts_preserved') is True,'completion not explicitly reviewed')
  need(isinstance(review.get('reviewer'),str) and review['reviewer'].strip() and isinstance(review.get('assessment'),str) and review['assessment'].strip(),'missing reviewer/assessment')
  validate="""import{readFileSync,existsSync}from'node:fs';import{checkPathEvidence,parseRequiredSteps,requiredStepProblems}from MOD;
 const[d,n,docs]=process.argv.slice(1);const e=JSON.parse(readFileSync(d+'/'+n+'.json'));const required=parseRequiredSteps(readFileSync(docs,'utf8')).get(n);if(!required?.length)throw Error('missing candidate requirements');const p=[...checkPathEvidence(n,e,{version:'1.5.0',commit:COMMIT}),...requiredStepProblems(n,e.steps,required)];if(p.length)throw Error(JSON.stringify(p));if(!e.attachments?.length)throw Error('missing attachments');console.log(JSON.stringify({required_steps:required.length,steps:e.steps.length}));""".replace('MOD',json.dumps(module.as_uri())).replace('COMMIT',json.dumps(C))
  result=json.loads(run([NODE,'--input-type=module','-e',validate,str(root),name,str(docs)]))
  record=json.loads(files[name+'.json'])
  for attached in record['attachments']:need(safe(attached) in files and attached.startswith(name+'/'),'attachment missing or outside path')
  snapshots.append((item,root,files,review_bytes,result))
 summary={'candidate_commit':C,'source_signature_valid':True,'source_files':len(old),'source_manifest_sha256':plan['source_manifest_sha256'],'checked_completions':names,'current_paths':verified['paths'],'inputs_unchanged':True,'scope':'Structural and cryptographic integration only; separately reviewed actual observations remain authoritative.'}
 if args.check_only:print(json.dumps(summary,indent=2));return
 need(names,'no reviewed completed path supplied; refusing to manufacture completion');need(args.out,'--out required')
 out=pathlib.Path(args.out).resolve();need(not out.exists(),'destination exists; no overwrite')
 need(out!=src and src not in out.parents and all(out!=r and r not in out.parents for _,r,_,_,_ in snapshots),'destination overlaps input')
 anchor=names[0]+'/retained-signed-bundles/'+plan['source_manifest_sha256'][:16]+'/'
 output=dict(old);overrides={}
 def preserve(rel):
  if rel not in old:return
  mapped=anchor+'files/'+rel;need(mapped not in output,'retention path collision');output[mapped]=old[rel];overrides[rel]=mapped;output.pop(rel,None)
 preserve('manifest.json');preserve('manifest.dsse.json')
 for item,root,files,review_bytes,result in snapshots:
  name=item['path']
  for rel,data in files.items():
   if rel=='manifest.json':continue
   if rel in old and old[rel]!=data:preserve(rel)
   need(rel not in output or output[rel]==data,'cross-completion collision');output[rel]=data
  detail=anchor+'completion-inputs/'+name+'/'
  output[detail+'manifest.json']=files['manifest.json'];output[detail+'review.json']=review_bytes
  output[detail+'validation.json']=encode(result)
 mapping={'schema':'axocoatl.recovery.retained-bundle-map/2','candidate_commit':C,'version':V,'source_manifest_sha256':plan['source_manifest_sha256'],'source_dsse_sha256':plan['source_dsse_sha256'],'source_file_count':len(old),'overrides':overrides,'default':'same relative path','reconstruction':'To verify older nested maps, first reconstruct this complete prior bundle through this map; do not reinterpret prior maps against changed top-level files.'}
 output[anchor+'retention-map.json']=encode(mapping);output[anchor+'staging-plan.json']=plan_path.read_bytes();output[anchor+'stage-reviewed-completions.py']=pathlib.Path(__file__).read_bytes();output[anchor+'gate-keys.public']=keys.read_bytes();output[anchor+'source-verification.json']=encode(verified)
 output[anchor+'candidate-contract/evidence.mjs']=module.read_bytes();output[anchor+'candidate-contract/RELEASE_GATE.md']=docs.read_bytes()
 output[anchor+'verify-retained-completion-source.mjs']=pathlib.Path('/private/tmp/axo-verify-retained-completion-source.mjs').read_bytes()
 output[anchor+'integration-provenance.json']=encode({**summary,'created_at':datetime.now(timezone.utc).isoformat(),'source_bundle':str(src),'replacement_record_bytes':'Copied exactly; no observed step/pass/exit status/attachment reference changed','unsigned':True,'no_auth_or_inference':True,'no_source_or_input_mutation':True,'active_completions':names,'original_failures':'All original signed files preserved byte-for-byte via retention-map.json'})
 for rel,data in old.items():need(output.get(overrides.get(rel,rel))==data,'original preservation failed: '+rel)
 # Same credential patterns as the existing collector; emit paths/counts only.
 pattern=''
 for line in (repo/'scripts/release-gate/collect-evidence.sh').read_text().splitlines():
  if line.startswith("secret_text='"):pattern=line[len("secret_text='"):-1]
  elif line.startswith('secret_text="$secret_text'):pattern+=line[len('secret_text="$secret_text'):-1]
 need(pattern,'collector scanner unavailable')
 suspicious=[]
 for rel,data in output.items():
  text=data.decode(errors='replace')
  # Claude's documented authorization request uses code=true as a boolean
  # flow selector. This exact vendor URL is not a returned OAuth credential.
  # Keep the original evidence bytes; exempt only this literal from scanning.
  callback_scan=text.replace('https://claude.com/cai/oauth/authorize?code=true&','https://claude.com/cai/oauth/authorize?flow_flag=true&')
  if re.search(pattern,text) or re.search(r'[?&](?:code|authorization_code|access_token|refresh_token)=[^\s&"<>]+',callback_scan):suspicious.append(rel)
  if any(x in rel.split('/') for x in ('secrets','.git','node_modules')) or pathlib.PurePosixPath(rel).name.startswith(('receipt-signing-key','local-api-token','id_ed25519','id_rsa')) or rel.endswith('.pem'):suspicious.append(rel)
 need(not suspicious,'sensitive/private payload refused; matched path count='+str(len(set(suspicious))))
 output[anchor+'privacy-scan.json']=encode({'collector_patterns':'clear','callback_credential_query_scan':'clear','private_names':'clear','matched_files':0,'raw_values_emitted':False})
 out.mkdir(parents=True,mode=0o700)
 for rel,data in output.items():
  p=out/rel;p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(data)
 build="import{writeFileSync}from'node:fs';import{buildManifest}from MOD;writeFileSync(process.argv[1]+'/manifest.json',JSON.stringify(buildManifest(process.argv[1],{version:'1.5.0',commit:COMMIT}),null,2)+'\\n');".replace('MOD',json.dumps(module.as_uri())).replace('COMMIT',json.dumps(C))
 run([NODE,'--input-type=module','-e',build,str(out)])
 need(inventory(src)==old,'source bundle changed during staging')
 for _,root,files,_,_ in snapshots:need(inventory(root)==files,'completion input changed during staging')
 for rel,data in old.items():need((out/overrides.get(rel,rel)).read_bytes()==data,'retention verification failed')
 need(not (out/'manifest.dsse.json').exists(),'unexpected successor signature')
 mapped_verification=json.loads(run([NODE,str(out/(anchor+'verify-retained-completion-source.mjs')),str(out),anchor+'retention-map.json',str(repo)]));need(mapped_verification['original_files_verified']==len(old),'mapped signature verification incomplete')
 print(json.dumps({**summary,'stage':str(out),'unsigned':True,'manifest_sha256':sha((out/'manifest.json').read_bytes()),'retention_map':anchor+'retention-map.json','original_files_preserved':len(old),'relocated_files':len(overrides),'manifest_files':len(json.loads((out/'manifest.json').read_text())['files'])},indent=2))
if __name__=='__main__':
 try:main()
 except Exception as e:print('REFUSED: '+str(e),file=sys.stderr);sys.exit(1)
