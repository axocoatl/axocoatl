#!/usr/bin/env python3
"""Bounded offline Codex criterion composition. No inference, auth, signing or source writes."""
import argparse, base64, copy, hashlib, json, pathlib, re, subprocess
from datetime import datetime, timezone
C='b222302f32639c9211255ab4f818c3262bce856a'
B='d25cd64f70da4d17eb781d223aeab553f4180d074dbb29af302b23492ce31d1b'
A='2a9fc208948c630efab48d2f58875eb0c8f6dd24baec005479f691bdc532d659'
CONFIG='bc0877f914dcabba791ffcc6b9c56ab23d1a0b363ff5ba2674bb8e11a6dfc990'
R=pathlib.Path('/Users/erick/.codex/worktrees/release-1-5-recovery/axocoatl')
T=R/'target/recovery-2026-10-25'
P=pathlib.Path('/private/tmp/axocoatl-b222-provider-continuation-20261027')
NODE='/Users/erick/.local/share/fnm/node-versions/v22.23.2/installation/bin/node'
PREFIX='C1-codex/continuation/'
SIGNED=PREFIX+'original-signed-bundle/'
FRESH=PREFIX+'fresh-interrupted/'
SUP='C1-codex/supplemental/'
SAMPLE=SUP+'one-hour-onboarding-no-connect/'
REQUIRED=[
 'no Codex sign-in was stored before this path',
 'onboarding was answered live in the terminal',
 'Codex billing was set to the ChatGPT plan',
 'Codex was signed in with ChatGPT in the browser during onboarding',
 'doctor: ready, no FAIL',
 "the site's fix-codex command passed on node-deps and python-pytest",
]
def need(x,why):
 if not x:raise ValueError(why)
def sha(x):return hashlib.sha256(x).hexdigest()
def enc(x):return (json.dumps(x,indent=2,ensure_ascii=False)+'\n').encode()
def now():return datetime.now(timezone.utc).isoformat()
def readjson(p):return json.loads(p.read_bytes())
def command(args):
 p=subprocess.run(args,capture_output=True)
 need(p.returncode==0,'verification command failed: '+p.stderr.decode()[:700])
 return p.stdout
def public_ref(files,name,index=None):
 d={'file':name,'sha256':sha(files[name])}
 if index is not None:
  s=json.loads(files[name])['steps'][index]
  d.update(step_index=index,step_name=s['name'],observed_status=s['status'])
 return d
def record(data):return {r['section']:r['data'] for r in map(json.loads,data.splitlines()) if r.get('section') in ('manifest','loadout','team','outcome')}
def execution_identity(a):
 c=a['configuration'];w=c['writer'];v=c['reviewer']
 need(len(w)==1,'not one writer')
 result={'writer':w,'reviewer':{k:v[k] for k in ('provider','model','runtime','program_version')},'helpers':c['helpers'],'host_required_checks':c['host_required_checks'],'required_review':c['required_review']}
 need(w[0]['runtime']=='codex' and w[0]['program_version']=='0.160.1' and w[0]['model']=='gpt-6.1-sol' and w[0]['credential']=='codex-chatgpt','wrong Codex execution identity')
 need(result['reviewer']=={'provider':'ollama','model':'gpt-oss:20b','runtime':'native','program_version':'ollama 0.40.11'},'wrong reviewer')
 need(c['helpers']==0 and c['host_required_checks'] is True and c['required_review'] is True,'required team changed')
 return result
def assessment(files,prefix,sample=False):
 a=json.loads(files[prefix+'assessment.json'])
 need(a['source_commit']==C,'artifact candidate mismatch')
 need(a['result']==('ACCEPTED_WITH_ADDED_TESTS' if sample else 'ACCEPTED'),'artifact not accepted')
 for k in ('receipt_exit','record_exit','markup_exit'):need(a['verification'][k]==0,'dependent verification failed')
 need(a['host_test']['exit_code']==0,'original tests failed')
 need(a['host_test']['original_suite_invocations']==1,'tests repeated')
 if sample:
  need(a['original_tests_preserved_byte_for_byte_after_removing_additions'] is True,'original sample assertions changed')
 else:
  need(a['original_tests_unchanged'] and a['all_other_tracked_files_unchanged'] and not a['unexpected_untracked_files'],'fixture contract changed')
 for n,h in a['evidence_hashes'].items():need(sha(files[prefix+n])==h,'artifact dependency hash mismatch: '+n)
 return a
