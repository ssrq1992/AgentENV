#!/usr/bin/env python3
import hashlib,json,pathlib,re,subprocess
root=pathlib.Path(__file__).resolve().parents[4];adapter=root/'SSF/adapter/agentenvadapter';errors=[]
for file in [root/'design/12-agentenvadapter-design-and-implementation.md',*adapter.rglob('*.md')]:
 for link in re.findall(r'\]\(([^)]+)\)',file.read_text()):
  link=link.split('#')[0]
  if not link or re.match(r'\w+://',link):continue
  if not (file.parent/link).exists():errors.append(f'{file.relative_to(root)}: missing {link}')
for f in (root/'SSF').rglob('*.go'):
 if 'adapter' in f.relative_to(root/'SSF').parts:continue
 if 'ssf.dev/ssf/adapter/' in f.read_text():errors.append('core imports adapter: '+str(f))
expected='9733935a2c0e2d144b67c8404361c835e7f8ed45d2b508992cb230da31b3d5dc'
assert hashlib.sha256((root/'design/assets/K8S-CSI架构分析.md').read_bytes()).hexdigest()==expected,'protected document changed'
for f in adapter.rglob('*.json'):
 if '.generated' not in f.parts:json.loads(f.read_text())
for f in adapter.rglob('*.sh'):subprocess.run(['bash','-n',str(f)],check=True)
report={'status':'FAIL' if errors else 'PASS','errors':errors,'checks':['relative Markdown links','core dependency boundary','protected CSI document SHA256','JSON parsing','bash syntax']}
(adapter/'validation/artifacts.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report));raise SystemExit(bool(errors))
