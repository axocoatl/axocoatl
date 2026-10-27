import pathlib,json,hashlib,copy,subprocess,datetime
R=pathlib.Path('/Users/erick/.codex/worktrees/release-1-5-recovery/axocoatl');T=R/'target/recovery-2026-10-25';C='b222302f32639c9211255ab4f818c3262bce856a';N='/Users/erick/.local/share/fnm/node-versions/v22.23.2/installation/bin/node';B='d25cd64f70da4d17eb781d223aeab553f4180d074dbb29af302b23492ce31d1b';A='2a9fc208948c630efab48d2f58875eb0c8f6dd24baec005479f691bdc532d659'
O=pathlib.Path('/private/tmp/axocoatl-b222-openrouter-completion-20261027');assert not O.exists()
sha=lambda b:hashlib.sha256(b).hexdigest();enc=lambda x:(json.dumps(x,indent=2)+'\n').encode();load=lambda p:json.loads(p.read_bytes())
prefix='C1-openrouter/continuation/';files={};index=[];omitted=[]
def capture(root,relative,filter_cache=False):
 for p in sorted(root.rglob('*')):
  rel=p.relative_to(root)
  if filter_cache and any(x in {'node_modules','npm-cache','__pycache__','host-venv','cargo-target','go-build'} for x in rel.parts):
   if p.is_file():omitted.append({'source':str(p),'reason':'dependency or runtime cache; provenance and original test output retained'})
   continue
  assert not p.is_symlink(),str(p)
  if p.is_file():
   d=p.read_bytes();dest=relative+str(rel);files[dest]=d;index.append({'source':str(p),'destination':dest,'sha256':sha(d),'bytes':len(d)})
freshroot=T/'provider-continuation-b222302f/openrouter-retry1';freshpath=freshroot/'1.5.0'/C/'C1-openrouter.json';old=load(freshpath)
suproot=T/'provider-continuation-b222302f/openrouter-fixtures-retry1';suppath=suproot/'1.5.0'/C/'supplemental-openrouter-three-fixtures.json';sup=load(suppath)
assert old['commit']==sup['commit']==C and old['pass'] is False and sup['pass'] is True and len(old['steps'])==37 and len(sup['steps'])==28
assert all(s['status']=='pass' for s in sup['steps'])
assert [i for i,s in enumerate(old['steps']) if s['status']=='fail']==list(range(13,36))
assert old['steps'][3]['exit_code']==0 and old['steps'][4]['status']==old['steps'][5]['status']=='pass'
prov=load(suppath.parent/'supplemental-openrouter-three-fixtures/provenance.json');assert prov['candidate_commit']==C and prov['binary_sha256']==B and prov['archive_sha256']==A
assert prov['configuration_only_difference']=={'server.port':{'from':8080,'to':18080}} and prov['authentication_disabled'] is False and prov['global_configuration_mutated'] is False
# Inspect copied YAML at byte level: only server.port differs, owner bytes have not changed.
work=pathlib.Path('/Users/erick/axocoatl-gate-work/provider-continuation-b222302f-openrouter-fixtures-retry1')
cfg=(pathlib.Path.home()/'Library/Application Support/Axocoatl/config.yaml').read_bytes();isolated=(work/'config.port18080.yaml').read_bytes()
assert sha(cfg)==prov['owner_configuration_sha256'] and sha(isolated)==prov['execution_configuration_sha256'] and isolated.replace(b'  port: 18080',b'  port: 8080')==cfg
assessments={}
for label in ('openrouter-node-deps','openrouter-python-pytest','claude-openrouter-node-deps'):
 p=suproot/'acceptance'/label;a=load(p/'assessment.json');assert a['source_commit']==C and a['original_tests_unchanged'] is True
 assert a['host_test']['exit_code']==0 and a['host_test']['observed_passed_tests']==4 and a['host_test']['original_suite_invocations']==1
 assert all(a['verification'][k]==0 for k in ('receipt_exit','record_exit','markup_exit')) and not a['unexpected_untracked_files']
 assert a['execution_binary_sha256']==B and a['execution_archive_sha256']==A
 for rel,h in a['evidence_hashes'].items():assert sha((p/rel).read_bytes())==h
 if label=='openrouter-node-deps':assert a['result']=='FAILED' and a['changed_files']==['README.md','src/paginate.js']
 else:assert a['result']=='ACCEPTED'
 assert a['configuration']['reviewer']['provider']=='openrouter' and a['configuration']['reviewer']['model']=='openai/gpt-oss-120b'
 if label.startswith('openrouter-'):assert a['configuration']['writer'][0]['model']=='qwen/qwen3-coder-30b-a3b-instruct'
 else:assert a['configuration']['writer'][0]['runtime']=='claude-code'
 assessments[label]=a
