#!/usr/bin/env node
// Validates original capture, real task/receipt evidence, and every encoded frame.
import assert from 'node:assert/strict';
import {createHash,createPublicKey,verify} from 'node:crypto';
import {readFileSync,lstatSync,mkdtempSync,writeFileSync,rmSync} from 'node:fs';
import {resolve,dirname,join} from 'node:path';
import {fileURLToPath,pathToFileURL} from 'node:url';
import {tmpdir} from 'node:os';
import {gunzipSync} from 'node:zlib';
import {execFileSync} from 'node:child_process';

const VERSION='1.5.3', COMMIT='c14e3957cd0a8cfd9e76480f724e902ada0f5aa4';
const BINARY='feeb6eb174c7eefd1cea83c8a008bb658944ed8202c5c3e379c4b7dc49676919';
const hash=b=>createHash('sha256').update(b).digest('hex');
const near=(a,b)=>assert(Math.abs(a-b)<0.00001,`Timing mismatch ${a} vs ${b}`);
const safe=o=>assert(!/sk-or-v1-|Bearer\s|[?&#](?:token|code|key)=/i.test(JSON.stringify(o)),'Authentication material in public evidence');
const digest=h=>assert(/^[a-f0-9]{64}$/.test(h),'Invalid SHA256');
function imageSize(b){
 if(b.subarray(0,8).equals(Buffer.from([137,80,78,71,13,10,26,10])))return[b.readUInt32BE(16),b.readUInt32BE(20)];
 assert.equal(b.readUInt16BE(0),0xffd8);let i=2;
 while(i<b.length){assert.equal(b[i++],255);while(b[i]===255)i++;const marker=b[i++];if([217,218].includes(marker))break;if(marker===1||(marker>=208&&marker<=215))continue;const size=b.readUInt16BE(i);assert(size>=2&&i+size<=b.length);if([192,193,194].includes(marker))return[b.readUInt16BE(i+5),b.readUInt16BE(i+3)];i+=size;}
 throw Error('Image dimensions absent');
}
export function verifyArtifactEvidence(e,p){
 assert.equal(e.schema,'axocoatl.dashboard-flow-artifact-evidence/1');safe(e);safe(p);
 assert.equal(p.product.version,VERSION);assert.equal(p.product.release_commit,COMMIT);assert.equal(p.product.binary_sha256,BINARY);
 assert.equal(p.capture.theme,'dark');assert.equal(p.capture.human_operator,false);
 assert.equal(p.presentation.new_inference,true);assert.equal(p.presentation.run_started_from,'dashboard');
 const file=f=>{assert(typeof f.path==='string'&&!f.path.startsWith('/')&&!f.path.includes('\\')&&!f.path.split('/').some(s=>['..','.git','node_modules','.axocoatl'].includes(s)));assert.equal(typeof f.content,'string');const b=Buffer.from(f.content);digest(f.sha256);assert.equal(hash(b),f.sha256);assert.equal(b.length,f.bytes);return(({content,...rest})=>rest)(f);};
 assert(Array.isArray(e.baseline_files)&&e.baseline_files.length);assert.equal(new Set(e.baseline_files.map(f=>f.path)).size,e.baseline_files.length);e.baseline_files.forEach(file);
 assert(Array.isArray(e.cases)&&e.cases.length);assert.equal(new Set(e.cases.map(c=>c.name)).size,e.cases.length);
 for(const c of e.cases){assert(p.product.turn_ids.includes(c.turn_id),'Case turn is not in recorded product provenance');assert(['ACCEPTED','FAILED','INTERRUPTED','PARTIAL'].includes(c.assessment.verdict));assert.equal(hash(Buffer.from(c.patch)),c.assessment.patch_sha256);assert(c.files.length);assert.equal(new Set(c.files.map(f=>f.path)).size,c.files.length);assert.deepEqual(c.assessment.files,c.files.map(file));}
 const final=e.cases.find(c=>c.name===e.final_case);assert(final,'Final case missing');
 assert(['accepted','failed','interrupted','partial'].includes(p.outcome.status));assert(!p.outcome.do_not_publish);
 const o=e.runtime_observation;assert(o&&typeof o.content==='string');digest(o.sha256);digest(o.source_response_sha256);const b=Buffer.from(o.content);assert.equal(hash(b),o.sha256);assert.equal(b.length,o.bytes);const runtime=JSON.parse(b);safe(runtime);
 assert.equal(runtime.session_id,p.product.session_id);assert(p.product.turn_ids.includes(runtime.turn_id));assert.equal(final.turn_id,runtime.turn_id);
 assert(runtime.required_review&&Array.isArray(runtime.required_checks));
 if(p.outcome.status==='accepted'){assert.equal(final.assessment.verdict,'ACCEPTED');assert.equal(runtime.required_review.state,'approved');assert.equal(runtime.required_review.verdict,'approved');assert(runtime.required_checks.length);for(const c of runtime.required_checks){assert.equal(c.state,'passed');assert.equal(c.exit_code,0);assert(Array.isArray(c.argv)&&c.argv.length&&c.argv.every(x=>typeof x==='string'));}}
 return runtime;
}
function stamp(value){let ms=Math.round(value),s=Math.floor(ms/1000);ms%=1000;let m=Math.floor(s/60);s%=60;const h=Math.floor(m/60);m%=60;return`${String(h).padStart(2,'0')}:${String(m).padStart(2,'0')}:${String(s).padStart(2,'0')}.${String(ms).padStart(3,'0')}`;}
function vtt(cues,end,key){const lines=['WEBVTT',''];for(const[i,c]of cues.entries()){lines.push(`${stamp(c.playback_ms)} --> ${stamp(cues[i+1]?.playback_ms??end)}`,c[key],'');}return lines.join('\n');}


function verifyReceiptEnvelope(receipt){
 safe(receipt);assert.equal(receipt.payloadType,'application/vnd.in-toto+json');assert.equal(receipt.signatures?.length,1);
 const payload=Buffer.from(receipt.payload,'base64'),statement=JSON.parse(payload);safe(statement);assert.equal(statement.predicate.run.axocoatl_version,VERSION);
 const signer=statement.predicate.signer,key=Buffer.from(signer.public_key,'base64');assert.equal(signer.algorithm,'ed25519');assert.equal(key.length,32);assert.equal(signer.key_id,`sha256:${hash(key)}`);assert.equal(receipt.signatures[0].keyid,signer.key_id);
 const pae=Buffer.concat([Buffer.from(`DSSEv1 ${Buffer.byteLength(receipt.payloadType)} ${receipt.payloadType} ${payload.length} `),payload]);const publicKey=createPublicKey({key:Buffer.concat([Buffer.from('302a300506032b6570032100','hex'),key]),format:'der',type:'spki'});assert(verify(null,pae,publicKey,Buffer.from(receipt.signatures[0].sig,'base64')),'Receipt signature is invalid');return statement;
}
function bindReceipt(statement,artifact){
 assert.equal(statement.predicate.native_turn.turn_id,artifact.turn_id);const files=new Map(artifact.files.map(f=>[f.path,f]));for(const subject of statement.subject){assert(files.has(subject.name),`Receipt path absent from preserved artifact: ${subject.name}`);assert.equal(files.get(subject.name).sha256,subject.digest.sha256);}for(const path of statement.predicate.files.removed??[])assert(!files.has(path));
}

async function main(){
 let directory=dirname(fileURLToPath(import.meta.url)),probe=false,repo=null,input=null,facts=null;
 for(let i=2;i<process.argv.length;i++){const a=process.argv[i];if(a==='--dir')directory=resolve(process.argv[++i]);else if(a==='--probe')probe=true;else if(a==='--repo')repo=resolve(process.argv[++i]);else if(a==='--check-inputs')input=resolve(process.argv[++i]);else if(a==='--facts')facts=resolve(process.argv[++i]);else throw Error('Usage: node verify-media.mjs [--dir ASSETS] [--repo REPO] [--probe]');}
 if(input||facts){assert(input&&facts);verifyArtifactEvidence(JSON.parse(readFileSync(input)),JSON.parse(readFileSync(facts)));console.log('Actual task and host evidence verified.');return;}
 const read=name=>{assert(/^[A-Za-z0-9][A-Za-z0-9.-]*$/.test(name));const p=resolve(directory,name),s=lstatSync(p);assert(s.isFile()&&!s.isSymbolicLink());assert(s.size<25*1024*1024,'Pages per-file limit exceeded');return readFileSync(p);};
 const p=JSON.parse(read('provenance.json'));assert.equal(p.schema,'axocoatl.dashboard-flow-provenance/1');safe(p);
 assert.equal(new Set(p.files.map(f=>f.file)).size,p.files.length);
 for(const f of p.files){const b=read(f.file);assert.equal(hash(b),f.sha256,f.file);assert.equal(b.length,f.bytes,f.file);}
 const e=JSON.parse(read('artifact-evidence.json')),runtime=verifyArtifactEvidence(e,p);
 const receipt=JSON.parse(read('receipt.dsse.json')),statement=verifyReceiptEnvelope(receipt),signer=statement.predicate.signer;
 const verification=JSON.parse(read('receipt-verification.json'));assert.equal(verification.result,'valid');assert.equal(verification.signature,'trusted');assert.equal(verification.exit_code,0);assert.equal(verification.run_id,runtime.turn_id);assert.equal(verification.key_id,signer.key_id);assert(!verification.files?.repo,'Local checkout path must not be public');
 assert.equal(verification.statement_sha256,`sha256:${hash(Buffer.from(receipt.payload,'base64'))}`);assert.equal(statement.predicate.native_turn.turn_id,runtime.turn_id);assert.equal(statement.predicate.run.axocoatl_version,VERSION);
 assert.equal(e.runtime_observation.source_kind,'signed_receipt_projection');assert.equal(e.runtime_observation.source_response_sha256,hash(read('receipt.dsse.json')));assert.deepEqual(runtime.required_checks,statement.predicate.checks);assert.deepEqual(runtime.required_review,{state:statement.predicate.review.passed?'approved':'failed',...statement.predicate.review});assert.deepEqual(p.effective_configuration.agents,statement.predicate.agents);assert.deepEqual(p.effective_configuration.review,statement.predicate.review);
 bindReceipt(statement,e.cases.find(c=>c.name===e.final_case));
 const priorTurns=new Set();for(const filename of p.prior_receipts){assert(/^prior-receipt-\d{2}\.dsse\.json$/.test(filename));const prior=verifyReceiptEnvelope(JSON.parse(read(filename)));assert.equal(prior.predicate.signer.key_id,signer.key_id,'Prior receipt uses a different signing key');assert.equal(prior.predicate.run.session_id,p.product.session_id);const turn=prior.predicate.native_turn.turn_id;assert(p.product.turn_ids.includes(turn));assert(turn!==runtime.turn_id&&!priorTurns.has(turn));priorTurns.add(turn);const before=e.cases.find(c=>c.turn_id===turn);assert(before,'Prior receipt has no preserved artifact case');bindReceipt(prior,before);if(before.assessment.verdict!=='ACCEPTED')assert.notEqual(prior.predicate.review?.passed,true,'Prior failure receipt unexpectedly claims review passed');}
 if(p.outcome.status==='accepted'){assert.equal(verification.verdict,'pass');assert.equal(verification.files.complete,true);assert.deepEqual(verification.files.mismatches,[]);}
 const required=['integration.json','timeline.json','artifact-evidence.json','receipt.dsse.json','receipt-verification.json','assemble-media.py','verify-media.mjs','ASSEMBLY.md',...p.prior_receipts,...p.source_archives,...Object.values(p.clips).flatMap(c=>[c.video,c.poster,c.captions,c.chapterTrack])];assert.deepEqual(p.files.map(f=>f.file).sort(),required.sort());
 const t=JSON.parse(read('timeline.json'));safe(t);assert.equal(t.schema,'axocoatl.dashboard-flow-timeline/1');assert.equal(t.fps,24);assert.deepEqual(t.encoded_dimensions,p.encoding.dimensions);assert.deepEqual(t.dimensions,p.encoding.source_dimensions);assert.deepEqual(t.encoded_dimensions,t.dimensions.map(n=>n+n%2));assert.equal(hash(read('assemble-media.py')),p.encoding.encoder_sha256);
 const members=new Map();assert(p.source_archives.length);
 for(const name of p.source_archives){assert(/^source-\d{3}\.tar\.gz$/.test(name));const b=gunzipSync(read(name),{maxOutputLength:256*1024*1024});for(let i=0;i+512<=b.length;){const h=b.subarray(i,i+512);if(h.every(x=>x===0))break;const path=h.subarray(0,100).toString().replace(/\0.*$/s,'');assert(/^(?:frame-\d{4,8}\.(?:jpg|png)|capture\.json)$/.test(path));assert(!members.has(path));assert(h[156]===0||h[156]===48);const n=parseInt(h.subarray(124,136).toString().replace(/\0.*$/s,'').trim(),8);assert(Number.isSafeInteger(n)&&n>0&&n<16*1024*1024&&i+512+n<=b.length);members.set(path,b.subarray(i+512,i+512+n));i+=512+Math.ceil(n/512)*512;}}
 const captureBytes=members.get('capture.json');assert(captureBytes);assert.equal(hash(captureBytes),t.source_capture_sha256);const capture=JSON.parse(captureBytes);safe(capture);assert.equal(capture.theme,'dark');assert.equal(capture.original_capture_sha256,t.original_capture_sha256);assert.equal(capture.duration_ms,t.actual_duration_ms);assert.equal(p.actual_duration_ms,t.actual_duration_ms);assert(Math.abs(Date.parse(capture.finished_at)-Date.parse(capture.started_at)-capture.duration_ms)<=10);
 assert.deepEqual(capture.segments,t.recording_segments);assert.deepEqual(capture.edit_plan,t.edit_plan);assert.deepEqual(capture.uncaptured_gaps,t.uncaptured_gaps);assert.equal(members.size,t.frames.length+1);assert.equal(p.source_frames,t.frames.length);
 const all=[...t.frames,...t.omitted_frames].sort((a,b)=>a.source_ms-b.source_ms);assert.equal(all.length,capture.original_frame_count);assert.equal(new Set(all.map(f=>f.file)).size,all.length);
 const endpoints=new Set([0,t.actual_duration_ms,...all.map(f=>f.source_ms),...t.recording_segments.flatMap(s=>[s.start_ms,s.end_ms])]);let last=-1;
 for(const edit of t.edit_plan){assert(['speed','hold','omit','gap'].includes(edit.kind));assert(endpoints.has(edit.start_ms)&&endpoints.has(edit.end_ms));assert(edit.start_ms>=last&&edit.start_ms<edit.end_ms);last=edit.end_ms;assert(typeof edit.reason==='string'&&edit.reason.length>5);if(edit.kind==='speed')assert(edit.rate>1&&edit.rate<=24);if(edit.kind==='hold')assert(edit.rate>0&&edit.rate<1);if(edit.kind==='gap')assert(t.uncaptured_gaps.some(g=>g.start_ms<=edit.start_ms&&edit.end_ms<=g.end_ms));}
 const mapped=ms=>ms-t.edit_plan.reduce((sum,s)=>sum+Math.max(0,Math.min(ms,s.end_ms)-s.start_ms)*(['speed','hold'].includes(s.kind)?1-1/s.rate:1),0);
 const omitted=ms=>t.edit_plan.some(e=>['omit','gap'].includes(e.kind)&&e.start_ms<=ms&&ms<e.end_ms);
 last=-1;let previousEnd=0;const gaps=[];
 for(const [i,f]of all.entries()){assert(f.source_ms>last&&f.source_ms<t.actual_duration_ms);last=f.source_ms;const owner=t.recording_segments.filter(s=>s.start_ms<=f.source_ms&&f.source_ms<s.end_ms);assert.equal(owner.length,1);assert.equal(f.source_end_ms,Math.min(all[i+1]?.source_ms??t.actual_duration_ms,owner[0].end_ms));if(f.source_ms>previousEnd)gaps.push({start_ms:previousEnd,end_ms:f.source_ms});previousEnd=f.source_end_ms;}
 if(previousEnd<t.actual_duration_ms)gaps.push({start_ms:previousEnd,end_ms:t.actual_duration_ms});assert.deepEqual(gaps,t.uncaptured_gaps);
 for(const [i,f]of t.frames.entries()){assert(!omitted(f.source_ms));assert.equal(f.file,capture.frames[i].file);assert.equal(f.source_ms,capture.frames[i].elapsed_ms);const b=members.get(f.file);assert(b);assert.equal(hash(b),f.sha256);assert.equal(b.length,f.bytes);assert.deepEqual(imageSize(b),t.dimensions);near(f.playback_ms,mapped(f.source_ms));near(f.playback_end_ms,mapped(f.source_end_ms));}
 for(const f of t.omitted_frames){assert(omitted(f.source_ms));assert(!members.has(f.file));digest(f.sha256);}
 const integration=JSON.parse(read('integration.json'));assert.equal(integration.schema,'axocoatl.dashboard-flow-integration/1');assert.equal(integration.version,VERSION);assert.deepEqual(integration.dimensions,t.encoded_dimensions);assert.deepEqual(integration.source_dimensions,t.dimensions);assert.deepEqual(integration.clips,p.clips);assert.deepEqual(Object.keys(p.clips).sort(),['dashboard-flow','receipt','review']);
 assert.equal(t.clips['dashboard-flow'].start_ms,t.frames[0].source_ms);assert.equal(t.clips['dashboard-flow'].end_ms,t.frames.at(-1).source_end_ms);
 for(const [name,clip]of Object.entries(t.clips)){
  const info=p.clips[name],origin=mapped(clip.start_ms);near(clip.duration_ms,mapped(clip.end_ms)-origin);near(info.duration_seconds,clip.duration_ms/1000);assert.deepEqual(info.chapters,clip.chapters.map(c=>({time_seconds:c.playback_ms/1000,label:c.title})));
  const expected=t.frames.filter(f=>clip.start_ms<=f.source_ms&&f.source_ms<clip.end_ms);assert.deepEqual(clip.frames.map(f=>f.file),expected.map(f=>f.file));
  for(const [i,f]of clip.frames.entries()){assert.equal(f.sha256,expected[i].sha256);near(f.playback_ms,mapped(f.source_ms)-origin);near(f.playback_end_ms,mapped(f.source_end_ms)-origin);assert.equal(f.output_start_frame,Math.round(f.playback_ms*24/1000));assert.equal(f.output_end_frame,Math.round(f.playback_end_ms*24/1000));assert(f.output_end_frame>f.output_start_frame);}
  assert.equal(hash(read(info.poster)),hash(members.get(clip.poster_source)));assert(clip.frames.some(f=>f.file===clip.poster_source));
  for(const [key,text]of [['chapters','title'],['captions','text']]){assert.equal(clip[key][0].source_ms,clip.start_ms);last=-1;for(const c of clip[key]){assert(c.source_ms>last&&clip.start_ms<=c.source_ms&&c.source_ms<clip.end_ms&&!omitted(c.source_ms));last=c.source_ms;near(c.playback_ms,mapped(c.source_ms)-origin);assert(c[text].trim()&&!c[text].includes('\n')&&!c[text].includes('-->'));}assert.equal(read(key==='chapters'?info.chapterTrack:info.captions).toString(),vtt(clip[key],clip.duration_ms,text));}
 }
 if(repo)assert.equal(execFileSync('git',['rev-parse',`${COMMIT}^{commit}`],{cwd:repo,encoding:'utf8'}).trim(),COMMIT);
 if(probe){
  const work=mkdtempSync(join(tmpdir(),'axo-flow-verify-'));
  try{
   for(const [name,b]of members)if(name!=='capture.json')writeFileSync(join(work,name),b);
   for(const [name,clip]of Object.entries(t.clips)){
    const video=resolve(directory,p.clips[name].video),m=JSON.parse(execFileSync('ffprobe',['-v','error','-show_streams','-show_format','-of','json',video],{encoding:'utf8'}));assert.equal(m.streams.length,1);const stream=m.streams[0];assert.equal(stream.codec_name,'h264');assert.equal(stream.pix_fmt,'yuv420p');assert.equal(stream.r_frame_rate,'24/1');assert.deepEqual([stream.width,stream.height],t.encoded_dimensions);assert(Math.abs(Number(m.format.duration)-clip.duration_ms/1000)<=1/24+.001);
    const lines=['ffconcat version 1.0'];for(const f of clip.frames)lines.push(`file '${join(work,f.file)}'`,'option framerate 24',`duration ${((f.output_end_frame-f.output_start_frame)/24).toFixed(9)}`);lines.push(`file '${join(work,clip.frames.at(-1).file)}'`,'option framerate 24');
    const concat=join(work,`${name}.ffconcat`),stats=join(work,`${name}.ssim`);writeFileSync(concat,lines.join('\n')+'\n');
    execFileSync('ffmpeg',['-hide_banner','-nostdin','-i',video,'-f','concat','-safe','0','-i',concat,'-filter_complex',`[1:v]fps=24,pad=ceil(iw/2)*2:ceil(ih/2)*2:0:0:color=black,scale=in_range=pc:out_range=tv,format=yuv420p[ref];[0:v][ref]ssim=stats_file=${stats}:shortest=1`,'-t',String(clip.duration_ms/1000),'-an','-f','null','-'],{stdio:['ignore','ignore','pipe'],maxBuffer:4*1024*1024});
    const scores=[...readFileSync(stats,'utf8').matchAll(/All:([0-9.]+)/g)].map(x=>Number(x[1]));assert(scores.length>=Number(stream.nb_frames)-1,`Missing decoded frames in ${name}`);assert(Math.min(...scores)>=.99,`${name}: original-frame SSIM below 0.99: ${Math.min(...scores)}`);console.log(`${name}: ${scores.length} encoded frames match actual source samples at SSIM >= ${Math.min(...scores).toFixed(6)}.`);
   }
  }finally{rmSync(work,{recursive:true,force:true});}
 }
 console.log(`Verified 1.5.3 task/receipt evidence, ${t.frames.length} original source samples, chronological edits, three clips, posters, captions, chapters, all archive and media hashes${probe?', and every decoded frame':''}.`);
}
if(process.argv[1]&&import.meta.url===pathToFileURL(resolve(process.argv[1])).href)await main();