def main(out):
 need(not out.exists(),'new output directory required')
 bundle=R/'release-evidence/1.5.0'/C
 original_manifest=(bundle/'manifest.json').read_bytes();original_dsse=(bundle/'manifest.dsse.json').read_bytes()
 m=json.loads(original_manifest);original={x['path']:(bundle/x['path']).read_bytes() for x in m['files']}
 for x in m['files']:need(sha(original[x['path']])==x['sha256'] and len(original[x['path']])==x['bytes'],'signed source changed')
 original.update({'manifest.json':original_manifest,'manifest.dsse.json':original_dsse})
 need(all(not (bundle/n).is_symlink() for n in original),'symlink in signed sources')
 module=R/'scripts/release-gate/evidence.mjs'
 code="""import{verifyBundle,parseKeys}from MODULE;import{readFileSync}from'node:fs';let v=verifyBundle(process.argv[1],{keys:parseKeys(readFileSync(process.argv[2],'utf8')),version:'1.5.0',commit:process.argv[3]});console.log(JSON.stringify({problems:v.problems,key:v.key?.shortId}));if(v.problems.length)process.exitCode=1;""".replace('MODULE',json.dumps(module.as_uri()))
 signature=json.loads(command([NODE,'--input-type=module','-e',code,str(bundle),str(R/'.axocoatl/gate-keys'),C]))
 prov=readjson(P/'input-manifest.json');need(prov['candidate_commit']==C and prov['binary_sha256']==B and prov['archive_sha256']==A,'input candidate mismatch')
 sources={}
 names=set(prov['gate_input_sha256'])|{'docs/RELEASE_GATE.md','crates/axocoatl-config/loadouts/fix-codex.yaml'}
 for n in sorted(names):
  d=command(['git','-C',str(R),'show',C+':'+n]);need((R/n).read_bytes()==d,'candidate source changed: '+n)
  if n in prov['gate_input_sha256']:need(sha(d)==prov['gate_input_sha256'][n],'launch source lock mismatch')
  sources[n]=d
 need(sha(pathlib.Path(prov['binary']).read_bytes())==B and sha(pathlib.Path(prov['archive']).read_bytes())==A,'binary/archive changed')
 old=json.loads(original['C1-codex.json']);need(old['pass'] is False and old['steps'][3]['exit_code']==1,'old failure changed')
 fresh_root=T/'provider-continuation-b222302f/codex/1.5.0'/C
 fresh_json=(fresh_root/'C1-codex.json').read_bytes();fresh=json.loads(fresh_json)
 need(fresh['commit']==C and fresh['pass'] is False and len(fresh['steps'])==5 and fresh['steps'][3]['exit_code']==130,'fresh interrupted attempt changed')
 need(fresh['steps'][2]['name']==REQUIRED[0] and fresh['steps'][2]['status']=='pass','fresh absence missing')
 fresh_files={'C1-codex.json':fresh_json}
 for n in fresh['attachments']:
  need(not (fresh_root/n).is_symlink(),'fresh source symlink');fresh_files[n]=(fresh_root/n).read_bytes()
 transcript=fresh_files['C1-codex/transcript.txt'].decode(errors='replace')
 for marker in ('Successfully logged in','stored your ChatGPT sign-in as secret codex-chatgpt','ChatGPT plan','Ready: yes','Run a first fix now'):
  need(marker in transcript,'missing actual fresh observation: '+marker)
 need('Step 7 · A first run' not in transcript,'fresh sample unexpectedly ran')
 restore=readjson(P/'codex-saved-credential-restored.json')
 prior=readjson(pathlib.Path('/private/tmp/axocoatl-b222-codex-signin-backup-e_8domxi/manifest.json'))
 issued=readjson(pathlib.Path('/private/tmp/axocoatl-b222-codex-new-login-preserved-27rjx87k/manifest.json'))
 need(restore['active_bytes_identical_to_saved'] is True and restore['owner_config_unchanged'] is True and restore['fresh_runner_exit']==130 and restore['new_inference'] is False,'restoration not observed')
 need(restore['restored_bytes_sha256']==prior['opaque_copy_sha256'],'saved credential bytes not restored')
 emails=issued['explicit_email_metadata'];need(len(emails)==1 and emails[0]['email']==restore['email'],'callback account differs from saved account')
 need(prior['config_sha256_before']==CONFIG and prior['name_only_absence_verified'] is True and prior['config_unchanged'] is True,'initial signed-out/config proof missing')
 sup=json.loads(original[SUP+'supplemental-codex-fixtures-only.json'])
 need(sup['commit']==C and sup['pass'] is True and len(sup['steps'])==27 and all(s['status']=='pass' for s in sup['steps']),'supplemental dependent steps not all pass')
 cfg=json.loads(original[SUP+'supplemental-codex-fixtures-only/configuration.json'])
 need(cfg['configuration_snapshot_sha256']==CONFIG and cfg['configuration_mutated'] is False and cfg['credential']=='codex-chatgpt' and cfg['reviewer']=='ollama:gpt-oss:20b','effective owner config mismatch')
 runtime_prov=json.loads(original[SUP+'supplemental-codex-fixtures-only/provenance.json'])
 need(runtime_prov['candidate_commit']==C and runtime_prov['binary_sha256']==B and runtime_prov['archive_sha256']==A,'fixture binary mismatch')
 node=assessment(original,SUP+'acceptance/node-deps/');python=assessment(original,SUP+'acceptance/python-pytest/');sample=assessment(original,SAMPLE+'acceptance/',True)
 identity=execution_identity(node);need(execution_identity(python)==identity==execution_identity(sample),'actual actor identities differ')
 need(sample['cli_sha256']==B,'sample acceptance CLI mismatch')
 sample_prov=json.loads(original[SAMPLE+'acceptance/candidate-provenance.json'])
 need(sample_prov['candidate_commit']==C and sample_prov['installed_cli_sha256']==B and sample_prov['archive_binary_sha256']==B and sample_prov['archive_sha256']==A,'sample candidate mismatch')
 sample_text=original[SAMPLE+'transcript.txt'].decode(errors='replace')
 for marker in ('no network. yes','Ready: yes','First run passed (exit 0)','Next, in your own repository'):
  need(marker in sample_text,'missing completed live onboard/sample observation: '+marker)
 sample_log=original[SAMPLE+'acceptance/onboarding.log'].decode(errors='replace')
 need('First run passed (exit 0)' in sample_log,'no independently retained sample result')
 # Compare the exact published command and the complete dependent fixture blocks.
 published=original[SUP+'supplemental-codex-fixtures-only/published-command.txt'].decode().strip()
 need(published.startswith('axocoatl run fix-codex '),'not the published Codex command')
 for i in (10,13,14,15,18,21,22,23):need(sup['steps'][i].get('exit_code')==0,'dependent CLI check not successful')
 need(sup['finished_at']<fresh['started_at'],'chronology differs: fixtures must remain earlier observations')
 files={SIGNED+n:d for n,d in original.items()}
 files.update({FRESH+n:d for n,d in fresh_files.items()})
 for n in ('docs/RELEASE_GATE.md','scripts/release-gate/newuser-mac.sh','scripts/release-gate/evidence.mjs','crates/axocoatl-config/loadouts/fix-codex.yaml'):
  files[PREFIX+'candidate-sources/'+n]=sources[n]
 files[PREFIX+'source-signature-verification.json']=enc(signature)
 files[PREFIX+'gate-keys.public']=(R/'.axocoatl/gate-keys').read_bytes()
 auth={'schema':'axocoatl.recovery.observed-authentication/1','operator':'Codex assistant through owned PTY; root and user through dedicated browser','human_operator':False,'candidate_commit':C,'new_credential_was_stored':True,'credential_name':'codex-chatgpt','fresh_absence_observed':True,'new_and_saved_account_email_equal':True,'email_claim_field':emails[0]['source_field'],'account_email_sha256':sha(restore['email'].encode()),'actual_authentication_result':'successful browser callback during live onboarding before user-directed cancellation','new_credential_preserved_privately':True,'saved_credential_restored_byte_identically':True,'saved_credential_sha256':restore['restored_bytes_sha256'],'restoration_finished_at':restore['finished_at'],'global_config_unchanged_before_OR':True,'owner_configuration_sha256':CONFIG,'fresh_runner_exit':130,'new_inference_after_callback':False,'authority':'User explicitly directed saved-token reuse; root authorized this criterion-level composite and preserving both failed attempts','private_credential_files_included':False}
 files[PREFIX+'authentication-and-restoration.json']=enc(auth)
 files[PREFIX+'effective-configuration.json']=enc({'owner_configuration_sha256':CONFIG,'execution_identity':identity,'different_fixture_environments_retained_in_records':True,'no_full_record_identity_claim':True})
 files[PREFIX+'composer.py']=pathlib.Path(__file__).read_bytes()
 fresh_ref=FRESH+'C1-codex.json';auth_ref=PREFIX+'authentication-and-restoration.json';sample_ref=SIGNED+SAMPLE+'transcript.txt';sup_ref=SIGNED+SUP+'supplemental-codex-fixtures-only.json'
 mappings=[
  {'name':REQUIRED[0],'sources':[public_ref(files,fresh_ref,2)],'detail':'Actual name-only absence immediately before today\'s fresh Codex browser sign-in.'},
  {'name':REQUIRED[1],'sources':[public_ref(files,sample_ref),public_ref(files,SIGNED+SAMPLE+'acceptance/onboarding.log'),public_ref(files,FRESH+'C1-codex/transcript.txt')],'detail':'Earlier interactive saved-token onboarding completed Ready with a passing sample; its sandbox consent was answered live. Today\'s fresh-sign-in onboarding was also answered live until user-directed cancellation before another sample. These are separate observations, not one uninterrupted command; original exit130 is preserved.'},
  {'name':REQUIRED[2],'sources':[public_ref(files,sample_ref),public_ref(files,FRESH+'C1-codex/transcript.txt')],'detail':'Both actual onboard transcripts explicitly selected ChatGPT-plan codex-chatgpt billing; no OpenAI API key supplied.'},
  {'name':REQUIRED[3],'sources':[public_ref(files,fresh_ref,2),public_ref(files,FRESH+'C1-codex/transcript.txt'),public_ref(files,auth_ref)],'detail':'Today\'s fresh absent-to-stored transition came from the browser callback within live onboarding. New and saved credentials explicitly identify the same account. User then required restoring the saved credential before inference; the restored bytes are not used to claim fresh sign-in.'},
  {'name':REQUIRED[4],'sources':[public_ref(files,sup_ref,i) for i in (4,5,6,7)],'detail':'Actual earlier exact-candidate doctor JSON and text passed under the identical owner configuration; the full dependent source steps are retained.'},
  {'name':REQUIRED[5],'sources':[public_ref(files,sup_ref,i) for i in range(10,25)]+[public_ref(files,SIGNED+SUP+'acceptance/'+f+'/assessment.json') for f in ('node-deps','python-pytest')],'detail':'Earlier actual published fix-codex command passed both complete fixture blocks, receipt/record/markup validation and independent original4/4tests once each; exact candidate and owner configuration match. No fixture rerun.'},
 ]
 result={'schema':'axocoatl.release-evidence.path/1','path':'C1-codex','tier':'C','version':'1.5.0','commit':C,'pass':True,'started_at':old['started_at'],'finished_at':restore['finished_at'],'steps':[dict(name=x['name'],status='pass',required=True,detail=x['detail'],observation_kind='operator-authorized-composition-of-verified-observations',dependencies=x['sources']) for x in mappings],'host':fresh['host'],'tools':fresh['tools'],'continuation':{'schema':'axocoatl.recovery.criterion-composition/1','unsigned':True,'operator':'Codex assistant','human_operator':False,'single_uninterrupted_run':False,'original_timeout_exit':1,'new_fresh_attempt_exit':130,'original_failures_preserved':True,'original_command_outcomes_rewritten':False,'fresh_authentication_actual':True,'saved_token_used_for_existing_accepted_inference':True,'user_directed_token_substitution':True,'new_inference':False,'new_tests':False,'chronology':'Prior saved-token fixtures and successful onboarding/sample preceded today\'s fresh browser sign-in. Today stopped before another sample at user direction and restored saved bytes. This successor combines criterion-level observations without claiming one uninterrupted invocation.','candidate_binary_sha256':B,'candidate_archive_sha256':A,'owner_configuration_sha256':CONFIG,'scope':'C1-codex outcomes only; other gates, unsigned acceptance and publication remain separate.'}}
 files[PREFIX+'step-provenance.json']=enc(mappings)
 files[PREFIX+'source-lock.json']=enc({'candidate_commit':C,'original_manifest_sha256':sha(original_manifest),'original_dsse_sha256':sha(original_dsse),'source_sha256':{n:sha(d) for n,d in sources.items()},'fresh_files_sha256':{n:sha(d) for n,d in fresh_files.items()},'original_files_preserved':len(original)})
 result['attachments']=sorted(files)
 files['C1-codex.json']=enc(result)
 out.mkdir(mode=0o700)
 for n,d in files.items():
  p=out/n;p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(d)
 code="""import{checkPathEvidence,parseRequiredSteps,requiredStepProblems,buildManifest}from MODULE;import{readFileSync,writeFileSync}from'node:fs';const d=process.argv[1],e=JSON.parse(readFileSync(d+'/C1-codex.json')),required=parseRequiredSteps(readFileSync(process.argv[2],'utf8'));let problems=[...checkPathEvidence('C1-codex',e,{version:'1.5.0',commit:process.argv[3]}),...requiredStepProblems('C1-codex',e.steps,required.get('C1-codex'))];console.log(JSON.stringify({problems,required_steps:required.get('C1-codex').length}));if(problems.length)process.exitCode=1;else writeFileSync(d+'/manifest.json',JSON.stringify(buildManifest(d,{version:'1.5.0',commit:process.argv[3]}),null,2)+'\\n');""".replace('MODULE',json.dumps(module.as_uri()))
 validation=json.loads(command([NODE,'--input-type=module','-e',code,str(out),str(R/'docs/RELEASE_GATE.md'),C]))
 for n,d in original.items():need((bundle/n).read_bytes()==d and (out/SIGNED/n).read_bytes()==d,'original source preservation failed')
 for n,d in fresh_files.items():need((fresh_root/n).read_bytes()==d,'fresh source changed')
 need(not (out/'manifest.dsse.json').exists(),'unexpected signing')
 print(json.dumps({'output':str(out),'unsigned':True,'original_files_preserved':len(original),'fresh_attempt_preserved':True,'required_criteria':validation['required_steps'],'source_signature':signature,'successor_sha256':sha(files['C1-codex.json']),'manifest_sha256':sha((out/'manifest.json').read_bytes()),'new_tests':False,'new_inference':False},indent=2))
if __name__=='__main__':
 p=argparse.ArgumentParser();p.add_argument('--out',required=True);a=p.parse_args();main(pathlib.Path(a.out))
