#!/usr/bin/env node
// Verify the immutable prior bundle through its explicit retention map.
import {readFileSync} from 'node:fs';
import path from 'node:path';
import {pathToFileURL} from 'node:url';
const root=path.resolve(process.argv[2]??'');
if(!process.argv[2])throw new Error('bundle directory required');
const base='C1-claude-code/continuation/';
const {openEnvelope,parseKeys,sha256}=await import(pathToFileURL(path.join(root,base+'candidate-sources/scripts/release-gate/evidence.mjs')));
const map=JSON.parse(readFileSync(path.join(root,base+'retention-map.json')));
function safe(rel){if(typeof rel!=='string'||path.isAbsolute(rel)||rel.split('/').some(x=>x==='..'||x===''||x==='.')||rel.includes('\\'))throw new Error('unsafe retention path');return path.join(root,rel);}
const manifestBytes=readFileSync(safe(map.overrides['manifest.json']));
const envelope=readFileSync(safe(map.overrides['manifest.dsse.json']),'utf8');
if(sha256(manifestBytes)!==map.manifest_sha256||sha256(Buffer.from(envelope))!==map.dsse_sha256)throw new Error('retained manifest or envelope changed');
const opened=openEnvelope(envelope,parseKeys(readFileSync(path.join(root,base+'gate-keys.public'),'utf8')));
if(!opened.ok||!opened.payload.equals(manifestBytes))throw new Error('original signature did not verify against exact manifest bytes');
const manifest=JSON.parse(manifestBytes);
if(manifest.commit!==map.commit||manifest.version!==map.version)throw new Error('retained candidate mismatch');
for(const entry of manifest.files){const actual=readFileSync(safe(map.overrides[entry.path]??entry.path));if(actual.length!==entry.bytes||sha256(actual)!==entry.sha256)throw new Error('retained source mismatch: '+entry.path);}
const original=JSON.parse(readFileSync(safe(map.overrides['C1-claude-code.json'])));
if(original.pass!==false||original.steps.length!==37||original.steps.filter(x=>x.status!=='pass').length!==2)throw new Error('original failed attempt was not retained');
console.log(JSON.stringify({signature_valid:true,signer:opened.key.shortId,source_files_verified:manifest.files.length+2,original_claude_pass:false,original_claude_steps:37,original_claude_failed_steps:2,manifest_sha256:map.manifest_sha256,dsse_sha256:map.dsse_sha256},null,2));
