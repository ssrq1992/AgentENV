#!/usr/bin/env python3
"""Render independent per-worker workloads, never edit AgentENV deployment sources.
JSON Kubernetes List is accepted by kubectl without a YAML dependency.
"""
import argparse, copy, json, pathlib, re
ROOT = pathlib.Path(__file__).resolve().parents[1]
p = argparse.ArgumentParser()
p.add_argument('--control', default=str(ROOT/'config/control.example.json'))
p.add_argument('--namespace', default='agentenv-ssf')
p.add_argument('--agent-image', default='agentenv-ssf:0.2.3')
p.add_argument('--adapter-image', default='agentenvadapter:0.1.0')
p.add_argument('--ssf-image', default='ssf:0.1.0')
a = p.parse_args(); c = json.load(open(a.control)); ns = a.namespace
objects = []
def obj(kind, name, spec=None, api='v1', **extra):
    o={'apiVersion':api,'kind':kind,'metadata':{'name':name,'namespace':ns},**extra}
    if spec is not None: o['spec']=spec
    objects.append(o); return o

def config(name, data): obj('ConfigMap',name,data=data)
def mount(name,path,**kw): return {'name':name,'mountPath':path,**kw}
def secret(name,source=None): return {'name':name,'secret':{'secretName':source or name}}
def host(name,path): return {'name':name,'hostPath':{'path':path,'type':'DirectoryOrCreate'}}
def cm(name,source): return {'name':name,'configMap':{'name':source}}
def svc(name,port): obj('Service',name,{'selector':{'app':name},'ports':[{'name':'tls','port':port,'targetPort':port}]})
def workload(kind,name,containers,volumes,sa,**spec):
    pod={'serviceAccountName':sa,'containers':containers,'volumes':volumes,**spec}
    s={'selector':{'matchLabels':{'app':name}},'template':{'metadata':{'labels':{'app':name,'agentenv.ssf.dev/role':sa}},'spec':pod}}
    if kind=='Deployment': s['replicas']=2
    obj(kind,name,s,api='apps/v1')
def affinity(node): return {'nodeAffinity':{'requiredDuringSchedulingIgnoredDuringExecution':{'nodeSelectorTerms':[{'matchFields':[{'key':'metadata.name','operator':'In','values':[node]}]}]}}}
def health(port): return {'httpGet':{'path':'/readyz','port':port},'periodSeconds':5,'timeoutSeconds':3}
def container(name,role,config_name,tls_name):
    return {'name':name,'image':a.adapter_image,'args':[role,'-config','/config/config.json'],'ports':[{'containerPort':9444},{'containerPort':9091}], 'volumeMounts':[mount('config','/config',readOnly=True),mount('tls','/tls',readOnly=True),mount('signing','/signing',readOnly=True)],'readinessProbe':health(9091),'livenessProbe':{'httpGet':{'path':'/livez','port':9091}},'resources':{'requests':{'cpu':'100m','memory':'128Mi'},'limits':{'memory':'512Mi'}}}
obj('Namespace',ns)['metadata'].pop('namespace')
for namespace in sorted({n for p in c['principals'].values() for n in p['namespaces']}): obj('Namespace',namespace)['metadata'].pop('namespace')
for sa in ('agentenv-control','agentenv-node','ssf-node'): obj('ServiceAccount',sa)
for sa,role in [('agentenv-control','ssf-consumption'),('ssf-node','ssf-node'),('agentenv-node','agentenv-node-read')]:
    o=obj('ClusterRoleBinding',f'{ns}-{sa}',api='rbac.authorization.k8s.io/v1',roleRef={'apiGroup':'rbac.authorization.k8s.io','kind':'ClusterRole','name':role},subjects=[{'kind':'ServiceAccount','name':sa,'namespace':ns}]);o['metadata'].pop('namespace')
