"""Archive performance acceptance; isolated data, no model calls or source writes.
Windows Job accounting includes exited descendants. RSS is sampled every 20 ms;
peak committed memory and CPU/I/O counters come from the kernel job object.
"""
from __future__ import annotations
import argparse, csv, ctypes as C, hashlib, json, math, os, random, shutil
import sqlite3, statistics, subprocess, time, zipfile
from ctypes import wintypes as W
from pathlib import Path

K=C.WinDLL('kernel32',use_last_error=True)
P=C.WinDLL('psapi',use_last_error=True)
S=C.c_size_t
class IO(C.Structure):
    _fields_=[(n,C.c_ulonglong) for n in ['read_ops','write_ops','other_ops','read_bytes','write_bytes','other_bytes']]
class BASIC(C.Structure):
    _fields_=[('user',C.c_longlong),('kernel',C.c_longlong),('period_user',C.c_longlong),('period_kernel',C.c_longlong),('faults',W.DWORD),('processes',W.DWORD),('active',W.DWORD),('terminated',W.DWORD)]
class ACCOUNT(C.Structure):
    _fields_=[('basic',BASIC),('io',IO)]
class LIMIT(C.Structure):
    _fields_=[('process_time',C.c_longlong),('job_time',C.c_longlong),('flags',W.DWORD),('min_ws',S),('max_ws',S),('active',W.DWORD),('affinity',S),('priority',W.DWORD),('scheduling',W.DWORD)]
class EXT(C.Structure):
    _fields_=[('basic',LIMIT),('io',IO),('process_memory',S),('job_memory',S),('peak_process_memory',S),('peak_job_memory',S)]
class MEM(C.Structure):
    _fields_=[('cb',W.DWORD),('faults',W.DWORD),('peak_ws',S),('ws',S),('peak_paged',S),('paged',S),('peak_nonpaged',S),('nonpaged',S),('pagefile',S),('peak_pagefile',S),('private',S)]
K.CreateJobObjectW.argtypes=[C.c_void_p,W.LPCWSTR]; K.CreateJobObjectW.restype=W.HANDLE
K.AssignProcessToJobObject.argtypes=[W.HANDLE,W.HANDLE]
K.QueryInformationJobObject.argtypes=[W.HANDLE,C.c_int,C.c_void_p,W.DWORD,C.c_void_p]
K.SetInformationJobObject.argtypes=[W.HANDLE,C.c_int,C.c_void_p,W.DWORD]
K.CloseHandle.argtypes=[W.HANDLE]
K.OpenProcess.argtypes=[W.DWORD,W.BOOL,W.DWORD]; K.OpenProcess.restype=W.HANDLE
P.GetProcessMemoryInfo.argtypes=[W.HANDLE,C.c_void_p,W.DWORD]
N=C.WinDLL('ntdll'); N.NtResumeProcess.argtypes=[W.HANDLE]

def check(ok):
    if not ok: raise C.WinError(C.get_last_error())

def save(path,value):
    reject_frozen_output(path)
    path.parent.mkdir(parents=True,exist_ok=True)
    temp=path.with_suffix(path.suffix+'.tmp')
    temp.write_text(json.dumps(value,ensure_ascii=False,indent=2),encoding='utf-8')
    os.replace(temp,path)

