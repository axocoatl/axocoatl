#!/usr/bin/env python3
"""Offline continuation tests using authenticated observed data; no provider calls."""
import copy, importlib.util, json, pathlib, subprocess, unittest
SPEC=importlib.util.spec_from_file_location('composer','/private/tmp/axo-compose-claude-completion.py')
m=importlib.util.module_from_spec(SPEC);SPEC.loader.exec_module(m)
ROOT=pathlib.Path('/Users/erick/.codex/worktrees/release-1-5-recovery/axocoatl')
LOCK=json.loads(pathlib.Path('/private/tmp/axo-claude-completion-inputs.json').read_text())
BUNDLE=ROOT/'release-evidence/1.5.0'/m.C
FILES,VERIFIED=m.signed_sources(BUNDLE,ROOT/'scripts/release-gate/evidence.mjs',ROOT/'.axocoatl/gate-keys',LOCK)
SOURCES={p:subprocess.check_output(['git','-C',str(ROOT),'show',m.C+':'+p]) for p in LOCK['candidate_source_sha256']}
# Semantic mutations update the in-memory manifest entry only. They model newly
# authenticated evidence with unsuitable facts, beyond ordinary tamper detection.
def replace(files,rel,data):
 files[rel]=data;manifest=json.loads(files['manifest.json'])
 for x in manifest['files']:
  if x['path']==rel:x.update(sha256=m.sha(data),bytes=len(data));break
 files['manifest.json']=m.encoded(manifest)
def change(files,rel,fn):
 obj=json.loads(files[rel]);fn(obj);replace(files,rel,m.encoded(obj))
def change_section(files,rel,section,fn):
 rows=[json.loads(x)for x in files[rel].splitlines()]
 for row in rows:
  if row.get('section')==section:fn(row['data'])
 replace(files,rel,b''.join(json.dumps(x).encode()+b'\n' for x in rows))
class Continuation(unittest.TestCase):
 def setUp(self):self.f=copy.deepcopy(FILES);self.s=copy.deepcopy(SOURCES)
 def refuses(self,pattern):
  with self.assertRaisesRegex(m.Refusal,pattern):m.validate(self.f,self.s,LOCK)
 def test_authenticated_real_observations_form_complete_successor_and_keep_failure(self):
  before=m.sha(self.f[m.OLD]);result,mapping,config,accepted=m.compose(self.f,self.s,LOCK,VERIFIED)
  self.assertEqual(37,len(result['steps']));self.assertTrue(result['pass']);self.assertTrue(all(x['status']=='pass'for x in result['steps']))
  self.assertEqual(before,m.sha(self.f[m.OLD]));self.assertFalse(json.loads(self.f[m.OLD])['pass'])
  self.assertEqual(7,len(result['continuation']['replaced_dependent_steps']));self.assertEqual(14,len(result['steps'][25]['dependencies']))
  self.assertEqual(37,len(mapping));self.assertFalse(result['continuation']['fresh_onboarding_repeated'])
  self.assertEqual(9,result['steps'][11]['observation']['step_index']);self.assertEqual(12,result['steps'][14]['observation']['step_index'])
  self.assertEqual(18,result['steps'][18]['observation']['step_index'])
 def test_wrong_candidate_refused(self):
  change(self.f,m.NEW,lambda x:x.update(commit='0'*40));self.refuses('candidate mismatch')
 def test_loadout_change_refused(self):
  change_section(self.f,m.NEWREC,'loadout',lambda x:x.update(text=x['text']+'\n'));self.refuses('loadout source mismatch')
 def test_fixture_original_identity_mismatch_refused(self):
  change(self.f,m.ACCEPT+'assessment.json',lambda x:x['file_hashes']['test/paginate.test.js'].update(original_sha256='0'*64));self.refuses('fixture original mismatch')
 def test_reviewer_config_change_refused(self):
  change_section(self.f,m.NEWREC,'team',lambda x:x['required_review']['inline'].update(model='different-model'));self.refuses('effective execution configuration mismatch')
 def test_budget_change_refused(self):
  change_section(self.f,m.NEWREC,'team',lambda x:x['slots'][0]['limits'].update(tokens=1));self.refuses('effective execution configuration mismatch')
 def test_unknown_effective_config_field_refused(self):
  change_section(self.f,m.NEWREC,'manifest',lambda x:x['options'].update(unexpected_override=True));self.refuses('effective execution configuration mismatch')
 def test_fresh_onboarding_missing_refused(self):
  change(self.f,m.OLD,lambda x:x['steps'][2].update(name='existing credential available'));self.refuses('missing fresh onboarding')
 def test_onboarding_interrupt_cannot_be_pass(self):
  change(self.f,m.OLD,lambda x:x['steps'][3].update(exit_code=130));self.refuses('fresh onboarding did not complete')
 def test_browser_auth_not_observed_refused(self):
  change(self.f,'C1-claude-code/operator.json',lambda x:x['claude_authentication'].update(operator_observed=False));self.refuses('missing observed browser')
 def test_altered_signed_source_refused(self):
  self.f[m.NEW]+=b' ';self.refuses('altered source hash')
 def test_altered_candidate_source_refused(self):
  self.s['scripts/release-gate/newuser-mac.sh']+=b' ';self.refuses('altered candidate source hash')
 def test_missing_receipt_dependent_step_refused(self):
  change(self.f,m.NEW,lambda x:x['steps'][12].update(name='receipt not checked'));self.refuses('whole dependent block identity mismatch')
 def test_failed_receipt_check_refused_even_if_status_says_pass(self):
  change(self.f,m.NEW,lambda x:x['steps'][12].update(exit_code=6));self.refuses('missing dependent receipt/run check')
 def test_independent_receipt_failure_refused(self):
  change(self.f,m.ACCEPT+'assessment.json',lambda x:x['verification'].update(receipt_exit=6));self.refuses('missing dependent receipt/record/markup check')
 def test_continued_record_interrupted_refused(self):
  change_section(self.f,m.NEWREC,'outcome',lambda x:x.update(verdict='interrupted'));self.refuses('continued outcome did not pass')
 def test_unrelated_original_failure_cannot_be_erased(self):
  change(self.f,m.OLD,lambda x:x['steps'][29].update(status='fail'));self.refuses('original failure history changed')
 def test_deadline_extension_refused(self):
  change_section(self.f,m.NEWREC,'team',lambda x:x['slots'][0].update(expires_at_ms=1999999999999));self.refuses('writer time allowance changed')
 def test_missing_acceptance_dependency_refused(self):
  del self.f[m.ACCEPT+'receipt.json'];self.refuses('missing referenced source')
 def test_changed_archive_identity_refused(self):
  change(self.f,m.RAW+'provenance.json',lambda x:x.update(archive_sha256='0'*64));self.refuses('candidate binary or archive mismatch')
if __name__=='__main__':unittest.main(verbosity=2)
