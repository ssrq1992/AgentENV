#!/usr/bin/env python3
"""Generate disposable test PKI; never overwrites an existing directory."""
import argparse, base64, json, os, pathlib, re, secrets, subprocess
p=argparse.ArgumentParser();p.add_argument('--control',required=True);p.add_argument('--out',required=True);p.add_argument('--namespace',default='agentenv-ssf');a=p.parse_args()
r=pathlib.Path(a.out);r.mkdir(mode=0o700);os.umask(0o077)
c=json.load(open(a.control));ns=a.namespace;objects=[]
def run(*cmd):subprocess.run(cmd,check=True,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
def secret(name,data):objects.append({'apiVersion':'v1','kind':'Secret','metadata':{'name':name,'namespace':ns},'type':'Opaque','data':{k:base64.b64encode(v).decode() for k,v in data.items()}})
run('openssl','req','-x509','-newkey','rsa:2048','-nodes','-keyout',str(r/'ca.key'),'-out',str(r/'ca.crt'),'-days','7','-subj','/CN=SSF disposable test CA')
roles=[('agentenv-control','spiffe://ssf/agentenv/control'),('user','spiffe://ssf/agentenv/user/demo')]
for w in c['workers']:
 slug=re.sub('[^a-z0-9-]','-',w['name'].lower());roles += [('aenv-'+slug,'spiffe://ssf/agentenv/node/'+w['name']),('ssf-aenv-'+slug,'spiffe://ssf/agentenv/ssf-node/'+w['name'])]
for name,uri in roles:
 ext=r/(name+'.ext');ext.write_text(f'subjectAltName=DNS:{name}.{ns}.svc,DNS:localhost,IP:127.0.0.1,URI:{uri}\nextendedKeyUsage=serverAuth,clientAuth\n')
 run('openssl','req','-newkey','rsa:2048','-nodes','-keyout',str(r/(name+'.key')),'-out',str(r/(name+'.csr')),'-subj','/CN='+name)
 run('openssl','x509','-req','-in',str(r/(name+'.csr')),'-CA',str(r/'ca.crt'),'-CAkey',str(r/'ca.key'),'-set_serial',str(secrets.randbits(128)),'-out',str(r/(name+'.crt')),'-days','7','-extfile',str(ext))
 if name!='user':secret(name+'-tls',{'tls.crt':(r/(name+'.crt')).read_bytes(),'tls.key':(r/(name+'.key')).read_bytes(),'ca.crt':(r/'ca.crt').read_bytes()})
run('openssl','genpkey','-algorithm','ED25519','-out',str(r/'plan.pem'))
run('openssl','pkey','-in',str(r/'plan.pem'),'-outform','DER','-out',str(r/'plan.der'))
run('openssl','pkey','-in',str(r/'plan.pem'),'-pubout','-outform','DER','-out',str(r/'plan.pub.der'))
pub=(r/'plan.pub.der').read_bytes()[-32:];seed=(r/'plan.der').read_bytes()[-32:]
secret('agentenv-plan-private',{'plan.key':base64.b64encode(seed+pub)})
secret('agentenv-plan-public',{'plan.pub':base64.b64encode(pub)})
secret('agentenv-native-key',{'AENV_API_KEY':secrets.token_hex(32).encode()})
(r/'secrets.json').write_text(json.dumps({'apiVersion':'v1','kind':'List','items':objects},indent=2))
print('Test credentials generated. SSF Guard/Gateway must trust the same CA; do not print or commit secrets.json.')
