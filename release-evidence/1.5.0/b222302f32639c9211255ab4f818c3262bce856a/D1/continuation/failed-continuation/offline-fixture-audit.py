import argparse,base64,collections,datetime,difflib,hashlib,json,os,pathlib,re,shutil,subprocess,sys,shlex,tarfile
os.umask(0o077)
p=argparse.ArgumentParser(description="Offline acceptance of one completed b222 fixture; original tests run once on a copy, never inference or auth.")
for name in ['repo','gate-dir','out','fixture','label','trusted-public-key','archive','archive-sha256','execution-binary-sha256','execution-provenance','artifact-review']:p.add_argument('--'+name,required=True)
p.add_argument('--sample',action='store_true',help='Only node-plain first-run sample; permits manually-reviewed added tests, never altered original assertions.')
p.add_argument('--source-repo',default='/Users/erick/.codex/worktrees/release-1-5-recovery/axocoatl')
p.add_argument('--verifier',default='/usr/local/bin/axocoatl')
p.add_argument('--verifier-sha256',default='d25cd64f70da4d17eb781d223aeab553f4180d074dbb29af302b23492ce31d1b')
p.add_argument('--node-bin',default='/Users/erick/.local/share/fnm/node-versions/v22.23.2/installation/bin')
p.add_argument('--node-deps-source',default='/Users/erick/.codex/worktrees/release-1-5-recovery/axocoatl/target/recovery-2026-10-25/live-c1-workbench-b222302f/acceptance/host-check')
p.add_argument('--python',default='/Users/erick/.codex/worktrees/release-1-5-recovery/axocoatl/target/recovery-2026-10-25/targeted-openrouter-b222302f/python-pytest/acceptance/host-venv/bin/python')
p.add_argument('--cargo',default='/Users/erick/.cargo/bin/cargo');p.add_argument('--go',default='/opt/homebrew/bin/go')
a=p.parse_args();R=pathlib.Path(a.source_repo);COMMIT='b222302f32639c9211255ab4f818c3262bce856a';CLI=a.verifier;NODE=pathlib.Path(a.node_bin);EXPECTED=a.verifier_sha256
sha=lambda b:hashlib.sha256(b).hexdigest();now=lambda:datetime.datetime.now(datetime.timezone.utc).isoformat()
flavor=a.fixture;review_explanation=a.artifact_review;assert flavor in ['node-plain','node-deps','python-pytest','rust','go'];assert not a.sample or flavor=='node-plain'
label=a.label;S=pathlib.Path(a.gate_dir);repo=pathlib.Path(a.repo);out=pathlib.Path(a.out);assert not out.exists(),'No repeat/overwrite';assert sha(pathlib.Path(CLI).read_bytes())==EXPECTED
archive=pathlib.Path(a.archive);assert sha(archive.read_bytes())==a.archive_sha256
with tarfile.open(archive,'r:gz') as tf:
 members=[m for m in tf.getmembers() if m.isfile() and pathlib.PurePosixPath(m.name).name=='axocoatl'];assert len(members)==1;assert sha(tf.extractfile(members[0]).read())==a.execution_binary_sha256
provenance=json.loads(pathlib.Path(a.execution_provenance).read_text());assert provenance['candidate_commit']==COMMIT and provenance['binary_sha256']==a.execution_binary_sha256 and provenance['archive_sha256']==a.archive_sha256
if a.sample:receipt_name='receipt.json';record_name='record.jsonl';junit_name=None
else:
 argv=shlex.split((S/'command.txt').read_text());flag=lambda name:next((v.split('=',1)[1] if '=' in v else argv[i+1] for i,v in enumerate(argv) if v==name or v.startswith(name+'=')),None)
 receipt_name=pathlib.Path(flag('--receipt')).name;record_name=pathlib.Path(flag('--record')).name;junit_name=pathlib.Path(flag('--junit')).name
assert (S/receipt_name).is_file() and (S/record_name).is_file(),'No completed receipt/record yet'

