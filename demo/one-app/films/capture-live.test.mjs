import assert from 'node:assert/strict';
import test from 'node:test';
import {validateCapturePlan} from './capture-live.mjs';
import {loadPortfolio} from './film-lib.mjs';
const portfolio=loadPortfolio(),film=portfolio.films.find(f=>f.slug==='workspace-knowledge');
const plan=()=>({film:film.slug,base_url:'http://localhost:8080',theme:'dark',shots:film.beats.map(beat=>({beat:beat.id,hold_frames:64,steps:[{action:'wait_visible',role:'button',name:'Knowledge'}],evidence:[{name:'notes',path:'/api/sessions/observed-session/knowledge'}]}))});
test('capture plan validates exact ordered beats and same-origin real API evidence',()=>{assert.equal(validateCapturePlan(plan()).timeline.shots.length,5);for(const mutate of [p=>p.shots.reverse(),p=>p.shots.pop(),p=>p.shots[0].hold_frames=1,p=>p.base_url='https://example.com',p=>p.shots[0].evidence[0].path='//example.com/api/secret',p=>p.start_path='//example.com/',p=>p.shots[0].steps=[{action:'evaluate',script:'mock API'}],p=>p.shots[0].steps=[{action:'wait',ms:60001}],p=>p.shots[0].evidence[0].name='../overwrite']){const p=plan();mutate(p);assert.throws(()=>validateCapturePlan(p));}});
