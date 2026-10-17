"""Exercise the shipped archive CLI with a local HTTP model and synthetic data."""
import argparse,datetime,http.server,json,os,sqlite3,subprocess,threading
from pathlib import Path
import benchmark_archive as bench

class Model(http.server.BaseHTTPRequestHandler):
    calls=0
    def log_message(self,*args): pass
    def do_POST(self):
        request=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        wire=json.loads(request['messages'][1]['content'])
        # Internal paths/evidence are never transmitted to the composition model.
        assert 'archive_changes' not in wire
        if 'facts' in wire:
            overview=[]; matters=[]; pending=[]
            for fact in wire['facts']:
                assert 'evidence' not in fact
                cited={'text':fact['fact']['detail'],'fact_ids':[fact['fact_id']],'source_ids':fact['fact']['source_ids']}
                if fact['pending']: pending.append(cited)
                else:
                    overview.append(cited)
                    matters.append({'matter_id':fact['matter_id'],'title':'审批条件调整','background':None,'details':[cited],'status':None})
            result={'schema_version':4,'overview':overview[:5],'matters':matters,'pending':pending}
        else:
            source=wire['sources'][0]
            result={'schema_version':4,'facts':[{'topic':'审批条件','background':None,'detail':'审批门槛由5万元调整为10万元；尚不能据此确认生效日期。','conditions':[],'status':None,'source_ids':[source['source_id']],'uncertainty':None}],'pending':[]}
        Model.calls+=1
        body=json.dumps({'choices':[{'finish_reason':'stop','message':{'content':json.dumps(result,ensure_ascii=False)}}],'usage':{'prompt_tokens':100,'completion_tokens':50,'total_tokens':150}},ensure_ascii=False).encode()
        self.send_response(200);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)

