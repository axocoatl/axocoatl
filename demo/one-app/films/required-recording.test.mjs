import assert from 'node:assert/strict';
import {execFileSync,spawnSync} from 'node:child_process';
import {chmodSync,copyFileSync,mkdirSync,mkdtempSync,readFileSync,rmSync,writeFileSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {dirname,join,resolve} from 'node:path';
import test from 'node:test';
import {repoRoot,sha256File,sourceContentDigest,provenanceSourceExcludes,validatePortfolio} from './film-lib.mjs';

const read=path=>JSON.parse(readFileSync(path,'utf8'));
function write(root,path,bytes){const target=join(root,path);mkdirSync(dirname(target),{recursive:true});writeFileSync(target,bytes);}
function json(root,path,value){write(root,path,JSON.stringify(value,null,2)+'\n');}
function git(root,...args){return execFileSync('git',args,{cwd:root,encoding:'utf8',stdio:['ignore','pipe','pipe']}).trim();}

// Development fixture: copy historical real media/receipts unchanged, then bind
// the copies to a synthetic local test binary/source. Never rewrites originals
// or asserts that this fixture is a product recording.
function fixture(t){
 const root=mkdtempSync(join(tmpdir(),'axocoatl-required-film-'));t.after(()=>rmSync(root,{recursive:true,force:true}));
 git(root,'init','--quiet');git(root,'config','user.name','Film Gate Test');git(root,'config','user.email','film-gate@example.invalid');
 const portfolio=read(resolve(repoRoot,'demo/one-app/films/compatibility/v1.0.1-portfolio.json'));
 for(const film of portfolio.films)film.status='required';
 validatePortfolio(portfolio);json(root,'demo/one-app/films/portfolio.json',portfolio);
 for(const name of ['film-lib.mjs','verify-film-set.mjs','release-compatibility.mjs'])write(root,'demo/one-app/films/'+name,readFileSync(resolve(repoRoot,'demo/one-app/films/'+name)));
 write(root,'demo/one-app/films/SHOT-MANIFEST.md',portfolio.films.map(f=>'## `'+f.slug+'`').join('\n'));
 write(root,'.gitignore','target/\n');write(root,'product.txt','frozen test product\n');
 const binaryPath='target/release/axocoatl';write(root,binaryPath,"#!/bin/sh\nprintf 'axocoatl film-gate fixture\\n'\n");chmodSync(join(root,binaryPath),0o755);
 for(const film of portfolio.films){
  write(root,film.scenario,'## Recording beats\nDevelopment fixture.\n## Durable evidence\nCopied receipts for verifier tests only.\n');
  const p=read(resolve(repoRoot,film.provenance));
  const paths=new Set([film.media.mp4,film.media.poster,p.capture.record,p.edit.timeline,p.edit.stage_record,p.evidence.record,...p.capture.keyframes.map(k=>k.path)]);
  for(const path of paths){mkdirSync(dirname(join(root,path)),{recursive:true});copyFileSync(resolve(repoRoot,path),join(root,path));}
  json(root,film.provenance,p);
 }
 git(root,'add','.');git(root,'commit','--quiet','-m','development fixture');
 const source={branch:git(root,'rev-parse','--abbrev-ref','HEAD'),head:git(root,'rev-parse','HEAD'),dirty:false,patch_sha256:null,patch_excludes:[...provenanceSourceExcludes],content_sha256:sourceContentDigest(root)};
 for(const film of portfolio.films){const p=read(join(root,film.provenance));p.source=source;p.binary={path:binaryPath,version:'axocoatl film-gate fixture',sha256:sha256File(join(root,binaryPath))};json(root,film.provenance,p);}
 const run=(...args)=>spawnSync(process.execPath,[join(root,'demo/one-app/films/verify-film-set.mjs'),...args],{cwd:root,encoding:'utf8'});
 return{root,portfolio,run};
}

test('required recording verifies complete media, provenance, source and binary without changing manifest acceptance state',t=>{
 const {root,portfolio,run}=fixture(t),manifestPath=join(root,'demo/one-app/films/portfolio.json'),manifestBefore=readFileSync(manifestPath,'utf8');
 let result=run();assert.equal(result.status,0,result.stderr||result.stdout);assert.match(result.stdout,/12 verified films/);
 result=run('--source-bound');assert.equal(result.status,0,result.stderr||result.stdout);
 result=run('--allow-needs-recording');assert.equal(result.status,0,result.stderr||result.stdout);assert.doesNotMatch(result.stderr,/WARN/);
 assert.equal(readFileSync(manifestPath,'utf8'),manifestBefore,'acceptance is derived without mutating the source-hashed manifest');
 const first=portfolio.films[0],provenancePath=join(root,first.provenance),provenance=readFileSync(provenancePath);
 rmSync(provenancePath);result=run('--allow-needs-recording');assert.notEqual(result.status,0);assert.match(result.stderr,/provenance must be|provenance does not exist/);writeFileSync(provenancePath,provenance);
 const mediaPath=join(root,first.media.mp4),media=readFileSync(mediaPath);rmSync(mediaPath);result=run('--allow-needs-recording');assert.notEqual(result.status,0);assert.match(result.stderr,/MP4 does not exist/);writeFileSync(mediaPath,media);
 write(root,'product.txt','changed after recording\n');
 for(const mode of ['--source-bound','--allow-needs-recording']){result=run(mode);assert.notEqual(result.status,0);assert.match(result.stderr,/source content differs from the recorded checkout/);}
 write(root,'product.txt','frozen test product\n');
 write(root,'target/release/axocoatl',"#!/bin/sh\nprintf 'different binary\\n'\n");result=run();assert.notEqual(result.status,0);assert.match(result.stderr,/release binary hash changed/);
});

test('legacy recording declarations remain valid and only needs_recording can opt out in the explicit non-release mode',()=>{
 const portfolio=read(resolve(repoRoot,'demo/one-app/films/compatibility/v1.0.1-portfolio.json'));
 for(const status of ['ready','needs_recording','required']){portfolio.films[0].status=status;assert.doesNotThrow(()=>validatePortfolio(portfolio));}
 portfolio.films[0].status='accepted';assert.throws(()=>validatePortfolio(portfolio),/status must/);
});
