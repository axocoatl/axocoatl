#!/usr/bin/env python3
"""Compose reviewed actual D1 observations; never run tests/auth or change inputs."""
import argparse,copy,datetime,hashlib,json,pathlib,re,subprocess,sys
C='b222302f32639c9211255ab4f818c3262bce856a';V='1.5.0';B='9e03af65d461cf23f8577bc3cee14667efc44db4726a8a3556dd3a5c6419c0b8';A='6637fb0057b114f0bc0f1b8bf5af687cfe59b9b6a50c55b211fd6655a1150cf2'
R=pathlib.Path('/Users/erick/.codex/worktrees/release-1-5-recovery/axocoatl');N='/Users/erick/.local/share/fnm/node-versions/v22.23.2/installation/bin/node'
FRESH=pathlib.Path('/private/tmp/axocoatl-d1-fresh-prefix-public-20261027');FAILED=pathlib.Path('/private/tmp/axocoatl-d1-b222-acceptance-input-20261027/unsigned-failed-attempt-support')
sha=lambda b:hashlib.sha256(b).hexdigest();enc=lambda x:(json.dumps(x,indent=2,ensure_ascii=False)+'\n').encode();load=lambda p:json.loads(p.read_bytes())
def need(ok,msg):
 if not ok:raise ValueError(msg)