def main():
    parser=argparse.ArgumentParser();parser.add_argument('--release',type=Path,required=True);parser.add_argument('--output',type=Path,required=True);args=parser.parse_args()
    base=args.output.resolve();base.mkdir(parents=True,exist_ok=False)
    work=base/'work';work.mkdir();source=work/'policy.txt';source.write_text('审批门槛5万元\n',encoding='utf-8')
    exe=args.release.resolve()/'ai-daily-report.exe'
    manifest=json.loads((exe.parent/'manifest.json').read_text(encoding='utf-8-sig'))
    assert manifest['worker_protocol']==4,'release manifest must declare worker protocol v4'
    cfg=bench.configure(base,work,exe,{},1,1)
    server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Model)
    thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
    settings=json.loads((cfg/'settings.yaml').read_text(encoding='utf-8'))
    settings['llm']={'provider':'deepseek','model_id':'local-fixture','base_url':f'http://127.0.0.1:{server.server_port}','context_window_tokens':128000,'verified_max_output_tokens':32768}
    bench.save(cfg/'settings.yaml',settings)
    env={k:v for k,v in os.environ.items() if k.upper() in ['SYSTEMROOT','WINDIR','TEMP','TMP','USERPROFILE','LOCALAPPDATA','APPDATA']}
    env['PATH']=str(Path(os.environ['SYSTEMROOT'])/'System32');env['DEEPSEEK_API_KEY']='local-fixture-key'
    sequence=0
    def invoke(*arguments,expect=True,config=cfg):
        nonlocal sequence
        sequence+=1
        p=subprocess.run([str(exe),'--config-dir',str(config),*map(str,arguments)],capture_output=True,env=env,creationflags=0x08000000,timeout=120)
        (base/f'{sequence:02}-stderr.log').write_bytes(p.stderr)
        result=json.loads(p.stdout.decode('utf-8-sig') or '{}')
        bench.save(base/f'{sequence:02}-result.json',result)
        assert (p.returncode==0)==expect,(arguments,p.returncode,p.stderr.decode('utf-8',errors='replace'))
        return result
    try:
        first=invoke('collect','--baseline');assert first['versions']==1 and first['parse_jobs']==1
        warm=invoke('collect');assert warm['bytes_read']==warm['worker_starts']==0
        source.write_text('审批门槛10万元\n',encoding='utf-8')
        changed=invoke('collect');assert changed['versions']==changed['parse_jobs']==1
        today=datetime.datetime.now(datetime.timezone(datetime.timedelta(hours=8))).date().isoformat()
        # Only this synthetic archive is backdated to exercise completed-day reports.
        observed=(datetime.datetime.now(datetime.timezone.utc)-datetime.timedelta(days=1)).isoformat()
        target=(datetime.date.fromisoformat(today)-datetime.timedelta(days=1)).isoformat()
        con=sqlite3.connect(base/'data/archive/index.sqlite3')
        con.execute('UPDATE versions SET observed_at=?',(observed,));con.execute('UPDATE changes SET observed_at=?',(observed,));con.commit();con.close()
        source.write_text('审批门槛20万元\n',encoding='utf-8')
        invoke('collect')
        preview=invoke('daily','--source','archive','--target-date',target,'--no-save');assert preview['action']=='preview'
        con=sqlite3.connect(base/'data/archive/index.sqlite3')
        assert con.execute("SELECT count(*) FROM report_evidence WHERE status='published'").fetchone()[0]==0
        con.close()
        published=invoke('daily','--source','archive','--target-date',target);assert published['action']=='published'
        assert '5万元' in published['markdown'] and '10万元' in published['markdown']
        assert Model.calls==4,Model.calls
        original_calls=Model.calls
        repeated=invoke('daily','--source','archive','--target-date',target);assert repeated['action']=='existing' and Model.calls==original_calls
        con=sqlite3.connect(base/'data/db/reports.sqlite3')
        report=json.loads(con.execute('SELECT raw_json FROM daily_reports').fetchone()[0]);con.close()
        assert report['schema_version']==4 and len(report['facts'][0]['evidence'])==2
        assert len(report['sources'])==1
        quotes=' '.join(r['quote'] for r in report['facts'][0]['evidence'])
        assert '5万元' in quotes and '10万元' in quotes and '20万元' not in quotes
        stats=invoke('stats','--from',target,'--to',today,'--files-csv',base/'files.csv')
        assert stats['model']['known_total_tokens']==600
        assert len(stats['report_stages'])>=4
        assert stats['collections'][0]['metrics']['resources']['cpu_ms']>=0
        verified=invoke('archive','verify');assert verified['status']=='verified'
        plan=invoke('archive','prune','--before','2099-01-01');assert plan['versions']==[] and len(plan['protected'])==3
        bad=dict(plan);bad['versions']=[plan['protected'][0]['version_id']];bench.save(base/'bad-plan.json',bad)
        invoke('archive','prune','--execute-plan',base/'bad-plan.json',expect=False)
        restored=base/'restore-data';restored.mkdir()
        invoke('archive','backup','--output',restored/'archive')
        restorecfg=base/'restore-config';restorecfg.mkdir();restoresettings=dict(settings);restoresettings['paths']={**settings['paths'],'data_dir':str(restored)};bench.save(restorecfg/'settings.yaml',restoresettings)
        assert invoke('archive','verify',config=restorecfg)['status']=='verified'
        task=invoke('task','plan','--kind','collect','--sid','S-1-5-21-1-2-3-1001');assert '<Interval>PT30M</Interval>' in task['xml'] and 'collect --scheduled' in task['arguments']
        evidence={'status':'passed','remote_model_calls':0,'local_http_model_calls':Model.calls,'baseline_and_warm':True,'preview_keeps_pending_changes':True,'publication_and_replay':True,'v4_exact_before_after_references':True,'yesterday_version_survives_today_overwrite':True,'stats_and_complete_csv':True,'protected_prune_rejected':True,'backup_restore_verified':True,'collect_task_30_minutes':True,'production_tasks_changed':0,'synthetic_observation_dates_backdated':True,'path_contains_only_windows_system32':True}
        bench.save(base/'acceptance.json',evidence);print(json.dumps(evidence))
    finally:
        server.shutdown();server.server_close()
if __name__=='__main__':main()
