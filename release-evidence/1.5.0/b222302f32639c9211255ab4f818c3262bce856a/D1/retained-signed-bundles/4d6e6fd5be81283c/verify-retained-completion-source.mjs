#!/usr/bin/env node
// Verify an immutable prior signed bundle through its explicit relocation map.
import {readFileSync,lstatSync} from 'node:fs';
import path from 'node:path';
import {pathToFileURL} from 'node:url';
const [stageArg,mapArg,repoArg]=process.argv.slice(2);
if(!stageArg||!mapArg||!repoArg)throw Error('usage: stage relative-map repo');
const stage=path.resolve(stageArg),repo=path.resolve(repoArg);
const {openEnvelope,parseKeys,sha256}=await import(pathToFileURL(path.join(repo,'scripts/release-gate/evidence.mjs')));
function safe(rel){if(typeof rel!=='string'||!rel||path.isAbsolute(rel)||rel.includes('\\')||rel.split('/').some(x=>!x||x==='.'||x==='..'))throw Error('unsafe retention path');return path.join(stage,rel);}
function read(rel){const p=safe(rel);if(!lstatSync(p).isFile())throw Error('nonregular retained member');return readFileSync(p);}
const map=JSON.parse(read(mapArg));
if(map.schema!=='axocoatl.recovery.retained-bundle-map/2')throw Error('unknown map schema');
const rel=(p)=>map.overrides[p]??p;
const manifest=read(rel('manifest.json')),envelope=read(rel('manifest.dsse.json'));
if(sha256(manifest)!==map.source_manifest_sha256||sha256(envelope)!==map.source_dsse_sha256)throw Error('source pins do not match');
const keys=parseKeys(readFileSync(path.join(repo,'.axocoatl/gate-keys'),'utf8'));
const opened=openEnvelope(envelope.toString(),keys);
if(!opened.ok||!opened.payload.equals(manifest))throw Error('source signature invalid');
const m=JSON.parse(manifest);
if(m.commit!==map.candidate_commit||m.version!==map.version)throw Error('source candidate differs');
const seen=new Set();
for(const e of m.files){if(seen.has(e.path))throw Error('duplicate source member');seen.add(e.path);const b=read(rel(e.path));if(b.length!==e.bytes||sha256(b)!==e.sha256)throw Error('retained source file differs: '+e.path);}
if(m.files.length+2!==map.source_file_count)throw Error('source file count differs');
console.log(JSON.stringify({signature_valid:true,signer:opened.key.shortId,original_files_verified:m.files.length+2,original_paths:m.paths,reconstruction:'Reconstruct prior bundle through this map before verifying its older nested maps.'},null,2));