sample=pathlib.Path('/private/tmp/axocoatl-mac-or-retry1-acceptance/sample-key-corrected');sa=load(sample/'assessment.json')
assert sa['source_commit']==C and sa['host_test']['exit_code']==0 and sa['host_test']['observed_passed_tests']==3 and all(sa['verification'][k]==0 for k in ('receipt_exit','record_exit','markup_exit'))
for rel,h in sa['evidence_hashes'].items():assert sha((sample/rel).read_bytes())==h
capture(freshroot,prefix+'fresh-prerequisites/');capture(T/'provider-continuation-b222302f/openrouter',prefix+'previous-oauth-failure/')
capture(suproot,prefix+'isolated-fixtures/',True);capture(sample,prefix+'first-sample-acceptance/',True)
oldrel=prefix+'fresh-prerequisites/1.5.0/'+C+'/C1-openrouter.json';newrel=prefix+'isolated-fixtures/1.5.0/'+C+'/supplemental-openrouter-three-fixtures.json'
def ref(rel,i=None):
 x={'file':rel,'sha256':sha(files[rel])}
 if i is not None:
  s=json.loads(files[rel])['steps'][i];x.update(step_index=i,step_name=s['name'],observed_status=s['status'])
 return x
result=copy.deepcopy(old);result['pass']=True;result['finished_at']=sup['finished_at'];mapping=[]
for i,s in enumerate(result['steps']):
 if i in list(range(13,27))+list(range(28,35)):
  hits=[j for j,n in enumerate(sup['steps']) if n['name']==s['name']];assert len(hits)==1
  j=hits[0];result['steps'][i]=copy.deepcopy(sup['steps'][j]);refs=[ref(newrel,j)]
 elif i==12:
  j=next(j for j,n in enumerate(sup['steps']) if n['name']=='listener belongs to owned Mac daemon');result['steps'][i]=copy.deepcopy(sup['steps'][j]);refs=[ref(newrel,j)]
  result['steps'][i]['detail']+='; replaces earlier misleading8080health observation: that listener was Lima Linux forwarding, not the Mac daemon'
 elif i in (27,35):
  indices=range(13,27) if i==27 else range(28,35)
  refs=[ref(newrel,next(j for j,n in enumerate(sup['steps']) if n['name']==old['steps'][k]['name'])) for k in indices]
  result['steps'][i]={'name':s['name'],'status':'pass','required':True,'detail':'Derived from the complete later successful dependent fixture blocks after operational port isolation; original401failures preserved. Independent original tests pass; Node extra-README artifact-score failure retained separately.'}
 else:refs=[ref(oldrel,i)]
 result['steps'][i]['observations']=refs;mapping.append({'successor_step':i,'name':result['steps'][i]['name'],'sources':refs})
for name in ('owned isolated Mac daemon stopped with exit0','global owner configuration unchanged'):
 j=next(j for j,n in enumerate(sup['steps']) if n['name']==name);s=copy.deepcopy(sup['steps'][j]);s['observations']=[ref(newrel,j)];result['steps'].append(s);mapping.append({'successor_step':len(result['steps'])-1,'name':name,'sources':s['observations']})
