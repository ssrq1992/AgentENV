#!/usr/bin/env python3
"""Optional parser check; NOT a substitute for cargo check/test on Linux.
Requires tree-sitter==0.25.2, tree-sitter-rust==0.24.0 on PYTHONPATH.
Compare inherited grammar errors with the unchanged Git baseline.
"""
import json,pathlib,subprocess,sys
import tree_sitter,tree_sitter_rust
root=pathlib.Path(__file__).resolve().parents[4];a=root/'AgentENV';adapter=pathlib.Path(__file__).resolve().parents[1]
manifest=json.load(open(adapter/'patches/manifest.json'));parser=tree_sitter.Parser(tree_sitter.Language(tree_sitter_rust.language()));out={};failed=False
for f in manifest['files']:
 def errors(data):
  found=[]
  def walk(n):
   if n.type=='ERROR' or n.is_missing:found.append({'text':data[n.start_byte:n.end_byte].decode(),'line':n.start_point.row+1,'type':n.type})
   for c in n.children:walk(c)
  walk(parser.parse(data).root_node);return found
 base=subprocess.run(['git','-C',str(a),'show',manifest['baseline']+':'+f],capture_output=True)
 current=errors((a/f).read_bytes());old=errors(base.stdout) if base.returncode==0 else []
 expected=[(e['text'],e['type']) for e in old];actual=[(e['text'],e['type']) for e in current]
 new=actual!=expected;failed|=new;out[f]={'errors':current,'baseline_errors':old,'new_errors':new}
report={'parser':'tree-sitter 0.25.2 / rust 0.24.0','type_check':False,'status':'FAIL' if failed else 'PASS compared with baseline','files':out}
(adapter/'validation/rust-syntax.json').write_text(json.dumps(report,indent=2)+'\n');print(report['status']);sys.exit(int(failed))
