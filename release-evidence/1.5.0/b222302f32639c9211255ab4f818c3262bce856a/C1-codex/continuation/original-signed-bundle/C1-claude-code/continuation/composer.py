#!/usr/bin/env python3
"""One bounded, offline b222 Claude continuation; never invokes a provider or signs."""
import argparse, copy, hashlib, json, pathlib, shutil, subprocess, sys
from datetime import datetime, timezone
C='b222302f32639c9211255ab4f818c3262bce856a'
BINARY='d25cd64f70da4d17eb781d223aeab553f4180d074dbb29af302b23492ce31d1b'
ARCHIVE='2a9fc208948c630efab48d2f58875eb0c8f6dd24baec005479f691bdc532d659'
NODE='/Users/erick/.local/share/fnm/node-versions/v22.23.2/installation/bin/node'
OLD='C1-claude-code.json'
NEWROOT='C1-claude-code/supplemental/one-hour-node-recovery/'
NEW=NEWROOT+'1.5.0/'+C+'/supplemental-claude-node.json'
RAW=NEWROOT+'1.5.0/'+C+'/supplemental-claude-node/'
ACCEPT=NEWROOT+'acceptance/node-deps/'
OLDREC='C1-claude-code/hero-node-deps/run.axorecord.jsonl'
NEWREC=ACCEPT+'run.axorecord.jsonl'
PREFIX='C1-claude-code/continuation/original-signed-bundle/'
SUMMARY="the home page's fix-claude-code command passed on node-deps and python-pytest"
EPHEMERAL=(
 'manifest.repo','manifest.repo_head','manifest.session_id','manifest.workspace_id',
 'manifest.run_id','manifest.started_at_ms','manifest.options.request_id',
 'manifest.options.environment.cache_volume','manifest.options.environment.setup.duration_ms',
 'manifest.options.environment.probe.duration_ms','session.created_at','session.last_active',
 'session.id','session.workspace_id','session.working_dir','session.environment.runtime.id',
 'session.environment.prepared_at','session.loadout.run_id','team.slots.0.expires_at_ms')
SOURCES=('scripts/release-gate/evidence.mjs','scripts/release-gate/newuser-mac.sh',
 'scripts/release-gate/lib.sh','docs/RELEASE_GATE.md','tests/fixtures/newuser/fixtures.json',
 'crates/axocoatl-config/loadouts/fix-claude-code.yaml')
class Refusal(ValueError): pass
def need(ok,why):
 if not ok: raise Refusal(why)
def sha(data): return hashlib.sha256(data).hexdigest()
def js(data): return json.loads(data)
def encoded(data): return (json.dumps(data,indent=2,ensure_ascii=False)+'\n').encode()
def now(): return datetime.now(timezone.utc).isoformat()
def command(args):
 p=subprocess.run(args,capture_output=True,check=False)
 need(p.returncode==0,'command failed: '+str(args[:3])+': '+p.stderr.decode()[:500])
 return p.stdout

def signed_sources(bundle, module, keys, lock):
 manifest=(bundle/'manifest.json').read_bytes(); envelope=(bundle/'manifest.dsse.json').read_bytes()
 need(sha(manifest)==lock['manifest_sha256'],'original signed manifest changed')
 need(sha(envelope)==lock['dsse_sha256'],'original signature envelope changed')
 # The repository verifier independently verifies Ed25519 DSSE, every file and the inventory.
 code="""import {verifyBundle,parseKeys} from processModule; import {readFileSync} from 'node:fs';
 const v=verifyBundle(process.argv[1],{keys:parseKeys(readFileSync(process.argv[2],'utf8')),version:'1.5.0',commit:process.argv[3]});
 console.log(JSON.stringify({problems:v.problems,key:v.key?.shortId,paths:[...v.paths].map(([name,p])=>({name,pass:p.pass,problems:p.problems}))}));
 if(v.problems.length)process.exitCode=1;""".replace('processModule',json.dumps(module.as_uri()))
 verified=js(command([NODE,'--input-type=module','-e',code,str(bundle),str(keys),C]))
 files={x['path']:(bundle/x['path']).read_bytes() for x in js(manifest)['files']}
 files.update({'manifest.json':manifest,'manifest.dsse.json':envelope})
 verify_hashes(files,js(manifest)['files'])
 return files,verified

