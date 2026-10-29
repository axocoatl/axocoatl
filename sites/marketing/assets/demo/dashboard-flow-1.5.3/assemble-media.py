#!/usr/bin/env python3
"""Assemble actual dashboard screenshot segments; never operates the product."""
import argparse, datetime, gzip, hashlib, io, json, math, pathlib, re, shutil, subprocess, tarfile, tempfile

VERSION = '1.5.3'
COMMIT = 'c14e3957cd0a8cfd9e76480f724e902ada0f5aa4'
BINARY = 'feeb6eb174c7eefd1cea83c8a008bb658944ed8202c5c3e379c4b7dc49676919'
FPS = 24
LIMIT = 25 * 1024 * 1024

def require(ok, message):
    if not ok: raise ValueError(message)

def raw(path):
    require(path.is_file() and not path.is_symlink(), f'Not a regular file: {path}')
    return path.read_bytes()

def sha(data): return hashlib.sha256(data).hexdigest()
def dump(path, value): path.write_text(json.dumps(value, indent=2) + '\n')
def public(value):
    require(not re.search(r'sk-or-v1-|Bearer\s|[?&#](?:token|code|key)=', json.dumps(value), re.I), 'Authentication material in public metadata')

def dimensions(data):
    if data[:8] == b'\x89PNG\r\n\x1a\n':
        return [int.from_bytes(data[16:20], 'big'), int.from_bytes(data[20:24], 'big')]
    require(data[:2] == b'\xff\xd8', 'Expected original JPEG or PNG')
    i = 2
    while i < len(data):
        require(data[i] == 255, 'Invalid JPEG marker'); i += 1
        while data[i] == 255: i += 1
        marker = data[i]; i += 1
        if marker in (217, 218): break
        if marker == 1 or 208 <= marker <= 215: continue
        size = int.from_bytes(data[i:i+2], 'big')
        require(size >= 2 and i + size <= len(data), 'Invalid JPEG segment')
        if marker in (192, 193, 194):
            return [int.from_bytes(data[i+5:i+7], 'big'), int.from_bytes(data[i+3:i+5], 'big')]
        i += size
    raise ValueError('Image dimensions absent')

def stamp(ms):
    seconds, millis = divmod(math.floor(ms + .5), 1000)
    minutes, seconds = divmod(seconds, 60); hours, minutes = divmod(minutes, 60)
    return f'{hours:02}:{minutes:02}:{seconds:02}.{millis:03}'

def vtt(entries, end, text_key):
    lines = ['WEBVTT', '']
    for i, item in enumerate(entries):
        finish = entries[i+1]['playback_ms'] if i+1 < len(entries) else end
        require(finish > item['playback_ms'], 'Nonpositive cue')
        require('\n' not in item[text_key] and '-->' not in item[text_key], 'Unsafe VTT text')
        lines += [f"{stamp(item['playback_ms'])} --> {stamp(finish)}", item[text_key], '']
    return '\n'.join(lines)

def archive(output, name, members):
    with (output/name).open('wb') as file, gzip.GzipFile(fileobj=file, mode='wb', mtime=0) as gz, tarfile.open(fileobj=gz, mode='w', format=tarfile.USTAR_FORMAT) as tar:
        for filename, data in members:
            entry = tarfile.TarInfo(filename); entry.size = len(data); entry.mode = 0o644; entry.mtime = 0
            tar.addfile(entry, io.BytesIO(data))
    require((output/name).stat().st_size < LIMIT, f'Archive exceeds Pages limit: {name}')