def run(exe,config,arguments,out):
    reject_frozen_output(out)
    out.mkdir(parents=True,exist_ok=True)
    job=K.CreateJobObjectW(None,None); check(job)
    limit=EXT(); limit.basic.flags=0x200|0x2000; limit.job_memory=3*1024**3
    check(K.SetInformationJobObject(job,9,C.byref(limit),C.sizeof(limit)))
    started=time.perf_counter()
    with (out/'stdout.json').open('w',encoding='utf-8') as stdout, (out/'stderr.log').open('w',encoding='utf-8') as stderr:
        process=subprocess.Popen([str(exe),'--config-dir',str(config),*arguments],stdout=stdout,stderr=stderr,creationflags=0x4|0x08000000)
        check(K.AssignProcessToJobObject(job,int(process._handle)))
        status=N.NtResumeProcess(int(process._handle))
        if status!=0: raise RuntimeError('NtResumeProcess failed')
        rss=0; parent_peak=0
        while process.poll() is None:
            ids=C.create_string_buffer(8+8*128)
            check(K.QueryInformationJobObject(job,3,ids,len(ids),None))
            count=int.from_bytes(ids.raw[4:8],'little'); current=0
            for index in range(min(count,128)):
                pid=int.from_bytes(ids.raw[8+8*index:16+8*index],'little')
                handle=K.OpenProcess(0x1000|0x10,False,pid)
                if handle:
                    memory=MEM(); memory.cb=C.sizeof(memory)
                    if P.GetProcessMemoryInfo(handle,C.byref(memory),C.sizeof(memory)):
                        current+=memory.ws
                        if pid==process.pid: parent_peak=max(parent_peak,memory.peak_ws)
                    K.CloseHandle(handle)
            rss=max(rss,current); time.sleep(.02)
    elapsed=time.perf_counter()-started
    account=ACCOUNT(); peak=EXT()
    check(K.QueryInformationJobObject(job,8,C.byref(account),C.sizeof(account),None))
    check(K.QueryInformationJobObject(job,9,C.byref(peak),C.sizeof(peak),None))
    K.CloseHandle(job)
    result={'wall_seconds':elapsed,'cpu_seconds':(account.basic.user+account.basic.kernel)/1e7,'processes':account.basic.processes,'peak_tree_rss_bytes':rss,'peak_tree_commit_bytes':peak.peak_job_memory,'peak_parent_rss_bytes':parent_peak,'read_bytes':account.io.read_bytes,'write_bytes':account.io.write_bytes,'exit_code':process.returncode,'rss_sampling_ms':20}
    result['output']=json.loads((out/'stdout.json').read_text(encoding='utf-8-sig') or '{}')
    save(out/'measurement.json',result)
    if process.returncode: raise RuntimeError(f'CLI failed: {out}')
    return result

def configure(base,work,exe,scanner,read,parse):
    reject_frozen_output(base)
    cfg=base/'config'; cfg.mkdir(parents=True,exist_ok=True)
    settings={'paths':{'work_dir':str(work),'data_dir':str(base/'data'),'db_dir':str(base/'data/db'),'reports_dir':str(base/'reports'),'log_dir':str(base/'logs')},'scanner':{**scanner,'office_worker_path':str(exe.parent/'ai-daily-office-parser.exe'),'pdf_worker_path':str(exe.parent/'ai-daily-pdf-parser.exe')},'archive':{'read_workers':read,'parse_workers':parse,'memory_mib':3072,'system_reserve_mib':2048,'parent_memory_mib':512,'worker_memory_mib':512}}
    # JSON is valid YAML; no configuration library is needed.
    save(cfg/'settings.yaml',settings)
    return cfg

def summary(db,output):
    con=sqlite3.connect(db); con.row_factory=sqlite3.Row
    rows=[dict(row) for row in con.execute('SELECT d.path,v.sha256,v.size_bytes,v.extension,e.status,e.error_code,e.manifest_json FROM documents d LEFT JOIN versions v ON v.id=d.current_version LEFT JOIN version_extractions ve ON ve.version_id=v.id LEFT JOIN extractions e ON e.id=ve.extraction_id ORDER BY d.path')]
    signatures=[]
    for row in rows:
        manifest=json.loads(row.pop('manifest_json') or '{}')
        row.update({key:manifest.get(key) for key in ['body_sha256','units_sha256','index_sha256','units','body_bytes','units_bytes','truncated','gaps','coverage','parser_backend','worker_lane']})
        signatures.append({key:row[key] for key in ['sha256','extension','status','error_code','body_sha256','units_sha256','index_sha256','truncated','gaps','coverage']})
    signatures.sort(key=lambda row:(row['sha256'] or '',row['extension'] or ''))
    digest=hashlib.sha256(json.dumps(signatures,sort_keys=True,ensure_ascii=False).encode()).hexdigest()
    metrics=[dict(row) for row in con.execute('SELECT f.*,v.extension,v.size_bytes FROM file_metrics f LEFT JOIN versions v ON v.id=f.version_id ORDER BY f.collection_id,f.path')]
    with (output/'file-timings.csv').open('w',newline='',encoding='utf-8-sig') as stream:
        writer=csv.DictWriter(stream,fieldnames=list(metrics[0]) if metrics else ['path']); writer.writeheader(); writer.writerows(metrics)
    slow=sorted(metrics,key=lambda row:sum(row.get(k) or 0 for k in ['capture_ms','parse_ms','diff_ms']),reverse=True)[:50]
    save(output/'slow-files.json',slow); save(output/'all-files.json',rows)
    result={'files':len(rows),'bytes':sum(row['size_bytes'] or 0 for row in rows),'signature':digest,'statuses':{status:sum(row['status']==status for row in rows) for status in sorted({str(row['status']) for row in rows})},'errors':[{'path':r['path'],'code':r['error_code']} for r in rows if r['status'] not in ['complete','partial']]}
    con.close(); save(output/'coverage.json',result); return result