def verify_hashes(files,entries):
 for x in entries:
  need(x['path'] in files,'missing referenced source: '+x['path'])
  need(sha(files[x['path']])==x['sha256'],'altered source hash: '+x['path'])
  need(len(files[x['path']])==x['bytes'],'altered source length: '+x['path'])

def record(data):
 rows=[js(x) for x in data.splitlines()]
 out={}
 for row in rows:
  if row.get('section') in ('manifest','loadout','session','team','outcome'):
   need(row['section'] not in out,'duplicate record section')
   out[row['section']]=row['data']
 need(set(out)=={'manifest','loadout','session','team','outcome'},'missing record configuration or outcome')
 return out

def at(obj, dotted):
 for k in dotted.split('.'):
  obj=obj[int(k)] if isinstance(obj,list) else obj[k]
 return obj

def effective(rec):
 out=copy.deepcopy({k:rec[k] for k in ('manifest','loadout','session','team')})
 for dotted in EPHEMERAL:
  parts=dotted.split('.'); parent=at(out,'.'.join(parts[:-1])); key=parts[-1]
  need(key in parent if isinstance(parent,dict) else int(key)<len(parent),'missing known per-attempt field: '+dotted)
  if isinstance(parent,list):parent[int(key)]='<per-attempt>'
  else:parent[key]='<per-attempt>'
 return out

def passing(step,label):
 need(step.get('status')=='pass',label+' did not pass')
 need(step.get('required') is not False,label+' was made optional')

def check_assessment(files,base):
 a=js(files[base+'assessment.json'])
 need(a['source_commit']==C,'acceptance candidate mismatch')
 need(a['result']=='ACCEPTED' and a['original_tests_unchanged'],'artifact not accepted with original tests')
 for key in ('receipt_exit','record_exit','markup_exit'):
  need(a['verification'].get(key)==0,'missing dependent receipt/record/markup check: '+key)
 need(a['host_test']['exit_code']==0,'original host acceptance failed')
 for rel,h in a['evidence_hashes'].items():
  need(base+rel in files and sha(files[base+rel])==h,'acceptance dependent hash mismatch: '+rel)
 return a