annotation={'schema':'axocoatl.recovery.artifact-contract-annotation/1','predeclared_shape_score':'FAILED','score_changed':False,'fixture':'openrouter-node-deps','actual_explicit_contract':'No test changes','original_tests_unchanged':True,'independent_original_tests':'4/4 passed once','artifact':'Correct items.length-start fix plus extra README explanation','readme_review':'Relevant and correct before/after example and explanation. Redundant headings and doubled period. No unsafe instructions or false capability claims. Extra documentation remains a recorded artifact-quality deviation.','gate_criteria_weakened':False,'assessment':ref(prefix+'isolated-fixtures/acceptance/openrouter-node-deps/assessment.json')}
files[prefix+'artifact-contract-annotation.json']=enc(annotation)
files[prefix+'step-provenance.json']=enc(mapping)
files[prefix+'composer.py']=pathlib.Path(__file__).read_bytes()
files[prefix+'source-copy-index.json']=enc(index);files[prefix+'omitted-cache-payload.json']=enc(omitted)
source_hashes={}
for rel in ('docs/RELEASE_GATE.md','scripts/release-gate/newuser-mac.sh','scripts/release-gate/lib.sh','scripts/release-gate/evidence.mjs','crates/axocoatl-config/loadouts/fix.yaml','crates/axocoatl-config/loadouts/fix-claude-code.yaml'):
 d=subprocess.check_output(['git','-C',str(R),'show',C+':'+rel]);assert (R/rel).read_bytes()==d;source_hashes[rel]=sha(d)
files[prefix+'candidate-source-hashes.json']=enc(source_hashes)
result['continuation']={'schema':'axocoatl.recovery.operator-continuation/1','unsigned':True,'operator':'Codex assistant through owned PTY; root browser operator','human_operator':False,'single_uninterrupted_run':False,'fresh_onboarding_and_browser_auth_reused_from_original_passed_steps':True,'original_path_pass':False,'original401failures_preserved':True,'global_configuration_changed':False,'intervention':'Port8080health hit LimaD1 forwarded daemon; MacAPI token was rejected401beforeadmission. Dedicated ownedMac18080 plus configuration copy differing only server.port and explicitrealMacdata root/URL resolved it without disabling authentication or changing product source. Published base commands were retained with explicit--config routing appended.','original_daemon_health_observation_replaced':True,'candidate_binary_sha256':B,'candidate_archive_sha256':A,'sample_acceptance':'Actualfreshonboardingfirstsample independently passed3originaltests; no repeat','artifact_score_deviation_retained':annotation,'original_tests_run_once_per_completed_fixture':True,'new_inference_by_composer':False,'remaining_scope':'OnlyC1-openrouter; other required paths, unsigned review and publication separate.'}
result['attachments']=sorted(files);files['C1-openrouter.json']=enc(result)
O.mkdir(mode=0o700)
for rel,d in files.items():p=O/rel;p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(d)
module=(R/'scripts/release-gate/evidence.mjs').as_uri()
js="import{checkPathEvidence,parseRequiredSteps,requiredStepProblems,buildManifest}from "+json.dumps(module)+";import{readFileSync,writeFileSync}from'node:fs';let d=process.argv[1],e=JSON.parse(readFileSync(d+'/C1-openrouter.json')),r=parseRequiredSteps(readFileSync(process.argv[2],'utf8'));let p=[...checkPathEvidence('C1-openrouter',e,{version:'1.5.0',commit:process.argv[3]}),...requiredStepProblems('C1-openrouter',e.steps,r.get('C1-openrouter'))];console.log(JSON.stringify({problems:p,required_steps:r.get('C1-openrouter').length}));if(p.length)process.exitCode=1;else writeFileSync(d+'/manifest.json',JSON.stringify(buildManifest(d,{version:'1.5.0',commit:process.argv[3]}),null,2)+'\\n');"
p=subprocess.run([N,'--input-type=module','-e',js,str(O),str(R/'docs/RELEASE_GATE.md'),C],capture_output=True);assert p.returncode==0,p.stderr.decode();validation=json.loads(p.stdout)
for x in index:assert sha(pathlib.Path(x['source']).read_bytes())==x['sha256'] and sha((O/x['destination']).read_bytes())==x['sha256']
print(json.dumps({'output':str(O),'unsigned':True,'validation':validation,'steps':len(result['steps']),'successor_sha256':sha(files['C1-openrouter.json']),'manifest_sha256':sha((O/'manifest.json').read_bytes()),'original_failures_preserved':True,'all_actual_original_tests_pass':True,'node_artifact_score_failed_preserved':True},indent=2))