def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--capture-dir', required=True, type=pathlib.Path)
    p.add_argument('--frame-dir', type=pathlib.Path)
    p.add_argument('--facts', required=True, type=pathlib.Path)
    p.add_argument('--artifact-evidence', required=True, type=pathlib.Path)
    p.add_argument('--receipt', required=True, type=pathlib.Path)
    p.add_argument('--receipt-verification', required=True, type=pathlib.Path)
    p.add_argument('--prior-receipt', action='append', default=[], type=pathlib.Path)
    p.add_argument('--clips', required=True, type=pathlib.Path)
    p.add_argument('--edit-plan', type=pathlib.Path)
    p.add_argument('--output', required=True, type=pathlib.Path)
    p.add_argument('--capture-finalized', action='store_true')
    p.add_argument('--plan-only', action='store_true')
    a = p.parse_args()
    require(a.capture_finalized, 'Capture must have ended before assembly')
    source_bytes = raw(a.capture_dir/'capture.json'); capture = json.loads(source_bytes)
    frame_dir = a.frame_dir or (a.capture_dir/'frames' if (a.capture_dir/'frames').is_dir() else a.capture_dir)
    require(capture.get('theme') == 'dark', 'Explicit Dark mode required')
    duration = capture['duration_ms']
    date = lambda text: datetime.datetime.fromisoformat(text.replace('Z', '+00:00'))
    require(abs((date(capture['finished_at']) - date(capture['started_at'])).total_seconds()*1000-duration) <= 10, 'Capture duration/timestamps differ')
    require(duration > 0 and len(capture['frames']) > 1, 'Empty capture')
    source = []; unique = set(); size = None; previous = -1
    for f in capture['frames']:
        name, t = f['file'], f['elapsed_ms']
        require(re.fullmatch(r'frame-\d{4,8}\.(?:jpg|png)', name) and name not in unique, 'Invalid or repeated frame name')
        require(previous < t < duration and t >= 0, 'Frame timestamps must increase')
        b = raw(frame_dir/name); shape = dimensions(b)
        require(max(shape) <= 4096 and min(shape) > 0, 'Unsupported image dimensions')
        size = size or shape; require(size == shape, 'Viewport changed during capture')
        source.append(dict(file=name, source_ms=t, sha256=sha(b), bytes=len(b)))
        unique.add(name); previous = t
    encoded_size = [x + x % 2 for x in size]
    segments = capture.get('segments') or [{'start_ms':0, 'end_ms':duration}]
    previous = -1
    for s in segments:
        require(previous <= s['start_ms'] < s['end_ms'] <= duration, 'Capture segments overlap or exceed clock')
        require(any(s['start_ms'] <= f['source_ms'] < s['end_ms'] for f in source), 'Empty capture segment')
        previous = s['end_ms']
    gaps = []; previous = 0
    for i, f in enumerate(source):
        owner = [s for s in segments if s['start_ms'] <= f['source_ms'] < s['end_ms']]
        require(len(owner) == 1, 'Frame outside capture segment')
        f['source_end_ms'] = min(source[i+1]['source_ms'] if i+1 < len(source) else duration, owner[0]['end_ms'])
        if previous < f['source_ms']: gaps.append(dict(start_ms=previous, end_ms=f['source_ms']))
        previous = f['source_end_ms']
    if previous < duration: gaps.append(dict(start_ms=previous, end_ms=duration))
    endpoints = {0, duration} | {f['source_ms'] for f in source} | {s[k] for s in segments for k in ('start_ms','end_ms')}
    edits = json.loads(raw(a.edit_plan))['segments'] if a.edit_plan else []
    previous = -1
    for e in edits:
        start, end = e['start_ms'], e['end_ms']; kind = e.get('kind', 'speed'); e['kind'] = kind
        require(kind in ('speed','omit') and previous <= start < end and start in endpoints and end in endpoints, 'Invalid edit window')
        require(isinstance(e.get('reason'), str) and len(e['reason']) > 5, 'Every edit needs disclosure')
        if kind == 'speed':
            require(1 < e['rate'] <= 24, 'Invalid speedup')
            require(not any(max(start,g['start_ms']) < min(end,g['end_ms']) for g in gaps), 'Speedup includes uncaptured interval')
        else: require(start > 0 and end < duration, 'Initial and final capture cannot be omitted')
        previous = end
    for gap in gaps:
        pieces = [(gap['start_ms'], gap['end_ms'])]
        for e in [e for e in edits if e['kind'] == 'omit']:
            next_pieces = []
            for start,end in pieces:
                if e['end_ms'] <= start or e['start_ms'] >= end: next_pieces.append((start,end))
                else:
                    if start < e['start_ms']: next_pieces.append((start,e['start_ms']))
                    if e['end_ms'] < end: next_pieces.append((e['end_ms'],end))
            pieces = next_pieces
        edits.extend(dict(kind='gap', start_ms=start, end_ms=end, reason='No screenshots captured between recorded segments') for start,end in pieces)
    for f in source:
        length=f['source_end_ms']-f['source_ms']
        if length < 1000/FPS and not any(e['start_ms'] <= f['source_ms'] < e['end_ms'] for e in edits):
            edits.append(dict(kind='hold',start_ms=f['source_ms'],end_ms=f['source_end_ms'],rate=length/(1000/FPS),reason='Hold this short original screenshot sample for one encoded frame; no source sample is dropped.'))
    edits.sort(key=lambda e:e['start_ms'])
    def mapped(t):
        return t-sum(max(0,min(t,e['end_ms'])-e['start_ms'])*(1 if e['kind'] in ('omit','gap') else 1-1/e['rate']) for e in edits)
    omitted = lambda t: any(e['kind'] in ('omit','gap') and e['start_ms'] <= t < e['end_ms'] for e in edits)
    retained = [f for f in source if not omitted(f['source_ms'])]
    for f in retained:
        f['playback_ms'] = mapped(f['source_ms']); f['playback_end_ms'] = mapped(f['source_end_ms'])
    facts = json.loads(raw(a.facts)); evidence = json.loads(raw(a.artifact_evidence)); verification = json.loads(raw(a.receipt_verification))
    public(facts); public(evidence); public(verification)
    require(facts['product']['version'] == VERSION and facts['product']['release_commit'] == COMMIT and facts['product']['binary_sha256'] == BINARY, 'Wrong released product identity')
    require(facts['capture']['theme'] == 'dark' and facts['capture']['human_operator'] is False, 'Capture attribution missing')
    require(facts['presentation']['new_inference'] is True and facts['presentation']['run_started_from'] == 'dashboard', 'Actual new dashboard task required')
    require(facts['outcome']['status'] in ('accepted','failed','interrupted','partial') and not facts['outcome'].get('do_not_publish'), 'Final actual outcome required')
    subprocess.run(['node',str(pathlib.Path(__file__).with_name('verify-media.mjs')),'--check-inputs',str(a.artifact_evidence),'--facts',str(a.facts)],check=True)
    receipt_bytes = raw(a.receipt); public(json.loads(receipt_bytes))
    require(verification.get('result') == 'valid' and verification.get('signature') == 'trusted' and verification.get('exit_code') == 0, 'Trusted offline receipt verification required')
    require(verification['run_id'] in facts['product']['turn_ids'], 'Receipt is not from recorded task')
    clip_specs = json.loads(raw(a.clips)); require(set(clip_specs) == {'dashboard-flow','review','receipt'}, 'Exactly three requested clips required')
    clips = {}; timeline_clips = {}
    for name, spec in clip_specs.items():
        start, end = spec['start_ms'], spec['end_ms']
        require(start in {f['source_ms'] for f in retained} and end in endpoints and start < end, f'Clip bounds must use source frame/segment boundaries: {name}')
        rows = [dict(f) for f in retained if start <= f['source_ms'] < end]
        require(len(rows) > 1, f'Clip has insufficient actual frames: {name}')
        origin = mapped(start)
        for f in rows:
            f['playback_ms'] -= origin; f['source_end_ms'] = min(f['source_end_ms'], end)
            f['playback_end_ms'] = mapped(f['source_end_ms'])-origin
            f['output_start_frame'] = math.floor(f['playback_ms']*FPS/1000+.5)
            f['output_end_frame'] = math.floor(f['playback_end_ms']*FPS/1000+.5)
            require(f['output_end_frame'] > f['output_start_frame'], 'Quantization lost original sample')
        length = mapped(end)-origin
        cues = {}
        for key, text_key in [('chapters','title'),('captions','text')]:
            values = spec[key]; require(values and values[0]['source_ms'] == start, f'{key} must begin at clip start')
            previous = -1; cues[key] = []
            for item in values:
                t = item['source_ms']; require(previous < t < end and t >= start and not omitted(t), 'Cue outside shown capture')
                require(isinstance(item[text_key],str) and item[text_key].strip(), 'Empty cue')
                cues[key].append({**item,'playback_ms':mapped(t)-origin}); previous = t
            if key == 'captions':
                for i,item in enumerate(cues[key]):
                    finish = cues[key][i+1]['playback_ms'] if i+1<len(cues[key]) else length
                    require(finish-item['playback_ms'] >= 2000, 'Caption shorter than two seconds')
        poster_name = spec['poster']; require(poster_name in {f['file'] for f in rows}, 'Poster must be an original shown source sample')
        extension = pathlib.Path(poster_name).suffix
        clips[name] = dict(video=f'{name}.mp4', poster=f'{name}{extension}', captions=f'{name}.captions.vtt', chapterTrack=f'{name}.chapters.vtt', duration_seconds=length/1000, chapters=[dict(time_seconds=c['playback_ms']/1000,label=c['title']) for c in cues['chapters']])
        timeline_clips[name] = dict(start_ms=start,end_ms=end,duration_ms=length,frames=rows,poster_source=poster_name,**cues)
    require(clip_specs['dashboard-flow']['start_ms'] == retained[0]['source_ms'] and clip_specs['dashboard-flow']['end_ms'] == retained[-1]['source_end_ms'], 'Full walkthrough must cover every retained sample')
    safe_actions = [{**action, 'playback_ms':mapped(action['elapsed_ms']), 'visually_captured':not any(g['start_ms']<=action['elapsed_ms']<g['end_ms'] for g in gaps), 'shown_in_film':not omitted(action['elapsed_ms'])} for action in capture['actions'] if not any(e['kind']=='omit' and e.get('privacy',True) and e['start_ms']<=action['elapsed_ms']<e['end_ms'] for e in edits)]
    public_capture = {k:capture[k] for k in ('started_at','finished_at','duration_ms','theme','errors')}
    public_capture.update(frames=[dict(file=f['file'],elapsed_ms=f['source_ms']) for f in retained], segments=segments, actions=safe_actions, original_capture_sha256=sha(source_bytes), original_frame_count=len(source), edit_plan=edits, uncaptured_gaps=gaps)
    public(public_capture)
    public_capture_bytes=(json.dumps(public_capture,indent=2)+'\n').encode()
    timeline = dict(schema='axocoatl.dashboard-flow-timeline/1',dimensions=size,encoded_dimensions=encoded_size,fps=FPS,source_capture_sha256=sha(public_capture_bytes),original_capture_sha256=sha(source_bytes),actual_duration_ms=duration,frames=retained,omitted_frames=[f for f in source if omitted(f['source_ms'])],recording_segments=segments,uncaptured_gaps=gaps,edit_plan=edits,actions=safe_actions,clips=timeline_clips)
    public(timeline)
    if a.plan_only:
        print(json.dumps(dict(dimensions=encoded_size,source_dimensions=size,frames=len(retained),original_frames=len(source),actual_seconds=duration/1000,clips=clips,edits=edits),indent=2)); return
    require(not a.output.exists(), 'Output directory must be new; previous attempts are preserved')
    a.output.mkdir(parents=True,mode=0o700)
    dump(a.output/'timeline.json',timeline); dump(a.output/'integration.json',dict(schema='axocoatl.dashboard-flow-integration/1',version=VERSION,dimensions=encoded_size,source_dimensions=size,clips=clips))
    (a.output/'artifact-evidence.json').write_bytes(raw(a.artifact_evidence)); (a.output/'receipt.dsse.json').write_bytes(receipt_bytes)
    prior_receipts=[]
    for i,path in enumerate(a.prior_receipt,1):
        name=f'prior-receipt-{i:02}.dsse.json'; data=raw(path); public(json.loads(data))
        (a.output/name).write_bytes(data); prior_receipts.append(name)
    # The private verification result may name its local checkout; the public projection removes only that path.
    verification = json.loads(json.dumps(verification)); verification.get('files',{}).pop('repo',None)
    dump(a.output/'receipt-verification.json',verification)
    source_archives=[]; group=[('capture.json',public_capture_bytes)]; size_total=len(public_capture_bytes)
    for f in retained:
        b=raw(frame_dir/f['file'])
        if size_total+len(b)>20*1024*1024 and len(group)>1:
            name=f'source-{len(source_archives)+1:03}.tar.gz'; archive(a.output,name,group); source_archives.append(name); group=[]; size_total=0
        group.append((f['file'],b)); size_total+=len(b)
    if group:
        name=f'source-{len(source_archives)+1:03}.tar.gz'; archive(a.output,name,group); source_archives.append(name)
    work=pathlib.Path(tempfile.mkdtemp(prefix='axo-flow-encode-',dir=str(a.output.parent)))
    for name, clip in timeline_clips.items():
        rows=clip['frames']; lines=['ffconcat version 1.0']
        for f in rows:
            path=(frame_dir/f['file']).resolve(); require("'" not in str(path) and '\n' not in str(path),'Unsafe concat path')
            lines += [f"file '{path}'", 'option framerate 24', f"duration {(f['output_end_frame']-f['output_start_frame'])/FPS:.9f}"]
        lines += [f"file '{(frame_dir/rows[-1]['file']).resolve()}'", 'option framerate 24']
        concat=work/f'{name}.ffconcat'; concat.write_text('\n'.join(lines)+'\n')
        command=['ffmpeg','-hide_banner','-nostdin','-n','-f','concat','-safe','0','-i',str(concat),'-an','-vf','fps=24,pad=ceil(iw/2)*2:ceil(ih/2)*2:0:0:color=black,scale=in_range=pc:out_range=tv,format=yuv420p','-c:v','libx264','-preset','slow','-crf','18','-pix_fmt','yuv420p','-color_range','tv','-movflags','+faststart','-t',f"{clip['duration_ms']/1000:.9f}",str(a.output/clips[name]['video'])]
        with (work/f'{name}.log').open('w') as log: subprocess.run(command,stdout=log,stderr=subprocess.STDOUT,check=True)
        (a.output/clips[name]['poster']).write_bytes(raw(frame_dir/clip['poster_source']))
        (a.output/clips[name]['chapterTrack']).write_text(vtt(clip['chapters'],clip['duration_ms'],'title'))
        (a.output/clips[name]['captions']).write_text(vtt(clip['captions'],clip['duration_ms'],'text'))
    for name in ('assemble-media.py','verify-media.mjs','ASSEMBLY.md'):
        shutil.copyfile(pathlib.Path(__file__).with_name(name),a.output/name)
    names=['integration.json','timeline.json','artifact-evidence.json','receipt.dsse.json','receipt-verification.json','assemble-media.py','verify-media.mjs','ASSEMBLY.md']+prior_receipts+source_archives
    for clip in clips.values(): names += [clip[k] for k in ('video','poster','captions','chapterTrack')]
    files=[dict(file=n,sha256=sha(raw(a.output/n)),bytes=(a.output/n).stat().st_size) for n in names]
    for f in files: require(f['bytes'] < LIMIT, f"Pages per-file limit exceeded: {f['file']}")
    provenance={**facts,'schema':'axocoatl.dashboard-flow-provenance/1','files':files,'clips':clips,'source_archives':source_archives,'prior_receipts':prior_receipts,'source_frames':len(retained),'actual_duration_ms':duration,'encoding':dict(codec='h264',pixel_format='yuv420p',fps=FPS,audio=False,dimensions=encoded_size,source_dimensions=size,padding=dict(left=0,top=0,right=encoded_size[0]-size[0],bottom=encoded_size[1]-size[1],color='black',reason='H.264 YUV420p requires even dimensions; every original dashboard pixel remains unchanged before encoding.'),frames='Chronological original screenshot samples; repeated frames only. No fabricated UI or motion interpolation.',edits='Recorded gaps removed, declared waits accelerated, and short samples held for one encoded frame; inspect timeline.json.',encoder_sha256=sha(raw(pathlib.Path(__file__))))}
    public(provenance); dump(a.output/'provenance.json',provenance)
    print(json.dumps(dict(output=str(a.output),source_frames=len(retained),clips=clips,source_archives=source_archives),indent=2))

if __name__=='__main__': main()