def validate(files,candidate_sources,lock):
 verify_hashes(files,js(files['manifest.json'])['files'])
 for rel,h in lock['candidate_source_sha256'].items():
  need(rel in candidate_sources and sha(candidate_sources[rel])==h,'altered candidate source hash: '+rel)
 old,new=js(files[OLD]),js(files[NEW])
 for d in (old,new):need(d['commit']==C and d['version']=='1.5.0','candidate mismatch')
 need(old['path']=='C1-claude-code' and new['path']=='supplemental-claude-node','wrong path identity')
 need(len(old['steps'])==37 and len(new['steps'])==18,'step inventory changed')
 need(old['pass'] is False and [i for i,s in enumerate(old['steps']) if s['status']!='pass']==[11,25], 'original failure history changed')
 need(new['pass'] is True,'continuation did not pass')
 for i,s in enumerate(new['steps']):passing(s,'continuation step '+str(i))
 for i,s in enumerate(old['steps']):
  if i not in (11,25) and s.get('required') is not False:passing(s,'original step '+str(i))
 fresh=['no Claude Code sign-in was stored before this path',
 'onboard --update --writer claude-code (interactive; every answer is in the transcript)',
 'onboarding was answered live in the terminal','Claude Code was signed in through the browser during onboarding']
 need([s['name'] for s in old['steps'][2:6]]==fresh,'missing fresh onboarding prerequisite')
 need(old['steps'][3].get('exit_code')==0,'fresh onboarding did not complete')
 operator=js(files['C1-claude-code/operator.json'])
 need(operator['source_head']==C and operator['claude_authentication']['operator_observed'] is True,'missing observed browser authentication')
 need(operator['human_operator'] is False,'operator attribution changed')
 for i,j in zip(range(11,18),range(9,16)):
  need(old['steps'][i]['name']==new['steps'][j]['name'].replace('claude-node-deps','hero-node-deps'),'whole dependent block identity mismatch')
 for i in (9,12,13,14):need(new['steps'][i].get('exit_code')==0,'missing dependent receipt/run check')
 need(old['steps'][25]['name']==SUMMARY,'summary criterion changed')
 a=record(files[OLDREC]);b=record(files[NEWREC])
 for rec,run in ((a,'run-8ff54b58-1904-4ddf-9193-3e983357cc38'),(b,'run-776fe067-8014-45a4-9391-8b7a918b281d')):
  m=rec['manifest'];s=rec['session']
  need(m['run_id']==run and s['loadout']['run_id']==run and rec['outcome']['run_id']==run,'attempt identity mismatch')
  need(m['session_id']==s['id']==s['environment']['runtime']['id'],'session identity mismatch')
  need(m['workspace_id']==s['workspace_id'] and m['repo']==s['working_dir'],'workspace identity mismatch')
  need(3600000<=rec['team']['slots'][0]['expires_at_ms']-m['started_at_ms']<=3610000,'writer time allowance changed')
  text=candidate_sources['crates/axocoatl-config/loadouts/fix-claude-code.yaml'].decode()
  need(rec['loadout']['text']==text and m['loadout_text']==text,'loadout source mismatch')
  need(rec['loadout']['digest']==sha(text.encode())==m['loadout']['digest'],'loadout digest mismatch')
 need(effective(a)==effective(b),'effective execution configuration mismatch')
 need(b['outcome']['verdict']=='pass' and b['outcome']['exit_code']==0,'continued outcome did not pass')
 need(a['outcome']['verdict']=='interrupted','original interrupted outcome changed')
 old_a=js(files['C1-claude-code/acceptance/hero-node-deps/assessment.json'])
 new_a=check_assessment(files,ACCEPT)
 for label in ('hero-python-pytest','keep-pr'):check_assessment(files,'C1-claude-code/acceptance/'+label+'/')
 need(old_a['result']=='NOT_ACCEPTED_INTERRUPTED','original negative acceptance changed')
 need(new_a['fixture']=='node-deps' and new_a['run_id']==b['manifest']['run_id'],'fixture or accepted run mismatch')
 need(new_a['original_tests_unchanged'] and new_a['all_other_tracked_files_unchanged'],'fixture acceptance changed')
 need(new_a['host_test']['original_suite_invocations']==1,'unexpected acceptance invocation count')
 need(new_a['file_hashes'].keys()==old_a['file_hashes'].keys(),'fixture file inventory changed')
 for rel,h in new_a['file_hashes'].items():
  source='tests/fixtures/newuser/node-deps/'+rel
  need(sha(candidate_sources[source])==h['original_sha256']==old_a['file_hashes'][rel]['original_sha256'],'fixture original mismatch: '+rel)
  for which in ('original','final'):
   need(sha(files[ACCEPT+which+'/'+rel])==h[which+'_sha256'],'fixture captured bytes mismatch: '+rel)
 config=js(files[RAW+'configuration.json']);prov=js(files[RAW+'provenance.json'])
 need(config['loadout']=='fix-claude-code' and config['reviewer']=='ollama:gpt-oss:20b' and config['credential']=='claude-code-oauth','continuation configuration mismatch')
 need(config['configuration_mutated'] is False and config['onboarding'] is False,'continuation changed owner configuration or repeated onboarding')
 need(new['steps'][8]['detail']==config['configuration_snapshot_sha256'],'continuation config checkpoint mismatch')
 need(prov['candidate_commit']==C and prov['binary_sha256']==BINARY and prov['archive_sha256']==ARCHIVE,'candidate binary or archive mismatch')
 build=js(files['C1-local/final-verification/mac-provenance.json'])
 need(all(build[k]==C for k in ('candidate_commit','packaging_source_commit','binary_build_commit')),'original candidate build mismatch')
 need(build['binary_sha256']==BINARY and build['archive_sha256']==ARCHIVE,'original binary mismatch')
 return old,new,a,b,new_a

def reference(files,rel,index=None):
 out={'file':PREFIX+rel,'sha256':sha(files[rel])}
 if index is not None:
  s=js(files[rel])['steps'][index];out.update(step_index=index,step_name=s['name'],observed_status=s['status'])
 return out

