#!/usr/bin/env python3
import hashlib, json, pathlib, subprocess, tarfile, tempfile, io
root=pathlib.Path(__file__).resolve().parents[4];agent=root/'AgentENV';out=pathlib.Path(__file__).resolve().parents[1]/'patches'
base='6cccaa7842bd5be2051111d4f74d9e37aa721244'
files=['src/cfg.rs','src/orchestrator/service.rs','src/sandbox/custom_extension/mod.rs','src/sandbox/custom_extension/storage.rs','src/sandbox/firecracker/config.rs','src/sandbox/firecracker/sandbox.rs','src/sandbox/manifest.rs']
def git(*args,check=True):return subprocess.run(['git','-C',str(agent),*args],check=check,stdout=subprocess.PIPE).stdout
assert git('rev-parse','HEAD').decode().strip()==base,'unexpected source baseline'
tracked=[f for f in files if f!='src/sandbox/custom_extension/storage.rs']
patch=git('diff','--binary','--',*tracked)+git('diff','--no-index','--','/dev/null','src/sandbox/custom_extension/storage.rs',check=False)
out.mkdir(exist_ok=True);(out/'agentenv-v0.2.3-external-storage.patch').write_bytes(patch)
manifest={'baseline':base,'tag':'v0.2.3','license':'MIT (AgentENV)','patch_sha256':hashlib.sha256(patch).hexdigest(),'files':{f:hashlib.sha256((agent/f).read_bytes()).hexdigest() for f in files}}
with tempfile.TemporaryDirectory(prefix='agentenv-patch-check-') as tmp:
 with tarfile.open(fileobj=io.BytesIO(git('archive',base))) as archive:archive.extractall(tmp,filter='data')
 subprocess.run(['git','apply','--check',str(out/'agentenv-v0.2.3-external-storage.patch')],cwd=tmp,check=True)
 subprocess.run(['git','apply',str(out/'agentenv-v0.2.3-external-storage.patch')],cwd=tmp,check=True)
 for f,digest in manifest['files'].items():assert hashlib.sha256((pathlib.Path(tmp)/f).read_bytes()).hexdigest()==digest,f
manifest['replay_check']='PASS: clean baseline archive -> apply -> all modified file hashes equal'
(out/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
(out/'AgentENV-LICENSE').write_bytes((agent/'LICENSE').read_bytes())
print(manifest['replay_check'])
