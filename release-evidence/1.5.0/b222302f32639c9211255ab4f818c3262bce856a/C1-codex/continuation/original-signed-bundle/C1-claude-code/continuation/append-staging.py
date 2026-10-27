#!/usr/bin/env python3
"""Build an unsigned review bundle; never modifies a source bundle or signs."""
import argparse,hashlib,json,pathlib,subprocess,sys
from datetime import datetime,timezone
C='b222302f32639c9211255ab4f818c3262bce856a'
OLD_MANIFEST='aaa872c3d26b7ecb02916e98cf01de65dd6f15a75125827458aa818bbe4638e7'
OLD_DSSE='8ec1d618a03ce2377b8d079b663dcda9849902190ceba683c190b4dd718481e1'
COMPOSER_MANIFEST='88f09d94352ceadc5a5c64980a3e9246ef32ed4330d8f25acd558afd587e3c20'
COMPOSER_RESULT='43ec0c3008b9f6b321143113261c90d2091da1b32a080c39b44c3c88692e7271'
NODE='/Users/erick/.local/share/fnm/node-versions/v22.23.2/installation/bin/node'
BASE='C1-claude-code/continuation/'
NESTED=BASE+'original-signed-bundle/'
OVERRIDES={p:BASE+'original-attempt/'+p for p in ['manifest.json','manifest.dsse.json','C1-claude-code.json']}
def need(ok,msg):
 if not ok:raise ValueError(msg)
def sha(b):return hashlib.sha256(b).hexdigest()
def encode(x):return (json.dumps(x,indent=2,ensure_ascii=False)+'\n').encode()
def run(args):
 p=subprocess.run(args,capture_output=True);need(p.returncode==0,p.stderr.decode() or p.stdout.decode());return p.stdout
def inventory(root):
 out={}
 for p in root.rglob('*'):
  need(not p.is_symlink(),'symbolic link refused: '+str(p))
  if p.is_file():out[str(p.relative_to(root))]=p.read_bytes()
 return out
def verify_inventory(files,pin):
 need(sha(files['manifest.json'])==pin,'source manifest differs from reviewed input')
 m=json.loads(files['manifest.json']);need(m['commit']==C,'candidate differs')
 allowed={'manifest.json','manifest.dsse.json'}
 for e in m['files']:
  p=e['path'];need(p in files and sha(files[p])==e['sha256'] and len(files[p])==e['bytes'],'source hash/length mismatch: '+p);allowed.add(p)
 need(not set(files)-allowed,'unlisted source files')
 return m