def main():
 ap=argparse.ArgumentParser();ap.add_argument('--final-public',required=True);ap.add_argument('--retry-public',required=True);ap.add_argument('--service-acceptance',required=True);ap.add_argument('--service-sample-acceptance',required=True);ap.add_argument('--out',required=True);a=ap.parse_args()
 final=pathlib.Path(a.final_public).resolve();out=pathlib.Path(a.out).resolve();need(not out.exists(),'output exists')
 records=list(final.rglob('D1-continuation2.json'));need(len(records)==1,'need one finalized actual continuation record');rp=records[0];fin=load(rp)
 need(fin.get('commit')==C and fin.get('version')==V and fin.get('pass') is False,'immutable original continuation identity/status changed')
 need(sha(rp.read_bytes())=='5abf12710a74c5574720d8fd9a16b5535ee37fff750c04a973ea8e411adf5182','original final record changed')
 need([i for i,s in enumerate(fin['steps']) if s.get('status')=='fail']==[53] and len(fin['steps'])==72,'unexpected original failure set')
 need(fin['steps'][53]['name']=='onboard --update --writer-model openrouter:deepseek/deepseek-chat-v3.1 --smoke-test (the configuration changes while the service runs)','wrong affected phase')
 retryroot=pathlib.Path(a.retry_public).resolve();retryrecords=list(retryroot.rglob('D1-service-smoke-retry.json'));need(len(retryrecords)==1,'need one finalized actual service retry record');rr=retryrecords[0];retry=load(rr)
 need(retry.get('commit')==C and retry.get('version')==V and retry.get('pass') is True and retry.get('finished_at') and len(retry['steps'])==17 and all(s['status']=='pass' for s in retry['steps']),'targeted service retry has not completely passed')
 need(fin.get('finished_at') and all(s.get('status')=='pass' or (i==53 and s.get('status')=='fail') or (s.get('status')=='skip' and s.get('required') is False) for i,s in enumerate(fin['steps'])),'unexpected failure/skip')
 retryprov=load(retryroot/'execution-provenance.json');need(retryprov['candidate_commit']==C and retryprov['binary_sha256']==B and retryprov['archive_sha256']==A and retryprov['os_account']=='d1gate','retry identity mismatch')
 retryattach=rr.parent/'D1-service-smoke-retry';bp=(retryattach/'config-sha256-before.txt').read_bytes();need(bp==(retryattach/'config-sha256-after.txt').read_bytes() and re.fullmatch(rb'[0-9a-f]{64}\n?',bp),'retry changed configuration')
 prov=load(final/'execution-provenance.json');need(prov['candidate_commit']==C and prov['binary_sha256']==B and prov['archive_sha256']==A and prov['os_account']=='d1gate','final identity mismatch')
 oldbytes=(FRESH/'original-steps.jsonl').read_bytes();need(sha(oldbytes)=='dd08ed0d143e200c062cbaa1dd41cfbfe2be49a00411baab35ec768d11ec0c4f','original failed steps changed')
 original=[json.loads(l) for l in oldbytes.splitlines()];need(sum(s['status']=='fail' for s in original)==43,'original failure inventory changed')
 fp=load(FRESH/'provenance.json');need(fp['candidate_commit']==C and fp['original_gate_d1_json_exists'] is False,'fresh provenance mismatch')
 for e in fp['files']:need(sha((FRESH/e['path']).read_bytes())==e['sha256'],'fresh source hash mismatch')
 need(load(FAILED/'gate/D1-continuation.json')['pass'] is False,'prior reviewer failure missing')
 authlog=FAILED/'gate/D1-continuation/logs/onboarding/onboard-20261027-011403.log';auth=authlog.read_bytes();need(sha(auth)=='4fa4c835e8de110444de559e83c5d5050cbd92eba7a4c213bcb1d5ecb614b605','auth source changed')
 for text in ['Sign in to OpenRouter in your browser (recommended)','OpenRouter sign-in: GET /callback','Connected OpenRouter: stored the key as secret openrouter']:
  need(text in auth.decode(),'fresh auth observation missing')
 prior=load(FAILED/'gate/D1-continuation.json');need(prior['steps'][0]['name']=='no OpenRouter secret before controlled onboarding retry' and prior['steps'][0]['status']=='pass','fresh secret absence missing')
 files={};index=[];omitted=[];prefix='D1/continuation/'
 cache={'.git','node_modules','npm-cache','__pycache__','host-venv','cargo-target','go-build','target','.pytest_cache'}
 def capture(root,dest):
  for p in sorted(root.rglob('*')):
   rel=p.relative_to(root)
   if any(z in cache for z in rel.parts):continue
   need(not p.is_symlink(),'unexpected source symlink: '+str(p))
   if p.is_file():
    need(p.stat().st_size<64*1024*1024,'oversized evidence');d=p.read_bytes();k=dest+str(rel);need(k not in files,'duplicate evidence name');files[k]=d;index.append({'source':str(p),'destination':k,'sha256':sha(d),'bytes':len(d)})
 capture(FRESH,prefix+'fresh-prefix/');capture(FAILED,prefix+'failed-continuation/');capture(final,prefix+'completed-continuation/');capture(retryroot,prefix+'service-smoke-retry/');capture(pathlib.Path('/private/tmp/axocoatl-d1-retry2-service-sample-20261027/acceptance/sample'),prefix+'failed-service-smoke-acceptance/')
 for name in ['axocoatl-d1-b222-continue.sh','axocoatl-d1-b222-sample-retry.sh','axocoatl-d1-b222-sample-retry-plan.json','axocoatl-d1-b222-service-smoke-retry.sh','axocoatl-d1-service-retry-launch-correction.json','axocoatl-d1-incomplete-arguments-diagnosis.json','axo-or-diag-result.json']:
  p=pathlib.Path('/private/tmp')/name
  if p.exists():
   d=p.read_bytes();dest=prefix+'operator-support/'+name;files[dest]=d;index.append({'source':str(p),'destination':dest,'sha256':sha(d),'bytes':len(d)})
 audits=[]
 inputs=[('sample',pathlib.Path('/private/tmp/axocoatl-d1-retry2-sample-20261027/acceptance/sample'))]+[(f,pathlib.Path('/private/tmp/axocoatl-d1-retry2-'+f+'-20261027/acceptance/'+f)) for f in ['node-plain','node-deps','python-pytest','rust','go']]+[('service-sample',pathlib.Path(a.service_sample_acceptance)),('service-node-deps',pathlib.Path(a.service_acceptance))]
 for label,p in inputs:
  j=load(p/'assessment.json');need(j['source_commit']==C and j['execution_binary_sha256']==B and j['execution_archive_sha256']==A,'acceptance identity mismatch: '+label)
  need(j['host_test']['exit_code']==0 and j['host_test']['suite_count_verified'] is True and j['host_test']['original_suite_invocations']==1,'original suite acceptance missing: '+label)
  need(all(j['verification'][k]==0 for k in ['receipt_exit','record_exit','markup_exit']),'dependent receipt/record check missing: '+label)
  need(j['required_checks_passed'] and j['required_review_passed'],'required checks/review missing: '+label)
  for rel,h in j['evidence_hashes'].items():need(sha((p/rel).read_bytes())==h,'altered acceptance evidence: '+label)
  if j['result']!='ACCEPTED':
   correction=p/'formal-gate-qualification-correction.json'
   if correction.exists():
    corrected=load(correction);need(label=='node-deps' and corrected.get('artifact_contract_result')=='FAILED' and corrected['assessment_sha256']==sha((p/'assessment.json').read_bytes()),'unexpected artifact correction')
    need(corrected['criterion']=='No test changes. All 4 tests pass after the fix.' and corrected['superseded_qualification_sha256']==sha((p/'formal-gate-qualification.json').read_bytes()),'negative artifact or erroneous earlier qualification not preserved')
    # Root explicitly permits runtime criterion composition with this actual
    # task failure retained. This does not accept or waive the artifact rule.
   else:
    q=load(p/'formal-gate-qualification.json');need(label=='sample' and q['original_assessment_sha256']==sha((p/'assessment.json').read_bytes()) and q['original_assessment_result_retained']=='FAILED' and q.get('formal_sample_result')=='PASS','missing sample-only qualification')
  capture(p,prefix+'acceptance/'+label+'/');audits.append({'case':label,'assessment':prefix+'acceptance/'+label+'/assessment.json','sha256':sha((p/'assessment.json').read_bytes()),'strict_result':j['result'],'original_tests':j['host_test']['observed_passed_tests'],'run_id':j['run_id'],'artifact_contract_result':'FAILED: added test violates explicit No test changes' if label=='node-deps' else ('see retained sample quality assessment' if j['result']=='FAILED' else 'ACCEPTED')})
 def ref(rel,i=None,jsonl=False):
  z={'file':rel,'sha256':sha(files[rel])}
  if i is not None:
   s=([json.loads(l) for l in files[rel].splitlines()] if jsonl else json.loads(files[rel])['steps'])[i];z.update(step_index=i,step_name=s['name'],observed_status=s['status'],format='jsonl' if jsonl else 'path-json')
  return z
 oldrel=prefix+'fresh-prefix/original-steps.jsonl';finrel=prefix+'completed-continuation/'+str(rp.relative_to(final));steps=[];mapping=[]
 for i in list(range(11))+[12,13,14]:
  s=copy.deepcopy(original[i]);need(s['status']=='pass','fresh prerequisite failed');s['observations']=[ref(oldrel,i,True)];steps.append(s)
 # Auth succeeded inside real fresh onboarding; the later reviewer failure remains
 # unchanged in its original record. This derived criterion is not command success.
 steps.append({'name':'OpenRouter was connected through the browser sign-in during onboarding','status':'pass','required':True,'detail':'Actual fresh secret absence, browser callback, and key stored are recorded in the first continuation. That command later failed required review; its failure is retained. Subsequent sample retry used this same key and completed the remaining unchanged criteria.','observations':[ref(prefix+'failed-continuation/gate/D1-continuation.json',0),ref(prefix+'failed-continuation/gate/D1-continuation/logs/onboarding/onboard-20261027-011403.log')]})
 for i,s in enumerate(fin['steps']):
  if i==53:continue
  z=copy.deepcopy(s);z['observations']=[ref(finrel,i)];steps.append(z)
 retryrel=prefix+'service-smoke-retry/'+str(rr.relative_to(retryroot))
 for i,s in enumerate(retry['steps']):
  z=copy.deepcopy(s);z['observations']=[ref(retryrel,i)];steps.append(z)
 for i,s in enumerate(steps):mapping.append({'successor_step':i,'name':s['name'],'sources':s['observations']})
 source_hashes={}
 for rel in ['docs/RELEASE_GATE.md','scripts/release-gate/newuser-linux.sh','scripts/release-gate/lib.sh','scripts/release-gate/evidence.mjs','scripts/release-gate/published-commands.mjs','tests/fixtures/newuser/fixtures.json','crates/axocoatl-config/loadouts/fix.yaml']:
  b=subprocess.check_output(['git','-C',str(R),'show',C+':'+rel]);need((R/rel).read_bytes()==b,'candidate contract changed');source_hashes[rel]=sha(b)
 files[prefix+'candidate-source-hashes.json']=enc(source_hashes);files[prefix+'step-provenance.json']=enc(mapping);files[prefix+'artifact-assessments.json']=enc(audits);files[prefix+'source-copy-index.json']=enc(index);files[prefix+'composer.py']=pathlib.Path(__file__).read_bytes()
 record={**fin,'path':'D1','pass':True,'steps':steps,'finished_at':retry['finished_at'],'started_at':load(FRESH/'operator.json')['at'],'attachments':sorted(files),'continuation':{'schema':'axocoatl.recovery.operator-continuation/1','unsigned':True,'operator':'Codex assistant; actual browser authorization coordinated by root after user permission','human_operator':False,'single_uninterrupted_run':False,'fresh_install_repeated':False,'fresh_auth_preserved':True,'original_gate_D1_json_existed':False,'started_at_source':'D1/continuation/fresh-prefix/operator.json at (original recorded operator setup time; no original path record was finalized)','original_43_failures_preserved':True,'previous_final_continuation':{'pass':False,'passed_steps':70,'failed_steps':1,'optional_skips':1,'affected_failed_step_index':53,'sha256':sha(rp.read_bytes())},'affected_phase_resolution':'The original changed-model onboarding command remains failed. Its configuration change and service restart passed in the original record. The later isolated retry reinstalled the identical service, ran the exact failed sample baseline and task with unchanged model/configuration, then stopped/uninstalled. This successor includes all passing original observations and the entire17step retry; no historical failed command exit is changed.','failed_first_review_preserved':True,'strict_artifact_quality_failures_preserved':True,'artifact_task_failure':{'fixture':'node-deps','result':'FAILED','criterion':'No test changes. All 4 tests pass after the fix.','observed':'A valid additional regression test was added; original four assertions preserved and passed, but explicit no-test-changes instruction violated.','runtime_criterion_only':'Its successful command/check/review/receipt/record support the formal runtime criterion only. The artifact is not accepted.','earlier_wrong_qualification':'Preserved unchanged alongside explicit correction.'},'candidate_binary_sha256':B,'candidate_archive_sha256':A,'historical_global_config_hash_known':False,'configuration_identity':'Same candidate and d1gate account, actual writer/reviewer/checks/environment identities retained per record and receipt; the service writer change is the required D1 configuration-change criterion. No historical whole-YAML hash is invented.','source_of_success':'All unchanged required criteria observed across original fresh setup and continued same-account execution; each step references original bytes. No failed command is relabeled successful.','raw_private_archive_included':False,'prior_private_archive_sha256':fp['original_private_archive_sha256'],'new_inference_by_composer':False}}
 files['D1.json']=enc(record)
 # Reuse collector credential patterns; emit names only, never matches.
 pattern=''
 for line in (R/'scripts/release-gate/collect-evidence.sh').read_text().splitlines():
  if line.startswith("secret_text='"):pattern=line[len("secret_text='"):-1]
  elif line.startswith('secret_text="$secret_text'):pattern+=line[len('secret_text="$secret_text'):-1]
 need(pattern,'collector scanner missing')
 for name,b in files.items():
  text=b.decode(errors='replace');need(not re.search(pattern,text),'credential-shaped data in '+name);need(not re.search(r'[?&](?:code|authorization_code|access_token|refresh_token)=[^\s&"<>]+',text),'callback credential in '+name)
 out.mkdir(mode=0o700)
 for rel,b in files.items():p=out/rel;p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(b)
 js="import{readFileSync,writeFileSync}from'node:fs';import{checkPathEvidence,parseRequiredSteps,requiredStepProblems,buildManifest}from "+json.dumps((R/'scripts/release-gate/evidence.mjs').as_uri())+";let d=process.argv[1],e=JSON.parse(readFileSync(d+'/D1.json')),r=parseRequiredSteps(readFileSync(process.argv[2],'utf8'));let p=[...checkPathEvidence('D1',e,{version:'1.5.0',commit:process.argv[3]}),...requiredStepProblems('D1',e.steps,r.get('D1'))];if(p.length)throw Error(JSON.stringify(p));writeFileSync(d+'/manifest.json',JSON.stringify(buildManifest(d,{version:'1.5.0',commit:process.argv[3]}),null,2)+'\\n');console.log(JSON.stringify({required:r.get('D1').length,steps:e.steps.length}));"
 result=subprocess.run([N,'--input-type=module','-e',js,str(out),str(R/'docs/RELEASE_GATE.md'),C],capture_output=True);need(result.returncode==0,result.stderr.decode())
 for x in index:need(sha(pathlib.Path(x['source']).read_bytes())==x['sha256'] and sha((out/x['destination']).read_bytes())==x['sha256'],'source changed during composition')
 print(json.dumps({'stage':str(out),'unsigned':True,'record_sha256':sha(files['D1.json']),'manifest_sha256':sha((out/'manifest.json').read_bytes()),'validation':json.loads(result.stdout),'original_failed_steps_preserved':43,'source_files':len(index)},indent=2))
if __name__=='__main__':
 try:main()
 except Exception as e:print('REFUSED: '+str(e),file=sys.stderr);sys.exit(1)