o=obj('ClusterRole','agentenv-node-read',api='rbac.authorization.k8s.io/v1',rules=[{'apiGroups':[''],'resources':['nodes'],'verbs':['get']}]);o['metadata'].pop('namespace')
config('agentenv-native',{'agentenv.toml':(ROOT/'config/agentenv.toml').read_text()})
# Fixed service per worker prevents routing a pinned launch to a different node.
for w in c['workers']:
    node=w['name']; slug=re.sub('[^a-z0-9-]','-',node.lower()); name='aenv-'+slug; ssf='ssf-'+name
    w['url']=f'https://{name}.{ns}.svc:9444'
    nc=json.load(open(ROOT/'config/node.example.json'));nc.update(node_name=node,ssf_endpoint=f'{ssf}.{ns}.svc:9443',evidence_peers=[f'spiffe://ssf/agentenv/ssf-node/{node}'])
    config(name,{'config.json':json.dumps(nc)})
    node_container=container('adapter','node',name,name+'-tls')
    node_container['securityContext']={'privileged':True,'runAsUser':0}
    node_container['volumeMounts'] += [mount('wal','/var/lib/agentenvadapter'),mount('runtime','/var/lib/agentenv-external'),mount('state','/var/lib/ssf',mountPropagation='HostToContainer'),mount('native-key','/native-key',readOnly=True),mount('dev','/dev')]
    native={'name':'agentenv','image':a.agent_image,'securityContext':{'privileged':True},'env':[
        {'name':'API_ADDR','value':'127.0.0.1:8000'}, {'name':'AENV_CONFIG_PATH','value':'/config/agentenv.toml'},
        {'name':'AENV_CUSTOM_EXTENSION_URL','value':'http://127.0.0.1:9445'}, {'name':'AENV_EXTERNAL_STORAGE_REQUIRED','value':'true'},
        {'name':'AENV_OBSERVABILITY_SCHEDULER_REPORT_ENABLED','value':'false'},
        {'name':'AENV_API_KEY','valueFrom':{'secretKeyRef':{'name':'agentenv-native-key','key':'AENV_API_KEY'}}}],
        'volumeMounts':[mount('workspace','/workspace'),mount('native-config','/config',readOnly=True),mount('runtime','/var/lib/agentenv-external'),mount('state','/var/lib/ssf',mountPropagation='HostToContainer'),mount('dev','/dev')],
        'resources':{'requests':{'cpu':str(w['capacity']['cpu']),'memory':str(w['capacity']['memory_mib'])+'Mi'}},
        'startupProbe':{'exec':{'command':['curl','-fsS','http://127.0.0.1:8000/health']},'periodSeconds':5,'failureThreshold':120}}
    vols=[cm('config',name),cm('native-config','agentenv-native'),secret('tls',name+'-tls'),secret('signing','agentenv-plan-public'),secret('native-key','agentenv-native-key'),host('wal','/var/lib/agentenvadapter'),host('runtime','/var/lib/agentenv-external'),host('state','/var/lib/ssf'),host('workspace','/var/lib/aenv'),host('dev','/dev')]
    workload('DaemonSet',name,[native,node_container],vols,'agentenv-node',shareProcessNamespace=True,affinity=affinity(node),terminationGracePeriodSeconds=120)
    svc(name,9444)
    sc={'Listen':':9443','HealthListen':':9090','Root':'/var/lib/ssf','NodeName':node,'PlanPublicKey':'/signing/plan.pub','Cert':'/tls/tls.crt','Key':'/tls/tls.key','CA':'/tls/ca.crt','AllowedPeers':[f'spiffe://ssf/agentenv/node/{node}'],'EvidenceEndpoint':f'https://{name}.{ns}.svc:9444/v1/evidence','Drivers':[{'Name':d['Name'],'NodeEndpoint':f"unix:///var/lib/kubelet/plugins/{d['Name']}/csi.sock",'Revision':d['Revision']} for d in c['drivers']]}
    config(ssf,{'config.json':json.dumps(sc)})
    sn={'name':'ssf-node','image':a.ssf_image,'command':['ssf-node'],'args':['-config','/config/config.json'],'securityContext':{'privileged':True},'ports':[{'containerPort':9443},{'containerPort':9090}],'livenessProbe':{'httpGet':{'path':'/livez','port':9090}},'volumeMounts':[mount('config','/config',readOnly=True),mount('tls','/tls',readOnly=True),mount('signing','/signing',readOnly=True),mount('state','/var/lib/ssf',mountPropagation='Bidirectional'),mount('plugins','/var/lib/kubelet/plugins',readOnly=True),mount('dev','/dev')]}
    workload('DaemonSet',ssf,[sn],[cm('config',ssf),secret('tls',ssf+'-tls'),secret('signing','agentenv-plan-public'),host('state','/var/lib/ssf'),host('plugins','/var/lib/kubelet/plugins'),host('dev','/dev')],'ssf-node',affinity=affinity(node))
    svc(ssf,9443)
config('agentenv-control',{'config.json':json.dumps(c)})
ctrl=container('control','control','agentenv-control','agentenv-control-tls')
ctrl['env']=[{'name':'SSF_DATABASE_URL','valueFrom':{'secretKeyRef':{'name':'agentenv-ledger','key':'url'}}}]
ctrl['securityContext']={'runAsUser':65532,'runAsNonRoot':True,'allowPrivilegeEscalation':False,'readOnlyRootFilesystem':True}
vols=[cm('config','agentenv-control'),secret('tls','agentenv-control-tls'),secret('signing','agentenv-plan-private')]
workload('Deployment','agentenv-control',[ctrl],vols,'agentenv-control')
svc('agentenv-control',9444)
migrate=copy.deepcopy(ctrl);migrate['name']='migrate';migrate['args']=['ctl','-config','/config/config.json','-action','migrate']
for k in ('livenessProbe','readinessProbe','ports'):migrate.pop(k,None)
obj('Job','agentenv-migrate',{'template':{'spec':{'serviceAccountName':'agentenv-control','restartPolicy':'OnFailure','containers':[migrate],'volumes':vols}},'backoffLimit':3},api='batch/v1')
# Native API and hook bind only loopback. mTLS identity protects adapter RPCs;
# network policy also limits worker ingress to the control and SSF services.
obj('NetworkPolicy','agentenv-workers',{'podSelector':{'matchLabels':{'agentenv.ssf.dev/role':'agentenv-node'}},'policyTypes':['Ingress'],'ingress':[{'from':[{'podSelector':{'matchLabels':{'agentenv.ssf.dev/role':role}}} for role in ('agentenv-control','ssf-node')],'ports':[{'protocol':'TCP','port':9444}]}]},api='networking.k8s.io/v1')
print(json.dumps({'apiVersion':'v1','kind':'List','items':objects},indent=2))