prod={'node-plain':'src/paging.js','node-deps':'src/paginate.js','python-pytest':'pages.py','rust':'src/lib.rs','go':'pages.go'}[flavor]
def cap(args,cwd=repo,env=None):
 childenv=(os.environ if env is None else env).copy();childenv['GIT_OPTIONAL_LOCKS']='0';return subprocess.run(args,cwd=cwd,env=childenv,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
def git(*args):
 r=cap(['git',*args]);assert r.returncode==0,r.stderr.decode();return r.stdout
statement=json.loads(base64.b64decode(json.loads((S/receipt_name).read_text())['payload']));pred=statement['predicate'];base=pred['repository']['base_commit'];assert git('rev-parse','HEAD').decode().strip()==base and pred['run']['finished_at_ms'];assert cap(['git','merge-base','--is-ancestor',COMMIT,'HEAD'],R).returncode==0; ancestry=cap(['git','diff','--name-only',COMMIT,'HEAD'],R).stdout.decode().splitlines(); assert all(p.startswith(('release-evidence/','release-gate-keys/','release-gate-overrides/')) for p in ancestry), 'Non-evidence source changes after candidate'; assert cap(['git','diff','--quiet',COMMIT,'--','scripts/release-gate','tests/fixtures/newuser/'+flavor],R).returncode==0
before=git('status','--porcelain=v1','--untracked-files=all');files=git('ls-files','-z').decode().rstrip('\0').split('\0');untracked=git('ls-files','--others','--exclude-standard').decode().splitlines();gate_outputs={x for x in [receipt_name,record_name,junit_name] if x};unexpected=[f for f in untracked if f not in gate_outputs];out.mkdir(parents=True,mode=0o700);commands=[];hashes={};basefiles={};finalfiles={};template_mismatches=[]
def write(name,x):(out/name).write_text(json.dumps(x,indent=2)+'\n')
def saved(args,label,cwd=repo,env=None):
 c={'name':label,'argv':args,'cwd':str(cwd),'start':now(),'status':'started'};commands.append(c);write('commands.json',commands);r=cap(args,cwd,env);(out/(label+'.stdout')).write_bytes(r.stdout);(out/(label+'.stderr')).write_bytes(r.stderr);c.update(status='completed',end=now(),exit_code=r.returncode,stdout_sha256=sha(r.stdout),stderr_sha256=sha(r.stderr));write('commands.json',commands);return r
for f in files:
 old=git('show',base+':'+f);assert not (repo/f).is_symlink(),'Unexpected tracked symlink: '+f;final=(repo/f).read_bytes();basefiles[f]=old;finalfiles[f]=final;template=R/'tests/fixtures/newuser'/flavor/f
 if not template.is_file() or template.read_bytes()!=old:template_mismatches.append(f)
 for sub,data in [('original',old),('final',final),('host-check',final if f==prod else old)]:
  p=out/sub/f;p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(data)
 hashes[f]={'original_sha256':sha(old),'final_sha256':sha(final),'unchanged':old==final}
for f in unexpected:
 p=repo/f
 if p.is_file():d=out/'unexpected-untracked'/f;d.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(p,d,follow_symlinks=False)
for p in S.iterdir():
 if p.is_file():shutil.copy2(p,out/p.name)
(out/'patch.diff').write_bytes(git('diff','--binary',base,'--'));(out/'git-status-before.txt').write_bytes(before);write('receipt-statement.json',statement);write('gate-output-exclusions.json',[{'path':f,'reason':'Generated by the official gate output flags; excluded from model scratch files','sha256':sha((repo/f).read_bytes())} for f in untracked if f in gate_outputs])
public=pathlib.Path(a.trusted_public_key).read_text().strip();assert public.split()[0]=='ed25519' and public.split()[1]==pred['signer']['public_key'];assert sha(base64.b64decode(public.split()[1]))==pred['signer']['key_id'].removeprefix('sha256:');(out/'trusted-public-key.txt').write_text(public+'\n');write('execution-provenance.json',provenance);write('audit-inputs.json',vars(a))
verified=saved([CLI,'receipt','verify',str(S/receipt_name),'--key',public,'--repo',str(repo),'--record',str(S/record_name),'--require-pass','--json'],'independent-receipt-verify');record=saved([CLI,'record','verify',str(S/record_name)],'independent-record-verify');markup=saved([str(NODE/'node'),str(R/'scripts/release-gate/record-answers.mjs'),str(S/record_name)],'accepted-answers')
host=out/'host-check';env=os.environ.copy();env.update(PATH=str(NODE)+':/Users/erick/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin',PYTHONDONTWRITEBYTECODE='1',PYTHONPATH=str(host),PYTEST_DISABLE_PLUGIN_AUTOLOAD='1',npm_config_cache=str(out/'npm-cache'),npm_config_update_notifier='false');testargs=None;runtime=None;dependency={}
if flavor.startswith('node-'):
 runtime=saved([str(NODE/'node'),'--version'],'runtime-version',host,env);assert runtime.stdout.strip()==b'v22.23.2'
 if flavor=='node-deps':
  origin=pathlib.Path(a.node_deps_source);assert (origin/'package-lock.json').read_bytes()==(host/'package-lock.json').read_bytes();shutil.copytree(origin/'node_modules',host/'node_modules',symlinks=True);assert (host/'node_modules/.bin/uvu').is_symlink();dependency={'copied_from':str(origin/'node_modules'),'symlinks_preserved':True,'lock_sha256':sha((host/'package-lock.json').read_bytes()),'new_install_or_download':False}
 testargs=[str(NODE/'npm'),'test']
elif flavor=='python-pytest':
 py=pathlib.Path(a.python);runtime=saved([str(py),'--version'],'runtime-version',host,env);assert b'3.13.7' in runtime.stdout;select=saved([str(py),'-B','-c','import pathlib,pages,pytest; assert pathlib.Path(pages.__file__).resolve()==pathlib.Path("pages.py").resolve();print(pages.__file__);print(pytest.__version__)'],'module-selection',host,env);assert select.returncode==0;dependency={'existing_venv':str(py.parent.parent),'actual_snapshot_module_verified':True,'new_install_or_download':False};testargs=[str(py),'-B','-m','pytest','-p','no:cacheprovider']
elif flavor=='rust':
 cargo=a.cargo;runtime=saved([cargo,'+1.95.0','--version'],'runtime-version',host,env);assert runtime.returncode==0 and b'1.95.0' in runtime.stdout;env['CARGO_TARGET_DIR']=str(out/'cargo-target');env['CARGO_NET_OFFLINE']='true';testargs=[cargo,'+1.95.0','test','--offline','--jobs','1'];dependency={'cargo_home':os.environ.get('CARGO_HOME','/Users/erick/.cargo'),'offline':True,'target':env['CARGO_TARGET_DIR'],'new_download':False}
else:
 go=a.go;runtime=saved([go,'version'],'runtime-version',host,env);assert runtime.returncode==0 and b'go1.26.2' in runtime.stdout;env.update(GOCACHE=str(out/'go-build'),GOTOOLCHAIN='local',GOPROXY='off',GOSUMDB='off',GOMAXPROCS='2');testargs=[go,'test','-json','-p','1','./...'];dependency={'gomodcache':'/Users/erick/go/pkg/mod','offline':True,'gocache':env['GOCACHE'],'new_download':False}
write('host-dependency-provenance.json',dependency)
# Test the predeclared original suite exactly once, even if the model edited tests.
restored=[]
for f,data in basefiles.items():
 if f.startswith(('test/','tests/')) or f.endswith('_test.go'):
  (host/f).write_bytes(data);restored.append(f)
if flavor=='rust':
 marker=b'#[cfg(test)]';assert marker in basefiles[prod] and marker in finalfiles[prod];(host/prod).write_bytes(finalfiles[prod].split(marker,1)[0]+marker+basefiles[prod].split(marker,1)[1]);restored.append(prod+' from #[cfg(test)] onward')
write('original-test-suite-provenance.json',{'original_tests_copied_from_base':base,'paths':restored,'single_invocation':True})
test=saved(testargs,'original-tests',host,env)
text=test.stdout.decode(errors='replace');expected_count=3 if flavor=='node-plain' else 4
if flavor=='go':executed=sum(1 for l in text.splitlines() if l.startswith('{') and (e:=json.loads(l)).get('Action')=='pass' and e.get('Test'))
elif flavor=='node-plain':m=re.search(r'^# pass (\d+)$',text,re.M);executed=int(m[1]) if m else 0
elif flavor=='node-deps':m=re.search(r'Passed:\s+(\d+)',text);executed=int(m[1]) if m else 0
else:m=re.search(r'(\d+) passed',text);executed=int(m[1]) if m else 0
suite_count_ok=executed==expected_count
rows=[json.loads(l) for l in (S/record_name).read_text().splitlines()];team=next(r['data'] for r in rows if r.get('section')=='team');outcome=next(r['data'] for r in rows if r.get('section')=='outcome');history=next(r['data'] for r in rows if r.get('section')=='history');network=[r['data'] for r in rows if r.get('section')=='network'];roles=[];errors=[];retries=[]
for h in history:
 for activation in h['turn']['activations']:
  role=activation['definition_name']['content'];events=[e['content'] for e in activation.get('stream',[])];counts=collections.Counter(e['payload']['kind'] for e in events);errors.extend({'role':role,'sequence':e['sequence'],'payload':e['payload'],'raw_cause':'Not inferred from public result hash/length'} for e in events if e['payload']['kind']=='tool_result' and e['payload'].get('is_error'));retries.extend({'role':role,'sequence':e['sequence'],'payload':e['payload']} for e in events if e['payload']['kind']=='provider_retry');roles.append({'role':role,'state':activation['activation']['state'],'failure':activation['activation'].get('failure'),'currently_accepted':activation['currently_accepted'],'event_counts':dict(counts),'usage':(activation.get('output') or {}).get('content',{}).get('usage'),'events':[e for e in events if e['payload']['kind'] not in ['reasoning_summary','text']]})
write('team-configuration.json',team);write('activation-summary.json',roles);write('network-events.json',network);denied=[e for e in network if e.get('event',{}).get('decision')=='deny'];negative={'tool_errors':errors,'provider_retries':retries,'denied_events':denied,'recorded_baseline_probe':pred['sandbox'].get('environment',{}).get('probe'),'failed_activations':[r for r in roles if r['failure'] is not None]};write('negative-evidence.json',negative);write('outcome.json',outcome)
changed=[f for f,v in hashes.items() if not v['unchanged']];tests_same=all(v['unchanged'] for f,v in hashes.items() if f.startswith(('test/','tests/')) or f.endswith('_test.go'))
if flavor=='rust':
 marker=b'#[cfg(test)]';tests_same=marker in basefiles[prod] and marker in finalfiles[prod] and basefiles[prod][basefiles[prod].index(marker):]==finalfiles[prod][finalfiles[prod].index(marker):]
fixture_spec=json.loads((R/'tests/fixtures/newuser/fixtures.json').read_text())['fixtures'][flavor];environment=pred['sandbox'].get('environment',{});image_ok=fixture_spec['image_contains'] in environment.get('image','');setup_expected=fixture_spec.get('setup_contains');setup_ok=not setup_expected or setup_expected in json.dumps(environment.get('setup',{}));after=git('status','--porcelain=v1','--untracked-files=all');assert after==before and all(sha((repo/f).read_bytes())==v['final_sha256'] for f,v in hashes.items());(out/'git-status-after.txt').write_bytes(after)
sample_additions_valid=False
if flavor=='node-plain':
 testfiles=[f for f in files if f.startswith(('test/','tests/'))];sample_additions_valid=all(all(tag in ['equal','insert'] for tag,*_ in difflib.SequenceMatcher(None,basefiles[f].splitlines(keepends=True),finalfiles[f].splitlines(keepends=True)).get_opcodes()) for f in testfiles) and all(hashes[f]['unchanged'] for f in files if f!=prod and f not in testfiles)
shape_ok=(changed==[prod] and tests_same) or (flavor=='node-plain' and sample_additions_valid and prod in changed and all(f==prod or f.startswith(('test/','tests/')) for f in changed))
required_checks_observed=bool(team.get('required_checks')) and bool(outcome.get('checks')) and all(c.get('state')=='passed' and c.get('exit_code')==0 for c in outcome.get('checks',[]))
required_review_observed=team.get('required_review') is not None and outcome.get('review',{}).get('passed') is True
accepted=required_checks_observed and required_review_observed and suite_count_ok and all(r.returncode==0 for r in [verified,record,markup,test]) and shape_ok and not unexpected and not template_mismatches and image_ok and setup_ok
assessment={'schema':'axocoatl.recovery.fixture-acceptance/1','source_commit':COMMIT,'execution_archive_sha256':a.archive_sha256,'execution_binary_sha256':a.execution_binary_sha256,'verifier_binary_sha256':EXPECTED,'assessed_at':now(),'fixture':flavor,'label':label,'repo':str(repo),'base_commit':base,'run_id':pred['run']['run_id'],'result':('ACCEPTED_WITH_ADDED_TESTS' if not tests_same else 'ACCEPTED') if accepted else 'FAILED','artifact_review':review_explanation,'changed_files':changed,'original_tests_unchanged':tests_same,'sample_original_assertions_preserved':sample_additions_valid if flavor=='node-plain' else None,'all_other_tracked_files_unchanged':all(v['unchanged'] for f,v in hashes.items() if f!=prod),'unexpected_untracked_files':unexpected,'gate_generated_outputs_excluded':sorted(set(untracked)&gate_outputs),'template_baseline_mismatches':template_mismatches,'file_hashes':hashes,'host_test':{'commands':commands,'original_suite_invocations':1,'exit_code':test.returncode,'expected_original_tests':expected_count,'observed_passed_tests':executed,'suite_count_verified':suite_count_ok,'production_files_exact_final_copy':True,'test_files_original_base_copy':True,'independent_baseline_rerun':False},'recorded_host_checks':outcome.get('checks'),'required_checks_passed':required_checks_observed,'required_review_passed':required_review_observed,'configuration':{'writer':pred['agents'],'reviewer':pred['review'],'helpers':max(0,len(team.get('slots',[]))-1),'host_required_checks':bool(team.get('required_checks')),'required_review':team.get('required_review') is not None,'environment':environment},'environment_contract':{'image_matches':image_ok,'setup_matches':setup_ok,'fixture_expectation':fixture_spec},'review_detail':outcome.get('review'),'usage':pred['usage'],'negative_evidence':negative,'gate_scope':'Independent artifact acceptance only; original gate attempts and outcomes are never changed.','network':pred['network'],'verification':{'receipt_exit':verified.returncode,'record_exit':record.returncode,'markup_exit':markup.returncode,'original_repo_unchanged_by_audit':True},'scope':'Independent acceptance of a completed actual Mac or Linux b222 artifact; no fresh-authentication, clean-install, whole-gate, or release completion claim.','new_inference':False,'source_config_or_runtime_changes':False,'evidence_hashes':{f:sha((out/f).read_bytes()) for f in [receipt_name,record_name,'patch.diff','negative-evidence.json','activation-summary.json','outcome.json','original-tests.stdout','original-tests.stderr']}}
write('assessment.json',assessment);print(json.dumps({'fixture':label,'result':assessment['result'],'changed_files':changed,'original_tests_unchanged':tests_same,'sample_original_assertions_preserved':sample_additions_valid if flavor=='node-plain' else None,'test_exit':test.returncode,'test_output':test.stdout.decode(),'test_stderr':test.stderr.decode()[-1500:],'receipt_exit':verified.returncode,'record_exit':record.returncode,'markup_exit':markup.returncode,'tool_errors':len(errors),'provider_retries':len(retries),'network_denials':len(denied),'unexpected_untracked':unexpected,'assessment':str(out/'assessment.json')},indent=2))
