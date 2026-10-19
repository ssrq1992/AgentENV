#!/usr/bin/env python3
"""Read-only Kubernetes evidence collection; no Secrets or ConfigMap contents.

Collect before and after fault injection. Logs can still contain sensitive data;
the output directory is private. Failed reads are recorded, never counted PASS.
"""
import argparse
import json
import os
import pathlib
import subprocess
import time


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--context', required=True)
    p.add_argument('--namespace', default='agentenv-demo')
    p.add_argument('--app-namespace', default='agentenv-ssf')
    p.add_argument('--driver-namespace', default='ssf-fixture')
    p.add_argument('--out', required=True, help='new directory')
    p.add_argument('--since', default='20m')
    p.add_argument('--run-report', help='suite report.json; filters PVC/PV objects to recorded UIDs/names')
    a = p.parse_args(); os.umask(0o077)
    out = pathlib.Path(a.out); out.mkdir(mode=0o700, parents=True, exist_ok=False)
    index = {'started_unix': time.time(), 'context': a.context, 'commands': [], 'status': 'COLLECTING'}

    def command(name, args, decode=False, optional=False):
        cmd = ['kubectl', '--context', a.context, '--request-timeout=30s', *args]
        try:
            r = subprocess.run(cmd, capture_output=True, text=True, timeout=45)
            code, stdout, stderr = r.returncode, r.stdout, r.stderr
        except (OSError, subprocess.TimeoutExpired) as e:
            code, stdout, stderr = -1, '', str(e)
        index['commands'].append({'file': name, 'args': args, 'exit_code': code, 'optional': optional})
        (out / (name+'.stderr.txt')).write_text(stderr)
        if code == 0 and decode:
            return json.loads(stdout)
        (out / name).write_text(stdout)
        return None

    command('version.json', ['version', '-o', 'json'])
    command('nodes.json', ['get', 'nodes', '-o', 'json'])
    command('csinodes.json', ['get', 'csinodes', '-o', 'json'])
    command('classes.json', ['get', 'storageclasses', '-o', 'json'])
    command('guard-webhook.json', ['get', 'validatingwebhookconfiguration', 'ssf-guard', '-o', 'json'])
    tracked = json.loads(pathlib.Path(a.run_report).read_text())['volumes'] if a.run_report else None
    claims = command('pvcs.json', ['get', 'pvc', '-n', a.namespace, '-o', 'json'], decode=True)
    pvs = command('pvs.json', ['get', 'pv', '-o', 'json'], decode=True)
    if claims is not None:
        if tracked is not None:
            names = {(v['namespace'], v['name']) for v in tracked}
            claims['items'] = [c for c in claims['items'] if (c['metadata']['namespace'], c['metadata']['name']) in names]
        (out/'pvcs.json').write_text(json.dumps(claims, indent=2))
    if pvs is not None:
        if tracked is not None:
            uids = {v['uid'] for v in tracked if v.get('uid')}
            names = {v['pv'] for v in tracked if v.get('pv')}
            pvs['items'] = [v for v in pvs['items'] if v['metadata']['name'] in names or v['spec'].get('claimRef', {}).get('uid') in uids]
        (out/'pvs.json').write_text(json.dumps(pvs, indent=2))
    for ns in dict.fromkeys([a.namespace, a.app_namespace, 'ssf-system', a.driver_namespace]):
        pods = command(ns+'-pods.json', ['get', 'pods', '-n', ns, '-o', 'json'], decode=True)
        command(ns+'-events.json', ['get', 'events', '-n', ns, '-o', 'json'])
        if pods is None: continue
        sanitized = []
        for pod in pods['items']:
            name = pod['metadata']['name']
            sanitized.append({'metadata': {k: pod['metadata'].get(k) for k in ('name','namespace','uid','creationTimestamp','labels')},
                'node': pod['spec'].get('nodeName'), 'shareProcessNamespace': pod['spec'].get('shareProcessNamespace'),
                'containers': [{'name': c['name'], 'image': c['image']} for c in pod['spec']['containers']],
                'status': pod.get('status')})
            if ns == a.namespace: continue  # do not collect unrelated tenant application logs
            for c in pod['spec']['containers']:
                base = f"{ns}-{name}-{c['name']}"
                command(base+'.log', ['logs', '-n', ns, name, '-c', c['name'], '--timestamps', '--since='+a.since])
                command(base+'.previous.log', ['logs', '-n', ns, name, '-c', c['name'], '--timestamps', '--previous', '--since='+a.since], optional=True)
        (out/(ns+'-pods.json')).write_text(json.dumps(sanitized, indent=2))
    index['status'] = 'COLLECTED' if all(c['exit_code']==0 or c['optional'] for c in index['commands']) else 'PARTIAL'
    index['finished_unix'] = time.time(); (out/'index.json').write_text(json.dumps(index, indent=2))
    print(index['status']+' (evidence collection, not test PASS)')
    return 0 if index['status'] == 'COLLECTED' else 1


if __name__ == '__main__':
    raise SystemExit(main())