def main():
 ap=argparse.ArgumentParser();ap.add_argument('--repo',required=True);ap.add_argument('--completion',required=True);ap.add_argument('--out',required=True);a=ap.parse_args()
 repo=pathlib.Path(a.repo).resolve();source=repo/'release-evidence/1.5.0'/C;completion=pathlib.Path(a.completion).resolve();out=pathlib.Path(a.out).resolve()
 need(not out.exists(),'destination exists; no overwrite permitted')
 old=inventory(source);new=inventory(completion);verify_inventory(old,OLD_MANIFEST);verify_inventory(new,COMPOSER_MANIFEST)
 need(sha(old['manifest.dsse.json'])==OLD_DSSE,'source signature differs')
 need(sha(new['C1-claude-code.json'])==COMPOSER_RESULT,'completion differs from reviewed record')
 for rel,data in old.items():need(new.get(NESTED+rel)==data,'completion did not retain current original: '+rel)
 # Keep each old file once. Only the three replaced namespace members move.
 output={OVERRIDES.get(p,p):data for p,data in old.items()}
 for rel,data in new.items():
  if rel in ('manifest.json','C1-claude-code.json') or rel.startswith(NESTED):continue
  need(rel not in output or output[rel]==data,'append collision: '+rel);output[rel]=data
 output[BASE+'composer-output.json']=new['C1-claude-code.json']
 output[BASE+'composer-input-manifest.json']=new['manifest.json']
 def map_path(p):return OVERRIDES.get(p,p)
 def remap(value):
  if isinstance(value,str) and value.startswith(NESTED):return map_path(value[len(NESTED):])
  if isinstance(value,list):return [remap(v)for v in value]
  if isinstance(value,dict):return {k:remap(v)for k,v in value.items()}
  return value
 record=remap(json.loads(new['C1-claude-code.json']))
 mapping={'schema':'axocoatl.recovery.retained-bundle-map/1','commit':C,'version':'1.5.0','default_location':'unchanged relative path in this bundle','overrides':OVERRIDES,'manifest_sha256':OLD_MANIFEST,'dsse_sha256':OLD_DSSE,'original_file_count':len(old),'standalone_composer_reference_prefix':NESTED,'instruction':'Remove the standalone prefix, then apply overrides; all other original files remain at the same relative path. Verify with verify-retained-bundle.mjs, not verifyBundle on original-attempt alone.'}
 output[BASE+'retention-map.json']=encode(mapping)
 output[BASE+'append-staging.py']=pathlib.Path(__file__).read_bytes()
 output[BASE+'verify-retained-bundle.mjs']=pathlib.Path('/private/tmp/axo-verify-retained-bundle.mjs').read_bytes()
 output[BASE+'step-provenance.json']=encode(remap(json.loads(new[BASE+'step-provenance.json'])))
 status={'schema':'axocoatl.recovery.resumed-four-gates-status/1','as_of':datetime.now(timezone.utc).isoformat(),'candidate_commit':C,'source':'Root task coordination at append preparation; pending states are not passing observations.','claude':{'state':'verified_continuation','pass':True,'fresh_rerun':False,'original_failed_attempt_preserved':True,'record':'C1-claude-code.json','assessment':BASE+'assessment.json','independent_review':'release_inventory found no blocker'},'linux':{'state':'live_oauth','pass':None},'mac_openrouter':{'state':'onboarding_live','pass':None},'codex':{'state':'pending_fresh_gate','pass':None},'release_ready':False,'signed':False}
 output[BASE+'resumed-status.json']=encode(status)
 provenance={'schema':'axocoatl.recovery.claude-append/1','prepared_at':status['as_of'],'candidate_commit':C,'root_review':'Root accepted the Claude continuation and independent read-only review.','scope':'References are relocated only; no observed result or criterion is changed. Existing signed failure and all original bytes are retained.','source_bundle':str(source),'source_manifest_sha256':OLD_MANIFEST,'source_dsse_sha256':OLD_DSSE,'source_file_count':len(old),'composer_stage':str(completion),'composer_manifest_sha256':COMPOSER_MANIFEST,'composer_record_sha256':COMPOSER_RESULT,'original_claude_record_sha256':sha(old['C1-claude-code.json']),'active_record_reference_changes':'The original-signed-bundle prefix is removed, except the three archived original namespace members use retention-map overrides. Original composer output is preserved separately.','global_yaml_historical_hash_known':False,'no_new_inference':True,'unsigned':True,'tracked_source_mutation':False}
 output[BASE+'append-provenance.json']=encode(provenance)
 # Claude attachments include its full retained path, tools and the original archive.
 # References into other original paths remain explicit in continuation provenance.
 record['attachments']=sorted(p for p in output if p.startswith('C1-claude-code/'))
 output['C1-claude-code.json']=encode(record)
 out.mkdir(parents=True)
 def put(p,b):dest=out/p;dest.parent.mkdir(parents=True,exist_ok=True);dest.write_bytes(b)
 for p,b in output.items():put(p,b)
 verified=json.loads(run([NODE,str(out/(BASE+'verify-retained-bundle.mjs')),str(out)]))
 put(BASE+'retained-source-verification.json',encode(verified))
 record['attachments'].append(BASE+'retained-source-verification.json');record['attachments'].sort();put('C1-claude-code.json',encode(record))
 module=out/(BASE+'candidate-sources/scripts/release-gate/evidence.mjs')
 code="""import{readFileSync,writeFileSync}from'node:fs';import{checkPathEvidence,parseRequiredSteps,requiredStepProblems,buildManifest}from MOD;
 const d=process.argv[1],c=process.argv[2],e=JSON.parse(readFileSync(d+'/C1-claude-code.json'));const req=parseRequiredSteps(readFileSync(d+'/C1-claude-code/continuation/candidate-sources/docs/RELEASE_GATE.md','utf8')).get('C1-claude-code');
 const problems=[...checkPathEvidence('C1-claude-code',e,{version:'1.5.0',commit:c}),...requiredStepProblems('C1-claude-code',e.steps,req)];if(problems.length)throw new Error(JSON.stringify(problems));writeFileSync(d+'/manifest.json',JSON.stringify(buildManifest(d,{version:'1.5.0',commit:c}),null,2)+'\\n');console.log(JSON.stringify({problems,required_steps:req.length}));""".replace('MOD',json.dumps(module.as_uri()))
 validated=json.loads(run([NODE,'--input-type=module','-e',code,str(out),C]))
 for rel,data in old.items():need((source/rel).read_bytes()==data and (out/map_path(rel)).read_bytes()==data,'original preservation failed: '+rel)
 need(inventory(completion)==new,'completion source changed')
 need(not (out/'manifest.dsse.json').exists(),'unexpected successor signature')
 final=json.loads((out/'manifest.json').read_text());need(all((out/p).is_file()for p in record['attachments']),'attachment remap missing target')
 print(json.dumps({'stage':str(out),'unsigned':True,'original_files_preserved_once':len(old),'files':len(final['files']),'claude_steps':37,'required_steps':validated['required_steps'],'source_signature_valid':verified['signature_valid'],'active_claude_sha256':sha((out/'C1-claude-code.json').read_bytes()),'manifest_sha256':sha((out/'manifest.json').read_bytes()),'status_file':str(out/(BASE+'resumed-status.json'))},indent=2))
if __name__=='__main__':
 try:main()
 except Exception as e:print('REFUSED: '+str(e),file=sys.stderr);sys.exit(1)