def cold(base,work,exe,scanner,read,parse):
    complete=base/'result.json'
    if complete.exists(): return json.loads(complete.read_text(encoding='utf-8'))
    config=configure(base,work,exe,scanner,read,parse)
    runs=[]
    baseline=not (base/'data/archive/index.sqlite3').exists()
    # Each CLI run has a durable 20 minute deadline. Resume only the recorded queue.
    for attempt in range(5):
        args=['collect']+(['--baseline'] if baseline else [])
        record=run(exe,config,args,base/f'run-{attempt}'); runs.append(record); baseline=False
        if record['output'].get('queued',0)==0: break
    coverage=summary(base/'data/archive/index.sqlite3',base)
    result={'read_workers':read,'parse_workers':parse,'runs':runs,'wall_seconds':sum(r['wall_seconds'] for r in runs),'peak_tree_rss_bytes':max(r['peak_tree_rss_bytes'] for r in runs),'peak_tree_commit_bytes':max(r['peak_tree_commit_bytes'] for r in runs),'coverage':coverage,'application_cache':'empty at initial run','os_file_cache':'not flushed; state uncontrolled, repeated reads may be warm'}
    save(complete,result); print(json.dumps({'case':base.name,'seconds':result['wall_seconds'],'files':coverage['files'],'statuses':coverage['statuses'],'peak_mib':round(result['peak_tree_commit_bytes']/1024**2,1)},ensure_ascii=False),flush=True)
    return result

def choose_files(files,old_csv):
    by_ext={}
    for path in files: by_ext.setdefault(path.suffix.lower(),[]).append(path)
    selected=set()
    for group in by_ext.values():
        group.sort(key=lambda p:p.stat().st_size)
        for q in [0,.25,.5,.75,.95,1]: selected.add(group[round((len(group)-1)*q)])
    by_name={p.name:p for p in files}
    manifest=json.loads((old_csv.parent/'corpus/manifest.json').read_text(encoding='utf-8-sig'))
    original_names={str(Path(item['original']['path'])).casefold():item['name'] for item in manifest['files']}
    with old_csv.open(encoding='utf-8-sig',newline='') as stream:
        slow=sorted(csv.DictReader(stream),key=lambda r:float(r.get('after_classify_parse_ms') or 0),reverse=True)
    for row in slow[:16]:
        name=original_names.get(str(Path(row['path'])).casefold(),Path(row['path']).name)
        if name in by_name: selected.add(by_name[name])
    return sorted(selected)

def copy_subset(files,source,destination):
    reject_frozen_output(destination)
    destination.mkdir(parents=True,exist_ok=True)
    for path in files:
        target=destination/path.relative_to(source); target.parent.mkdir(parents=True,exist_ok=True)
        if not target.exists(): shutil.copy2(path,target)

def percentile(values): return sorted(values)[math.ceil(.95*len(values))-1]

def reject_frozen_output(path):
    resolved=path.resolve()
    if any((parent/'.frozen-evidence.json').exists() for parent in (resolved,*resolved.parents)):
        raise RuntimeError('Frozen historical evidence is read-only; use a new benchmark output directory')