def compose(files,candidate_sources,lock,signature):
 old,new,a,b,accepted=validate(files,candidate_sources,lock)
 result=copy.deepcopy(old);result['pass']=True;result['finished_at']=new['finished_at']
 mappings=[]
 for i in range(37):
  if 11<=i<=17:
   j=i-2;s=copy.deepcopy(new['steps'][j]);s['name']=old['steps'][i]['name']
   ref=reference(files,NEW,j);s['observation']=ref;result['steps'][i]=s
  elif i==25:
   deps=[reference(files,NEW,j) for j in range(9,16)]+[reference(files,OLD,j) for j in range(18,25)]
   result['steps'][i]={'name':SUMMARY,'status':'pass','required':True,
    'detail':'Operator-authorized continuation: the original Python block and later complete Node block satisfy the unchanged criterion. The original failed attempt remains preserved.',
    'observation_kind':'derived-from-verified-dependent-observations','dependencies':deps,
    'acceptance':[reference(files,ACCEPT+'assessment.json'),reference(files,'C1-claude-code/acceptance/hero-python-pytest/assessment.json')]}
   ref={'dependencies':deps}
  else:
   ref=reference(files,OLD,i);result['steps'][i]['observation']=ref
  mappings.append({'successor_step':i,'name':result['steps'][i]['name'],'source':ref})
 result['continuation']={
  'schema':'axocoatl.recovery.operator-continuation/1','prepared_at':now(),'operator':'Codex assistant',
  'authority':'Maintainer authorized completion from the previously observed fresh prerequisites and the later missing Node block; unchanged pass criteria.',
  'unsigned':True,'original_attempt':reference(files,OLD),'original_attempt_pass':False,
  'original_failed_steps':[11,25],'replaced_dependent_steps':list(range(11,18)),'rederived_summary_step':25,
  'fresh_onboarding_repeated':False,'new_inference':False,'tracked_source_changed':False,
  'observation_intervals':[[old['started_at'],old['finished_at']],[new['started_at'],new['finished_at']]],
  'global_configuration_identity':'Historical global YAML hash was not recorded and is not claimed known. Equality is established for the recorded effective execution configuration, built-in loadout, fixture originals, image, setup, checks, review and budgets.',
  'operator_attribution':'Original path labels name the OS account erick; the signed operator record attributes the actions to Codex using a tool-owned PTY and CUA, with browser authentication observed. This successor makes no human-only operator claim.',
  'excluded_per_attempt_fields':list(EPHEMERAL),'effective_configuration_sha256':sha(encoded(effective(a))),
  'supplemental_owner_configuration_sha256':js(files[RAW+'configuration.json'])['configuration_snapshot_sha256'],
  'source_signature':signature,'remaining_release_conditions':'This is Claude path completion only; it is unsigned until maintainer review and does not establish other paths, main CI or publication readiness.'}
 result['attachments']=[PREFIX+x for x in sorted(files)]
 return result,mappings,effective(a),accepted

