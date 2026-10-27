#!/usr/bin/env node
// Read-only validation of actual recorded dashboard-task media; no network/product calls.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,lstatSync} from 'node:fs';
import {resolve,dirname} from 'node:path';
import {fileURLToPath} from 'node:url';
import {gunzipSync} from 'node:zlib';
import {execFileSync,spawnSync} from 'node:child_process';
let directory=dirname(fileURLToPath(import.meta.url)),probe=false,repository=null;
for(let i=2;i<process.argv.length;i++){
 if(process.argv[i]==='--dir'&&process.argv[i+1])directory=resolve(process.argv[++i]);
 else if(process.argv[i]==='--repo'&&process.argv[i+1])repository=resolve(process.argv[++i]);
 else if(process.argv[i]==='--probe')probe=true;
 else throw Error('Usage: node verify-task.mjs [--dir ASSETS] [--repo REPO] [--probe]');
}
const close=(a,b)=>assert(Math.abs(a-b)<0.000001,`Timeline numerical mismatch: ${a} vs ${b}`);
const hash=b=>createHash('sha256').update(b).digest('hex');
const read=name=>{assert(/^[a-z0-9][a-z0-9.-]*$/.test(name));const p=resolve(directory,name),s=lstatSync(p);assert(s.isFile()&&!s.isSymbolicLink());return readFileSync(p);};
const p=JSON.parse(read('provenance.json'));
assert.equal(p.schema,'axocoatl.dashboard-task-provenance/1');
assert.equal(p.product.release_commit,'19e3c89ca7b4d02cd742bccd671f7274e68b95d7');
assert.equal(p.product.runtime_commit,'b222302f32639c9211255ab4f818c3262bce856a');
assert.equal(p.product.binary_sha256,'d25cd64f70da4d17eb781d223aeab553f4180d074dbb29af302b23492ce31d1b');
assert.equal(p.capture.human_operator,false);
assert.equal(p.presentation.new_inference,true);assert.equal(p.presentation.run_started_from,'dashboard');
assert(!/sk-or-v1-|Bearer\s|[?&](?:token|code|key)=/i.test(JSON.stringify(p)),'authentication material in public provenance');
const required=['dashboard-task.mp4','result.jpg','chapters.vtt','source-capture.tar.gz','timeline.json','artifact-evidence.json','captions.vtt'];
assert.deepEqual(p.files.map(x=>x.file),required);
for(const entry of p.files){const b=read(entry.file);assert.equal(hash(b),entry.sha256,entry.file);assert.equal(b.length,entry.bytes,entry.file);}
const evidence=JSON.parse(read('artifact-evidence.json'));assert.equal(evidence.schema,'axocoatl.dashboard-task-public-artifact-evidence/1');assert.deepEqual(evidence.cases.map(c=>c.assessment.verdict),['FAILED','ACCEPTED']);
for(const c of evidence.cases){assert.equal(hash(Buffer.from(c.patch)),c.assessment.patch_sha256);for(const f of c.files){const b=Buffer.from(f.content);assert.equal(hash(b),f.sha256);assert.equal(b.length,f.bytes);assert(!f.path.includes('node_modules'));}assert.deepEqual(c.assessment.files,c.files.map(({content,...entry})=>entry));}
for(const f of evidence.baseline_files){const b=Buffer.from(f.content);assert.equal(hash(b),f.sha256);assert.equal(b.length,f.bytes);}
assert.equal(evidence.cases[0].assessment.criteria.original_tests_preserved.passed,false);assert.equal(evidence.cases[1].assessment.criteria.original_tests_preserved.passed,true);assert.equal(p.outcome.status,'accepted');
function jpegSize(b){assert.equal(b.readUInt16BE(0),0xffd8);let i=2;while(i<b.length){assert.equal(b[i++],255);while(b[i]===255)i++;const marker=b[i++];if([217,218].includes(marker))break;if(marker===1||(marker>=208&&marker<=215))continue;const n=b.readUInt16BE(i);assert(n>=2&&i+n<=b.length);if([192,193,194].includes(marker))return[b.readUInt16BE(i+5),b.readUInt16BE(i+3)];i+=n;}throw Error('JPEG dimensions absent');}
const bytes=gunzipSync(read('source-capture.tar.gz'),{maxOutputLength:512*1024*1024}),members=new Map();
for(let i=0;i+512<=bytes.length;){const h=bytes.subarray(i,i+512);if(h.every(v=>!v))break;const name=h.subarray(0,100).toString().replace(/\0.*$/s,'');assert(/^(?:frame-\d{4,8}\.jpg|capture\.json)$/.test(name));assert(!members.has(name));assert(h[156]===0||h[156]===48);const n=parseInt(h.subarray(124,136).toString().replace(/\0.*$/s,'').trim(),8);assert(Number.isSafeInteger(n)&&n>0&&n<16*1024*1024&&i+512+n<=bytes.length);members.set(name,bytes.subarray(i+512,i+512+n));i+=512+Math.ceil(n/512)*512;}
const timeline=JSON.parse(read('timeline.json')),captureBytes=members.get('capture.json');assert(captureBytes);assert.equal(hash(captureBytes),timeline.source_capture_sha256);const capture=JSON.parse(captureBytes);
assert.equal(timeline.schema,'axocoatl.dashboard-task-timeline/1');assert.equal(capture.duration_ms,timeline.actual_duration_ms);assert.equal(p.actual_duration_ms,timeline.actual_duration_ms);assert.equal(p.playback_duration_ms,timeline.playback_duration_ms);assert(Math.abs(Date.parse(capture.finished_at)-Date.parse(capture.started_at)-capture.duration_ms)<=2);
assert.deepEqual(timeline.capture_errors,capture.errors);assert.equal(p.source_frames,timeline.frames.length);assert.equal(members.size,timeline.frames.length+1);assert.equal(capture.frames.length,timeline.frames.length);
const allFrames=[...timeline.frames,...timeline.omitted_frames].sort((a,b)=>a.source_ms-b.source_ms);assert.equal(allFrames.length,timeline.original_frame_count);assert.equal(capture.original_frame_count,timeline.original_frame_count);assert.equal(capture.original_capture_sha256,timeline.private_original_capture_sha256);assert.deepEqual(capture.edit_plan,timeline.edit_plan);assert.equal(new Set(allFrames.map(f=>f.file)).size,allFrames.length);
assert.deepEqual(capture.segments,timeline.recording_segments);assert.deepEqual(capture.uncaptured_gaps,timeline.uncaptured_gaps);
const endpoints=new Set([0,...allFrames.map(f=>f.source_ms),...timeline.recording_segments.flatMap(s=>[s.start_ms,s.end_ms]),timeline.actual_duration_ms]);let previous=-1;
for(const s of timeline.edit_plan.segments){assert(endpoints.has(s.start_ms)&&endpoints.has(s.end_ms));assert(previous<=s.start_ms&&s.start_ms<s.end_ms);assert(['omit','speed','gap'].includes(s.kind??'speed'));if(s.kind==='omit')assert(s.start_ms>0&&s.end_ms<timeline.actual_duration_ms);else if(s.kind==='gap')assert(timeline.uncaptured_gaps.some(g=>g.start_ms<=s.start_ms&&s.end_ms<=g.end_ms));else assert(Number.isFinite(s.rate)&&s.rate>1&&s.rate<=24);assert(typeof s.reason==='string'&&s.reason.length>5);previous=s.end_ms;}
const mapped=ms=>ms-timeline.edit_plan.segments.reduce((sum,s)=>sum+Math.max(0,Math.min(ms,s.end_ms)-s.start_ms)*(['omit','gap'].includes(s.kind)?1:1-1/s.rate),0);
close(mapped(timeline.actual_duration_ms),timeline.playback_duration_ms);previous=-1;
for(const [i,f]of timeline.frames.entries()){assert.equal(f.file,capture.frames[i].file);assert.equal(f.source_ms,capture.frames[i].elapsed_ms);assert(f.source_ms>previous&&f.source_ms<timeline.actual_duration_ms);if(i===0)assert.equal(mapped(f.source_ms),0);previous=f.source_ms;const b=members.get(f.file);assert(b);assert.equal(hash(b),f.sha256,f.file);assert.equal(b.length,f.bytes);assert.deepEqual(jpegSize(b),timeline.dimensions);const sourceIndex=allFrames.findIndex(x=>x.file===f.file);const segment=timeline.recording_segments.filter(s=>s.start_ms<=f.source_ms&&f.source_ms<s.end_ms);assert.equal(segment.length,1);assert.equal(f.source_end_ms,Math.min(allFrames[sourceIndex+1]?.source_ms??timeline.actual_duration_ms,segment[0].end_ms));assert(!timeline.edit_plan.segments.some(s=>s.kind==='omit'&&s.start_ms<=f.source_ms&&f.source_ms<s.end_ms));close(f.playback_ms,mapped(f.source_ms));close(f.playback_end_ms,mapped(f.source_end_ms));assert(f.playback_end_ms-f.playback_ms>=1000/24);assert.equal(f.output_start_frame,Math.round(f.playback_ms*24/1000));assert.equal(f.output_end_frame,Math.round(f.playback_end_ms*24/1000));assert(f.output_end_frame>f.output_start_frame);}
assert.deepEqual(jpegSize(read('result.jpg')),timeline.dimensions);
for(const f of timeline.omitted_frames){assert(!members.has(f.file),'Private frame leaked into public archive');assert(timeline.edit_plan.segments.some(s=>s.kind==='omit'&&s.start_ms<=f.source_ms&&f.source_ms<s.end_ms));assert(/^[a-f0-9]{64}$/.test(f.sha256));}
const gaps=allFrames.flatMap((f,i)=>i&&f.source_ms-allFrames[i-1].source_ms>1500?[{after:allFrames[i-1].file,milliseconds:f.source_ms-allFrames[i-1].source_ms}]:[]);assert.deepEqual(timeline.capture_gaps,gaps);
assert.deepEqual(timeline.actions.map(({playback_ms,...a})=>a),capture.actions);for(const a of timeline.actions)close(a.playback_ms,mapped(a.elapsed_ms));for(const a of timeline.actions){assert.equal(a.visually_captured,!timeline.uncaptured_gaps.some(g=>g.start_ms<=a.elapsed_ms&&a.elapsed_ms<g.end_ms));assert.equal(a.shown_in_film,!timeline.edit_plan.segments.some(s=>['omit','gap'].includes(s.kind)&&s.start_ms<=a.elapsed_ms&&a.elapsed_ms<s.end_ms));}
const actualGaps=[];let previousEnd=0;for(const f of allFrames){if(f.source_ms>previousEnd)actualGaps.push({start_ms:previousEnd,end_ms:f.source_ms});previousEnd=f.source_end_ms;}if(previousEnd<timeline.actual_duration_ms)actualGaps.push({start_ms:previousEnd,end_ms:timeline.actual_duration_ms});assert.deepEqual(timeline.uncaptured_gaps,actualGaps);
const vttTime=value=>{let ms=Math.round(value),s=Math.floor(ms/1000);ms%=1000;let m=Math.floor(s/60);s%=60;const h=Math.floor(m/60);m%=60;return`${String(h).padStart(2,'0')}:${String(m).padStart(2,'0')}:${String(s).padStart(2,'0')}.${String(ms).padStart(3,'0')}`;};
const vtt=['WEBVTT',''];previous=-1;
for(const[i,ch]of timeline.chapters.entries()){assert(ch.source_ms>previous&&ch.source_ms<timeline.actual_duration_ms);if(i===0)assert.equal(ch.source_ms,timeline.frames[0].source_ms);close(ch.playback_ms,mapped(ch.source_ms));assert(!ch.title.includes('\n'));previous=ch.source_ms;vtt.push(`${vttTime(ch.playback_ms)} --> ${vttTime(timeline.chapters[i+1]?.playback_ms??timeline.playback_duration_ms)}`,ch.title,'');}
assert.equal(read('chapters.vtt').toString(),vtt.join('\n'));
const captionLines=['WEBVTT',''];for(const [i,c]of p.captions.entries()){close(c.playback_start_ms,mapped(c.source_ms));assert.equal(c.playback_end_ms,p.captions[i+1]?.playback_start_ms??timeline.playback_duration_ms);assert(c.playback_end_ms>c.playback_start_ms);captionLines.push(`${vttTime(c.playback_start_ms)} --> ${vttTime(c.playback_end_ms)}`,c.text,'');}assert.equal(read('captions.vtt').toString(),captionLines.join('\n'));
if(repository){const actual=execFileSync('git',['rev-parse',`${p.product.release_commit}^{commit}`],{cwd:repository,encoding:'utf8'}).trim();assert.equal(actual,p.product.release_commit);}
if(probe){const path=resolve(directory,'dashboard-task.mp4');const m=JSON.parse(execFileSync('ffprobe',['-v','error','-show_streams','-show_format','-of','json',path],{encoding:'utf8'}));assert.equal(m.streams.length,1);const s=m.streams[0];assert.equal(s.codec_name,'h264');assert.equal(s.pix_fmt,'yuv420p');assert.equal(s.r_frame_rate,'24/1');assert.deepEqual([s.width,s.height],timeline.dimensions);assert(Math.abs(Number(m.format.duration)-timeline.playback_duration_ms/1000)<=1/24+0.001);
 let roundedSeeks=0;
 for(const f of timeline.frames){const frameIndex=f.output_start_frame+Math.floor((f.output_end_frame-f.output_start_frame-1)/2);const midpoint=Math.max(0,(frameIndex-0.1)/24);let matched=false,lastScore=null;
  // The encoder assigns explicit cumulative24fps frame boundaries to each sample.
  // Require the original image within one output frame of its exact mapped midpoint.
  for(const shift of [0,-1/24,1/24]){const timestamp=Math.max(0,midpoint+shift);const r=spawnSync('ffmpeg',['-hide_banner','-ss',String(timestamp),'-i',path,'-i','pipe:0','-lavfi','[0:v]trim=end_frame=1,setpts=PTS-STARTPTS[a];[1:v]setpts=PTS-STARTPTS[b];[a][b]ssim=shortest=1','-frames:v','1','-f','null','-'],{input:members.get(f.file),encoding:'utf8',maxBuffer:2*1024*1024});assert.equal(r.status,0,r.stderr);const score=r.stderr.match(/All:([0-9.]+)/);lastScore=score?.[1];if(score&&Number(score[1])>=0.99){matched=true;if(shift)roundedSeeks++;break;}}
  assert(matched,`Output differs from actual capture ${f.file} within one24fps frame of${midpoint}s: ${lastScore}`);
 }
 console.log(`All retained source samples matched at SSIM>=0.99; ${roundedSeeks} seeks needed at most one24fps frame of timestamp rounding.`);
}
console.log(`Verified ${timeline.frames.length} retained original JPEG samples, declared privacy omissions and recording gaps, chronological edit mapping, chapters, all asset hashes${probe?' and every encoded sample against the original':''}. Actual duration ${timeline.actual_duration_ms/1000}s; playback ${timeline.playback_duration_ms/1000}s.`);