def incrementals(root,corpus,files,exe,scanner,read,parse):
    reject_frozen_output(root)
    base=root/'incremental'; work=base/'work'
    copy_subset(files,corpus,work)
    cold(base,work,exe,scanner,read,parse)
    cfg=base/'config'
    warm=base/'unchanged.json'
    if not warm.exists():
        run(exe,cfg,['collect'],base/'warmup')
        values=[run(exe,cfg,['collect'],base/f'unchanged-{i:02}') for i in range(20)]
        assert all(r['output']['bytes_read']==r['output']['parse_jobs']==r['output']['worker_starts']==r['output']['model_calls']==0 for r in values)
        assert all(r['output']['files']==r['output']['unchanged']==len(files) and r['output']['errors']==r['output']['queued']==0 for r in values)
        save(warm,{'runs':values,'median_seconds':statistics.median(r['wall_seconds'] for r in values),'p95_seconds':percentile([r['wall_seconds'] for r in values])})
    assert json.loads(warm.read_text(encoding='utf-8'))['p95_seconds']<=5,'unchanged P95 exceeds 5 seconds'
    # Representative physical changes retain format and parse coverage.
    parsed=json.loads((base/'all-files.json').read_text(encoding='utf-8'))
    save(base/'baseline-files.json',parsed)
    baseline_files={Path(row['path']).name:row for row in parsed}
    usable={Path(row['path']).name for row in parsed if row['status'] in ['complete','partial']}
    by_ext={}
    for f in files:
        if f.name in usable: by_ext.setdefault(f.suffix.lower(),[]).append(f)
    selected=[]
    counts={'.pdf':12,'.xlsx':7,'.docx':2,'.pptx':1,'.xls':1,'.md':1,'.txt':1,'.csv':1}
    for ext,count in counts.items():
        group=sorted(by_ext.get(ext,[]),key=lambda p:p.stat().st_size)
        selected.extend(group[round((len(group)-1)*(i+1)/(count+1))] for i in range(count))
    text_files=sorted((p for p in by_ext['.txt'] if p.stat().st_size>0 and baseline_files[p.name]['status']=='complete'),key=lambda p:p.stat().st_size)
    single=[text_files[len(text_files)//2]]
    for kind,modified in [('single',single),('one-percent',selected)]:
        result=base/f'{kind}.json'
        if result.exists(): continue
        values=[]
        for i in range(20):
            for source in modified:
                target=work/source.relative_to(corpus)
                temporary=target.with_name(target.name+'.benchmark-write')
                shutil.copy2(source,temporary)
                tag=f'archive-benchmark-{kind}-{i:02}'
                if source.suffix in ['.docx','.pptx','.xlsx']:
                    with zipfile.ZipFile(temporary,'a') as package: package.comment=tag.encode()
                else:
                    with temporary.open('ab') as stream:
                        if source.suffix=='.xls':
                            block=tag.encode();stream.write(block+b'\0'*(512-len(block)))
                        else:
                            stream.write((('\n% '+tag+'\n') if source.suffix=='.pdf' else ('\n'+tag+'\n')).encode())
                os.replace(temporary,target)
            measured=run(exe,cfg,['collect'],base/f'{kind}-{i:02}')
            assert measured['output']['versions']==len(modified),measured
            assert measured['output']['parse_jobs']==len(modified),measured
            assert measured['output']['errors']==measured['output']['queued']==0,measured
            con=sqlite3.connect(base/'data/archive/index.sqlite3')
            changed={Path(row[0]).resolve() for row in con.execute('SELECT path FROM file_metrics WHERE collection_id=? AND parse_ms IS NOT NULL',(measured['output']['run_id'],))}
            parsed_now=con.execute('SELECT f.path,e.status,e.manifest_json FROM file_metrics f JOIN version_extractions ve ON ve.version_id=f.version_id JOIN extractions e ON e.id=ve.extraction_id WHERE f.collection_id=? AND f.parse_ms IS NOT NULL',(measured['output']['run_id'],)).fetchall()
            con.close()
            assert changed=={(work/p.relative_to(corpus)).resolve() for p in modified}
            for path,status,manifest in parsed_now:
                previous=baseline_files[Path(path).name]
                if Path(path).suffix.lower() not in ['.txt','.md','.csv']:
                    assert status==previous['status'],('coverage changed',path,status,previous['status'])
                    actual=json.loads(manifest)
                    assert all(actual[key]==previous[key] for key in ['body_sha256','units_sha256','index_sha256','truncated','gaps','coverage']),('metadata-only mutation changed body',path)
                else:
                    assert status in ['complete','partial'],('text mutation failed',path,status)
            values.append(measured)
        save(result,{'modified_files':len(modified),'runs':values,'median_seconds':statistics.median(r['wall_seconds'] for r in values),'p95_seconds':percentile([r['wall_seconds'] for r in values])})
    summary(base/'data/archive/index.sqlite3',base)

def main():
    parser=argparse.ArgumentParser(); parser.add_argument('--release',required=True,type=Path); parser.add_argument('--corpus',required=True,type=Path); parser.add_argument('--output',required=True,type=Path); parser.add_argument('--reference',required=True,type=Path); parser.add_argument('--phase',choices=['sample','full','incremental','all'],default='all'); args=parser.parse_args()
    exe=args.release.resolve()/'ai-daily-report.exe'; corpus=args.corpus.resolve(); output=args.output.resolve()
    reject_frozen_output(output)
    output.mkdir(parents=True,exist_ok=True)
    files=sorted(p for p in corpus.rglob('*') if p.is_file() and p.relative_to(corpus).parts[0].startswith('batch-'))
    assert len(files)==2648,(len(files),'frozen corpus changed')
    assert sum(p.stat().st_size for p in files)==5420774249,'frozen corpus size changed'
    scanner=json.loads((args.reference/'scanner-settings.json').read_text(encoding='utf-8-sig'))['scanner']
    previous=output/'environment.json'
    if previous.exists():
        assert json.loads(previous.read_text(encoding='utf-8'))['binary_sha256']==hashlib.sha256(exe.read_bytes()).hexdigest(),'a new executable requires a separate benchmark output'
    save(output/'environment.json',{'files':len(files),'bytes':sum(p.stat().st_size for p in files),'binary_sha256':hashlib.sha256(exe.read_bytes()).hexdigest(),'os_cache':'not flushed','memory_budget_mib':3072,'os_reserve_mib':2048,'started_at':time.strftime('%Y-%m-%dT%H:%M:%S%z')})
    if args.phase in ['sample','all']:
        sample=choose_files(files,args.reference/'file-timings.csv'); sample_dir=output/'sample-corpus'; copy_subset(sample,corpus,sample_dir)
        combos=[(r,p) for r in [1,2] for p in range(1,6)]; random.Random(20261017).shuffle(combos)
        results=[cold(output/f'sample-r{r}-p{p}',sample_dir,exe,scanner,r,p) for r,p in combos]
        signature=next(r['coverage']['signature'] for r in results if r['read_workers']==r['parse_workers']==1)
        eligible=[r for r in results if r['coverage']['signature']==signature]
        eligible.sort(key=lambda r:r['wall_seconds']); assert len(eligible)>=2
        save(output/'sample-ranking.json',{'sample_files':len(sample),'results':results,'eligible':[[r['read_workers'],r['parse_workers']] for r in eligible],'rejected':[[r['read_workers'],r['parse_workers']] for r in results if r not in eligible]})
    ranking=json.loads((output/'sample-ranking.json').read_text(encoding='utf-8'))
    if args.phase in ['full','all']:
        full=[]
        for r,p in ranking['eligible'][:2]:
            runs=[cold(output/f'full-r{r}-p{p}-{i}',corpus,exe,scanner,r,p) for i in range(3)]
            assert len({v['coverage']['signature'] for v in runs})==1,'full run coverage changed'
            full.append({'read_workers':r,'parse_workers':p,'median_seconds':statistics.median(v['wall_seconds'] for v in runs),'peak_tree_commit_bytes':max(v['peak_tree_commit_bytes'] for v in runs),'signature':runs[0]['coverage']['signature'],'runs':runs})
        assert full[0]['signature']==full[1]['signature'],'parallel configurations changed content'
        full.sort(key=lambda r:r['median_seconds'])
        selected=min(full,key=lambda r:r['peak_tree_commit_bytes']) if full[1]['median_seconds']<=1.03*full[0]['median_seconds'] else full[0]
        save(output/'full-ranking.json',{'candidates':full,'selected':{'read_workers':selected['read_workers'],'parse_workers':selected['parse_workers'],'median_seconds':selected['median_seconds']},'same_content':True})
    if args.phase in ['incremental','all']:
        selected=json.loads((output/'full-ranking.json').read_text(encoding='utf-8'))['selected']
        incrementals(output,corpus,files,exe,scanner,selected['read_workers'],selected['parse_workers'])
    print('phase complete',args.phase,flush=True)
if __name__=='__main__': main()
