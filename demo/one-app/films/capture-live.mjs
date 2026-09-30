#!/usr/bin/env node
/** Capture actual UI states. This helper never mocks HTTP, synthesizes evidence, or marks a take accepted. */
import {createHash} from 'node:crypto';
import {mkdir, readFile, writeFile} from 'node:fs/promises';
import {resolve} from 'node:path';
import {fileURLToPath} from 'node:url';
import {findFilm, loadPortfolio, validateTimeline} from './film-lib.mjs';

const digest = bytes => createHash('sha256').update(bytes).digest('hex');
export function validateCapturePlan(plan, portfolio = loadPortfolio()) {
  const film = findFilm(portfolio, plan.film);
  const origin = new URL(plan.base_url);
  if (!['localhost','127.0.0.1','[::1]'].includes(origin.hostname) || origin.protocol !== 'http:' || origin.username || origin.password || origin.pathname !== '/' || origin.search || origin.hash) throw Error('Capture requires an explicit loopback HTTP origin.');
  if (plan.start_path && (!plan.start_path.startsWith('/') || new URL(plan.start_path,origin).origin !== origin.origin)) throw Error('Initial navigation must stay on the captured product origin.');
  if (!['light','dark'].includes(plan.theme)) throw Error('Capture theme must be light or dark.');
  if (!Array.isArray(plan.shots) || plan.shots.length !== film.beats.length) throw Error('One shot per portfolio beat is required.');
  const timeline = {schema_version:1,film:film.slug,input_fps:8,shots:plan.shots.map(shot=>({beat:shot.beat,source:`shot-${shot.beat}.jpg`,hold_frames:shot.hold_frames}))};
  validateTimeline(portfolio,film,timeline);
  for (const shot of plan.shots) {
    if (!Array.isArray(shot.steps) || !Array.isArray(shot.evidence)) throw Error('Each shot requires steps and evidence arrays.');
    for (const step of shot.steps) {
      if (!['click','fill','select','press','wait_visible','wait_text','reload','goto','wait'].includes(step.action)) throw Error(`Unsupported UI action: ${step.action}`);
      if (step.action === 'goto' && (!step.path?.startsWith('/') || new URL(step.path,origin).origin !== origin.origin)) throw Error('Navigation must stay on the captured product origin.');
      if (step.action === 'wait' && (!Number.isInteger(step.ms) || step.ms < 0 || step.ms > 60000)) throw Error('Wait must be 0–60000ms.');
      if (!['reload','goto','wait'].includes(step.action) && !(step.selector || (step.role && step.name))) throw Error('UI action requires a selector or accessible role/name.');
    }
    for (const proof of shot.evidence) {
      if (!/^[a-z0-9-]+$/.test(proof.name) || !proof.path?.startsWith('/api/') || new URL(proof.path,origin).origin !== origin.origin) throw Error('Evidence must name a same-origin API GET.');
    }
  }
  return {film,timeline,origin:origin.origin};
}

export async function captureLive(plan, outputDirectory) {
  const {film,timeline,origin} = validateCapturePlan(plan);
  // Exclusive directory creation protects every previous take, including failed takes.
  await mkdir(outputDirectory,{recursive:false});
  const {chromium} = await import('../../../axocoatl-server/browser-tests/node_modules/playwright/index.mjs');
  const executablePath = process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE;
  const browser = await chromium.launch({headless:true,...(executablePath?{executablePath}:{})});
  const context = await browser.newContext({viewport:{width:1280,height:720},deviceScaleFactor:1,colorScheme:plan.theme,reducedMotion:'reduce'});
  const page = await context.newPage(),errors=[],observations=[];
  page.setDefaultTimeout(60000);
  page.on('pageerror',error=>errors.push(error.message));
  const save = async(name,bytes)=>{await writeFile(resolve(outputDirectory,name),bytes,{flag:'wx'});return {path:name,sha256:digest(bytes)};};
  await save('capture-plan.json',JSON.stringify(plan,null,2)+'\n');
  const started = new Date().toISOString();
  try {
    await page.goto(origin+(plan.start_path||'/'));
    await page.evaluate(theme=>{localStorage.setItem('axo:theme-pref',theme);if(theme==='light')document.documentElement.dataset.theme='light';else document.documentElement.removeAttribute('data-theme');},plan.theme);
    for (const shot of plan.shots) {
      for (const step of shot.steps) {
        const target = step.selector ? page.locator(step.selector) : step.role ? page.getByRole(step.role,{name:step.name,exact:step.exact!==false}) : null;
        if (step.action==='click') await target.click();
        else if(step.action==='fill') await target.fill(step.value);
        else if(step.action==='select') await target.selectOption(step.value);
        else if(step.action==='press') await target.press(step.key);
        else if(step.action==='wait_visible') await target.waitFor({state:'visible'});
        else if(step.action==='wait_text') await target.filter({hasText:step.text}).waitFor({state:'visible'});
        else if(step.action==='reload') await page.reload();
        else if(step.action==='goto') await page.goto(origin+step.path);
        else if(step.action==='wait') await page.waitForTimeout(step.ms);
      }
      const proofs=[];
      for (const proof of shot.evidence) {
        const response=await context.request.get(origin+proof.path);
        const bytes=await response.body();
        if(bytes.length>4*1024*1024)throw Error('Evidence response exceeds 4MiB; retain it independently.');
        proofs.push({url:proof.path,status:response.status(),...(await save(`${shot.beat}-${proof.name}.json`,bytes))});
        if(!response.ok())throw Error(`Evidence GET failed: ${proof.path} (${response.status()})`);
      }
      const bytes=await page.screenshot({type:'jpeg',quality:94,fullPage:false});
      if(observations.some(previous=>previous.screenshot.sha256===digest(bytes)))throw Error('Distinct beats cannot reuse identical screenshots.');
      const screenshot=await save(`shot-${shot.beat}.jpg`,bytes);
      const text=await save(`${shot.beat}-visible.txt`,await page.locator('body').innerText());
      observations.push({beat:shot.beat,captured_at:new Date().toISOString(),url:page.url(),screenshot,text,evidence:proofs});
    }
    if(errors.length)throw Error('Browser errors occurred; inspect observations before accepting any take.');
    await save('timeline.json',JSON.stringify(timeline,null,2)+'\n');
  } finally {
    await save('observations.json',JSON.stringify({schema_version:1,film:film.slug,started_at:started,browser:browser.version(),viewport:{width:1280,height:720,device_scale_factor:1},theme:plan.theme,reduced_motion:'reduce',observations,errors,acceptance:'not_reviewed'},null,2)+'\n');
    await browser.close();
  }
}
if(process.argv[1]&&resolve(process.argv[1])===fileURLToPath(import.meta.url)) {
  if(process.argv.length!==4)throw Error('Usage: capture-live.mjs <reviewed-plan.json> <new-output-dir>');
  await captureLive(JSON.parse(await readFile(process.argv[2],'utf8')),resolve(process.argv[3]));
  console.log('Actual UI capture retained. Review every beat and durable evidence before writing passed acceptance.');
}
