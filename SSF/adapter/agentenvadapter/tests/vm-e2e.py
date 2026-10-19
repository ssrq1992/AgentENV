#!/usr/bin/env python3
"""Real adapter -> real AgentENV -> guest -> CSI. No simulated success mode."""
import argparse, concurrent.futures, json, pathlib, sys, time, uuid
ROOT=pathlib.Path(__file__).resolve().parents[1];sys.path.insert(0,str(ROOT/'examples'))
from client import Client
p=argparse.ArgumentParser();p.add_argument('--url',required=True);p.add_argument('--ca',required=True);p.add_argument('--cert',required=True);p.add_argument('--key',required=True);p.add_argument('--namespace',default='agentenv-demo');p.add_argument('--class',dest='storage_class',default='ssf-block-immediate');p.add_argument('--image',required=True);p.add_argument('--template',required=True);p.add_argument('--existing-pvc');p.add_argument('--runs',type=int,default=3);p.add_argument('--concurrency',type=int,default=1);p.add_argument('--report',required=True);a=p.parse_args()
if a.existing_pvc and (a.runs!=1 or a.concurrency!=1):p.error('existing static PVC requires --runs 1 --concurrency 1')
samples=[];checks=[];started=time.time()
def scenario(index):
 c=Client(a.url,a.ca,a.cert,a.key);name='e2e-'+uuid.uuid4().hex[:20]
 if a.existing_pvc:
  name=a.existing_pvc;claim=c.call('GET',f'/v1/volumes/{a.namespace}/{name}')
 else:claim=c.call('POST','/v1/volumes',{'namespace':a.namespace,'name':name,'storage_class':a.storage_class,'bytes':1073741824,'initialize_ext4':True})
 uid=claim['metadata']['uid'];data=uuid.uuid4().hex
 rows=[]
 # Cold RW initializes; template RW verifies retained data; RO verifies both
 # slot/device enforcement and guest mount behavior after clean release.
 for kind,readonly in [('cold',False),('template',False),('cold',True),('template',True)]:
  launch={'image':a.image,'cpuCount':2,'memoryMB':512} if kind=='cold' else {'templateID':a.template}
  launch['timeout']=600
  q={'request_id':uuid.uuid4().hex,'kind':kind,'launch':launch,'mounts':[{'namespace':a.namespace,'pvc':name,'uid':uid,'id':'data','path':'/data','readonly':readonly}]}
  t=time.perf_counter();o=c.call('POST','/v1/sandboxes',q);id=o['id'];ready=c.wait(id);total=(time.perf_counter()-t)*1000
  repeat=c.call('POST','/v1/sandboxes',q);assert repeat['instance']==ready['instance'],'duplicate request changed VM instance'
  if kind=='cold' and not readonly:cmd=f"printf '%s' '{data}' > /data/retained; sync; cat /data/retained"
  else:cmd='cat /data/retained'
  code,out,err=c.execute(id,cmd);assert code==0 and out==data,(code,out,err)
  if readonly:
   code,_,_=c.execute(id,'echo forbidden > /data/readonly-must-fail');assert code!=0,'readonly write succeeded'
  c.call('DELETE','/v1/operations/'+id);c.wait(id,'Released')
  remaining=c.call('GET',f'/v1/volumes/{a.namespace}/{name}');assert remaining['metadata']['uid']==uid,'sandbox deletion removed PVC'
  rows.append({'kind':kind,'readonly':readonly,'overall_ms':total,**ready.get('timings_ms',{}),'operation':id,'sandbox_id':ready.get('sandbox_id'),'pvc':name,'pvc_uid':uid})
 return rows
report={'started_unix':started,'driver_class':a.storage_class,'real_vm':True,'status':'RUNNING','samples':samples}
try:
 with concurrent.futures.ThreadPoolExecutor(max_workers=a.concurrency) as pool:
  for rows in pool.map(scenario,range(a.runs)):samples.extend(rows)
 report['status']='PASS'
except Exception as e:
 report['status']='FAIL';report['error']=str(e);raise
finally:
 report['finished_unix']=time.time();report['percentiles']={}
 for kind in ('cold','template'):
  group=[s for s in samples if s['kind']==kind]
  metrics={k for s in group for k in s if k.endswith('_ms')}
  report['percentiles'][kind]={}
  for metric in metrics:
   values=sorted(s[metric] for s in group if metric in s)
   report['percentiles'][kind][metric]={f'p{q}':values[min(len(values)-1,round((len(values)-1)*q/100))] for q in (50,95,99)}
 pathlib.Path(a.report).write_text(json.dumps(report,indent=2))
