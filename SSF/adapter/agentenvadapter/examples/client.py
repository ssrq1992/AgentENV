#!/usr/bin/env python3
"""Standard-library mTLS client, including envd Connect JSON stream decoding."""
import argparse, base64, json, ssl, struct, sys, time, urllib.request
class Client:
    def __init__(self,url,ca,cert,key):
        self.url=url.rstrip('/');self.tls=ssl.create_default_context(cafile=ca);self.tls.load_cert_chain(cert,key)
    def call(self,method,path,body=None):
        req=urllib.request.Request(self.url+path,data=None if body is None else json.dumps(body).encode(),method=method,headers={'Content-Type':'application/json'})
        with urllib.request.urlopen(req,context=self.tls,timeout=120) as r:return json.load(r)
    def wait(self,id,state='Ready',timeout=300):
        until=time.monotonic()+timeout
        while time.monotonic()<until:
            o=self.call('GET','/v1/operations/'+id)
            if o['state']==state:return o
            if state=='Ready' and (o['state']=='Released' or o['stopping']):raise RuntimeError({k:o.get(k) for k in ('id','state','reason','stopping')})
            time.sleep(.05)
        raise TimeoutError(f'operation {id} still pending; resources may be in use')
    def execute(self,id,command):
        payload=json.dumps({'process':{'cmd':'/bin/sh','args':['-c',command],'cwd':'/'},'stdin':False}).encode()
        req=urllib.request.Request(self.url+f'/v1/sandboxes/{id}/guest/process.Process/Start',data=b'\0'+struct.pack('>I',len(payload))+payload,headers={'Content-Type':'application/connect+json','Connect-Protocol-Version':'1','Authorization':'Basic '+base64.b64encode(b'root:').decode()})
        code=None;stdout=b'';stderr=b''
        with urllib.request.urlopen(req,context=self.tls,timeout=120) as response:
            def exact(n):
                b=b''
                while len(b)<n:
                    chunk=response.read(n-len(b))
                    if not chunk:raise EOFError('incomplete Connect stream')
                    b+=chunk
                return b
            while True:
                h=response.read(1)
                if not h:break
                flags=h[0];length=struct.unpack('>I',exact(4))[0]
                if length>16*1024*1024:raise ValueError('oversized envelope')
                msg=json.loads(exact(length))
                if flags&1:raise ValueError('compressed envelope not negotiated')
                if flags&2:
                    if msg.get('error'):raise RuntimeError(msg['error'])
                    break
                event=msg.get('event',{});data=event.get('data',{})
                stdout+=base64.b64decode(data.get('stdout',''));stderr+=base64.b64decode(data.get('stderr',''))
                if 'end' in event:code=int(event['end'].get('exitCode',0))
        if code is None:raise RuntimeError('no guest process exit evidence')
        return code,stdout.decode(errors='replace'),stderr.decode(errors='replace')
def main():
    p=argparse.ArgumentParser();p.add_argument('--url',required=True);p.add_argument('--ca',required=True);p.add_argument('--cert',required=True);p.add_argument('--key',required=True)
    p.add_argument('action',choices=['create','inspect','delete','volume','exec']);p.add_argument('value');p.add_argument('command',nargs='?');a=p.parse_args();c=Client(a.url,a.ca,a.cert,a.key)
    if a.action in ('create','volume'):out=c.call('POST','/v1/'+('sandboxes' if a.action=='create' else 'volumes'),json.load(open(a.value)))
    elif a.action=='exec':
        code,out,err=c.execute(a.value,a.command);sys.stdout.write(out);sys.stderr.write(err);return code
    else:out=c.call('GET' if a.action=='inspect' else 'DELETE','/v1/operations/'+a.value)
    print(json.dumps(out,indent=2));return 0
if __name__=='__main__':sys.exit(main())