def main():
 p=argparse.ArgumentParser();p.add_argument('--repo',required=True);p.add_argument('--lock',required=True);p.add_argument('--out',required=True);p.add_argument('--tests-log',required=True);args=p.parse_args()
 repo=pathlib.Path(args.repo).resolve();lockpath=pathlib.Path(args.lock);lock=js(lockpath.read_bytes());out=pathlib.Path(args.out)
 need(not out.exists(),'output already exists; never replace an attempt')
 bundle=repo/'release-evidence/1.5.0'/C
 sources={rel:command(['git','-C',str(repo),'show',C+':'+rel]) for rel in lock['candidate_source_sha256']}
 module=repo/'scripts/release-gate/evidence.mjs'
 need(sha(module.read_bytes())==lock['candidate_source_sha256']['scripts/release-gate/evidence.mjs'],'current verifier differs from candidate')
 files,verified=signed_sources(bundle,module,repo/'.axocoatl/gate-keys',lock)
 result,mapping,config,accepted=compose(files,sources,lock,verified)
 out.mkdir(parents=True)
 def put(rel,data):
  dest=out/rel;dest.parent.mkdir(parents=True,exist_ok=True);dest.write_bytes(data)
 for rel,data in files.items():put(PREFIX+rel,data)
 sup='C1-claude-code/continuation/'
 for rel,data in sources.items():put(sup+'candidate-sources/'+rel,data)
 put(sup+'gate-keys.public',(repo/'.axocoatl/gate-keys').read_bytes())
 put(sup+'source-lock.json',lockpath.read_bytes())
 put(sup+'composer.py',pathlib.Path(__file__).read_bytes())
 put(sup+'composer-tests.py',pathlib.Path('/private/tmp/axo-test-claude-completion.py').read_bytes())
 put(sup+'composer-tests.log',pathlib.Path(args.tests_log).read_bytes())
 put(sup+'source-signature-verification.json',encoded(verified))
 put(sup+'step-provenance.json',encoded(mapping))
 put(sup+'effective-execution-configuration.json',encoded(config))
 assessment={'schema':'axocoatl.recovery.claude-completion/1','prepared_at':now(),'candidate':C,
  'binary_sha256':BINARY,'archive_sha256':ARCHIVE,'unsigned':True,'path_pass':True,
  'original_path_sha256':sha(files[OLD]),'original_pass':False,'original_steps':37,'original_failed_steps':[11,25],
  'successor_steps':37,'successor_passing_steps':37,'original_signed_files_preserved':len(files),
  'original_manifest_sha256':sha(files['manifest.json']),'original_dsse_sha256':sha(files['manifest.dsse.json']),
  'node_artifact':accepted['artifact_review'],'node_original_tests_passed':4,
  'node_negative_evidence':accepted['negative_evidence'],'node_usage':accepted['usage'],
  'new_inference':False,'new_authentication':False,'global_yaml_historical_hash_known':False,
  'scope':'New operator-authorized continuation record; immutable original failure remains nested with its complete authenticated bundle. Other release paths remain outside this result.'}
 put(sup+'assessment.json',encoded(assessment))
 result['attachments']=sorted(str(x.relative_to(out)) for x in out.rglob('*') if x.is_file())
 put('C1-claude-code.json',encoded(result))
 # Apply the unmodified candidate's public path/required-step validator.
 code="""import {checkPathEvidence,parseRequiredSteps,requiredStepProblems,buildManifest} from processModule; import{readFileSync,writeFileSync}from'node:fs';
 const d=process.argv[1],c=process.argv[2],e=JSON.parse(readFileSync(d+'/C1-claude-code.json')); const required=parseRequiredSteps(readFileSync(d+'/C1-claude-code/continuation/candidate-sources/docs/RELEASE_GATE.md','utf8'));
 const problems=[...checkPathEvidence('C1-claude-code',e,{version:'1.5.0',commit:c}),...requiredStepProblems('C1-claude-code',e.steps,required.get('C1-claude-code'))];
 console.log(JSON.stringify({problems,required_steps:required.get('C1-claude-code').length}));if(problems.length)process.exitCode=1;else writeFileSync(d+'/manifest.json',JSON.stringify(buildManifest(d,{version:'1.5.0',commit:c}),null,2)+'\\n');""".replace('processModule',json.dumps(module.as_uri()))
 validated=js(command([NODE,'--input-type=module','-e',code,str(out),C]))
 # Recheck immutable source and copied bytes after writing. No original file is ever opened for writing.
 for rel,data in files.items():
  need((bundle/rel).read_bytes()==data,'source changed during composition: '+rel)
  need((out/PREFIX/rel).read_bytes()==data,'preservation failed: '+rel)
 need(not (out/'manifest.dsse.json').exists(),'successor unexpectedly signed')
 print(json.dumps({'output':str(out),'candidate':C,'unsigned':True,'original_failure_preserved':True,'original_files':len(files),'successor_steps':37,'required_steps_validated':validated['required_steps'],'successor_sha256':sha((out/'C1-claude-code.json').read_bytes()),'manifest_sha256':sha((out/'manifest.json').read_bytes())},indent=2))
if __name__=='__main__':
 try:main()
 except (Refusal,KeyError,ValueError) as e:print('REFUSED: '+str(e),file=sys.stderr);sys.exit(1)
