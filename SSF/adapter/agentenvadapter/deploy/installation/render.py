#!/usr/bin/env python3
"""Render the single-worker installation guide's manifests; never contact a cluster.

This is an installation overlay, not a production CSI/PostgreSQL operator.
Secrets, webhook caBundle and the existing cluster are supplied separately.
"""
import argparse
import copy
import json
import pathlib
import subprocess
import sys

ADAPTER = pathlib.Path(__file__).resolve().parents[2]
SSF = pathlib.Path(__file__).resolve().parents[4]


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--control', required=True)
    p.add_argument('--base-json', required=True,
                   help='kubectl create --dry-run=client -f SSF/deploy/base.yaml -o json')
    p.add_argument('--out', required=True)
    p.add_argument('--agent-image', required=True)
    p.add_argument('--adapter-image', required=True)
    p.add_argument('--ssf-image', required=True)
    p.add_argument('--test-image', required=True)
    a = p.parse_args()
    cfg = json.loads(pathlib.Path(a.control).read_text())
    if len(cfg['workers']) != 1:
        p.error('the local block fixture and template repository require exactly one worker')
    if cfg.get('database_url'):
        p.error('keep database credentials in Secrets, not the control JSON')
    node = cfg['workers'][0]['name']
    out = pathlib.Path(a.out)
    out.mkdir(parents=True, exist_ok=True)

    def write(name, items):
        (out / (name + '.json')).write_text(json.dumps(
            {'apiVersion': 'v1', 'kind': 'List', 'items': items}, indent=2) + '\n')

    def resource(kind, name, ns, **fields):
        meta = {'name': name}
        if ns:
            meta['namespace'] = ns
        return {'apiVersion': 'v1', 'kind': kind, 'metadata': meta, **fields}

    def cm(name, ns, content):
        return resource('ConfigMap', name, ns, data={'config.json': json.dumps(content)})

    def volume(name, path):
        return {'name': name, 'hostPath': {'path': path, 'type': 'DirectoryOrCreate'}}

    def seed():
        return {'name': 'seed-workspace', 'image': a.agent_image,
                'command': ['/bin/sh', '-ec', 'cp -an /workspace/. /seed/'],
                'securityContext': {'runAsUser': 0},
                'volumeMounts': [{'name': 'workspace', 'mountPath': '/seed'}]}

    app = json.loads(subprocess.check_output([
        sys.executable, str(ADAPTER / 'scripts/render.py'), '--control', a.control,
        '--agent-image', a.agent_image, '--adapter-image', a.adapter_image,
        '--ssf-image', a.ssf_image]))['items']
    # kubectl printers can emit a List or a stream of individual JSON objects.
    raw = pathlib.Path(a.base_json).read_text().strip()
    base = []
    decoder = json.JSONDecoder()
    while raw:
        doc, end = decoder.raw_decode(raw)
        base.extend(doc['items'] if doc.get('kind') == 'List' else [doc])
        raw = raw[end:].lstrip()
    if not any(o['kind'] == 'ClusterRole' and o['metadata']['name'] == 'ssf-node' for o in base):
        p.error('--base-json must contain the SSF base ClusterRole resources')

    template_config = ('\n[snapshot]\nrepository_backend = "posix_fs"\np2p_enabled = false\n'
                       '[backend.posix_fs]\nsnapshot_store = "/templates"\n')
    for o in app:
        if o['kind'] == 'ConfigMap' and o['metadata']['name'] == 'agentenv-native':
            o['data']['agentenv.toml'] += template_config
        if o['kind'] != 'DaemonSet':
            continue
        spec = o['spec']['template']['spec']
        for c in spec['containers']:
            if c['name'] == 'ssf-node':
                # The shared SSF image defaults to UID 65532; real mounts need root.
                c['securityContext']['runAsUser'] = 0
            if c['name'] == 'agentenv':
                spec['initContainers'] = [seed()]
                spec['volumes'].append(volume('templates', '/var/lib/aenv-templates'))
                c['volumeMounts'].append({'name': 'templates', 'mountPath': '/templates'})
        # Applying configuration must not automatically replace a live VMM Pod.
        o['spec']['updateStrategy'] = {'type': 'OnDelete'}

    namespaces = [o for o in app if o['kind'] == 'Namespace']
    namespaces += [resource('Namespace', ns, None) for ns in ('ssf-system', 'ssf-fixture')]
    for o in namespaces:
        if o['metadata']['name'] in ('agentenv-ssf', 'ssf-fixture', 'ssf-system'):
            o['metadata']['labels'] = {'pod-security.kubernetes.io/enforce': 'privileged'}
    write('00-namespaces', namespaces)
    roles = {'ClusterRole', 'ClusterRoleBinding', 'ServiceAccount'}
    write('01-rbac', [o for o in base + app if o['kind'] in roles])
    write('03-app-config', [o for o in app if o['kind'] in ('ConfigMap', 'Service', 'NetworkPolicy')])
    write('04-migration', [o for o in app if o['kind'] == 'Job'])

    pg_spec = {
        'replicas': 1, 'strategy': {'type': 'Recreate'},
        'selector': {'matchLabels': {'app': 'ssf-postgres'}},
        'template': {'metadata': {'labels': {'app': 'ssf-postgres'}}, 'spec': {
            'nodeName': node,
            'containers': [{'name': 'postgres', 'image': 'postgres:17.4',
                'env': [{'name': 'POSTGRES_DB', 'value': 'ssf'},
                        {'name': 'POSTGRES_PASSWORD', 'valueFrom': {'secretKeyRef': {
                            'name': 'ssf-postgres', 'key': 'password'}}}],
                'ports': [{'containerPort': 5432}],
                'readinessProbe': {'exec': {'command': ['pg_isready', '-U', 'postgres', '-d', 'ssf']}},
                'resources': {'requests': {'cpu': '200m', 'memory': '256Mi'}},
                'volumeMounts': [{'name': 'data', 'mountPath': '/var/lib/postgresql/data'}]}],
            'volumes': [volume('data', '/var/lib/ssf-postgres')]}}}
    pg = resource('Deployment', 'ssf-postgres', 'ssf-system', spec=pg_spec)
    pg['apiVersion'] = 'apps/v1'
    write('02-postgres-test-only', [pg, resource('Service', 'ssf-postgres', 'ssf-system', spec={
        'selector': {'app': 'ssf-postgres'}, 'ports': [{'port': 5432, 'targetPort': 5432}]})])

    guard = [copy.deepcopy(o) for o in base if o['kind'] in ('Deployment', 'Service')
             and o['metadata']['name'] == 'ssf-guard']
    for o in guard:
        if o['kind'] == 'Deployment':
            o['spec']['template']['spec']['containers'][0]['image'] = a.ssf_image
    guard_cfg = json.loads((SSF / 'deploy/guard.example.json').read_text())
    guard.append(cm('ssf-guard-config', 'ssf-system', guard_cfg))
    write('05-guard', guard)

    fixture = json.loads(subprocess.check_output([
        sys.executable, str(SSF / 'scripts/render_fixture.py')]))['items']
    fixture = [o for o in fixture if o['kind'] != 'Namespace' and
               o['metadata']['name'] not in ('ssf-file', 'test.file.ssf.dev',
                                            'ssf-file-immediate', 'ssf-file-wffc')]
    gateway = json.loads((SSF / 'deploy/gateway.example.json').read_text())
    gateway.update(AllowedPeers=['spiffe://ssf/agentenv/control'],
                   DriverName='test.block.ssf.dev', DriverEndpoint='unix:///csi/csi.sock')
    fixture.append(cm('ssf-gateway-config', 'ssf-fixture', gateway))
    for o in fixture:
        if o['kind'] != 'DaemonSet':
            continue
        o['spec']['updateStrategy'] = {'type': 'OnDelete'}
        spec = o['spec']['template']['spec']
        spec['affinity'] = {'nodeAffinity': {'requiredDuringSchedulingIgnoredDuringExecution': {
            'nodeSelectorTerms': [{'matchFields': [{'key': 'metadata.name', 'operator': 'In',
                                                  'values': [node]}]}]}}}
        spec['containers'][0]['image'] = a.test_image
        spec['volumes'] += [
            {'name': 'gateway-config', 'configMap': {'name': 'ssf-gateway-config'}},
            {'name': 'gateway-tls', 'secret': {'secretName': 'ssf-gateway-tls'}}]
        spec['containers'].append({'name': 'gateway', 'image': a.ssf_image,
            'command': ['ssf-csi-gateway'], 'args': ['-config', '/etc/ssf/config.json'],
            'env': [{'name': 'SSF_DATABASE_URL', 'valueFrom': {'secretKeyRef': {
                'name': 'ssf-ledger', 'key': 'url'}}}],
            # CSI socket permissions are driver-specific. Root is test-only here.
            'securityContext': {'runAsUser': 0},
            'ports': [{'containerPort': 9443}, {'containerPort': 9090}],
            'volumeMounts': [{'name': 'socket', 'mountPath': '/csi'},
                             {'name': 'gateway-config', 'mountPath': '/etc/ssf', 'readOnly': True},
                             {'name': 'gateway-tls', 'mountPath': '/etc/ssf-tls', 'readOnly': True}]})
    fixture.append(resource('Service', 'ssf-gateway', 'ssf-fixture', spec={
        'selector': {'app': 'ssf-block'}, 'ports': [{'port': 9443, 'targetPort': 9443}]}))
    write('06-block-fixture-test-only', fixture)

    builder_config = ('[custom_extension]\nstorage_required = false\nstorage_readonly_slots = 2\n'
                      '[volume]\nmax_volume_count = 8\n' + template_config)
    builder = resource('Pod', 'agentenv-template-builder', 'agentenv-ssf', spec={
        'nodeName': node, 'restartPolicy': 'Never', 'terminationGracePeriodSeconds': 120,
        'initContainers': [seed()],
        'containers': [{'name': 'agentenv', 'image': a.agent_image,
            'securityContext': {'privileged': True, 'runAsUser': 0},
            'env': [{'name': 'API_ADDR', 'value': '127.0.0.1:8000'},
                    {'name': 'AENV_CONFIG_PATH', 'value': '/config/agentenv.toml'},
                    {'name': 'AENV_EXTERNAL_STORAGE_REQUIRED', 'value': 'false'},
                    {'name': 'AENV_OBSERVABILITY_SCHEDULER_REPORT_ENABLED', 'value': 'false'},
                    {'name': 'AENV_API_KEY', 'valueFrom': {'secretKeyRef': {
                        'name': 'agentenv-native-key', 'key': 'AENV_API_KEY'}}}],
            'resources': {'requests': {'cpu': '2', 'memory': '1Gi'}},
            'readinessProbe': {'exec': {'command': ['curl', '-fsS', 'http://127.0.0.1:8000/health']},
                               'initialDelaySeconds': 10, 'timeoutSeconds': 3},
            'volumeMounts': [{'name': 'workspace', 'mountPath': '/workspace'},
                             {'name': 'config', 'mountPath': '/config', 'readOnly': True},
                             {'name': 'templates', 'mountPath': '/templates'},
                             {'name': 'dev', 'mountPath': '/dev'}]}],
        'volumes': [volume('workspace', '/var/lib/aenv-builder'),
                    volume('templates', '/var/lib/aenv-templates'), volume('dev', '/dev'),
                    {'name': 'config', 'configMap': {'name': 'agentenv-template-builder'}}]})
    write('07-template-builder', [resource('ConfigMap', 'agentenv-template-builder', 'agentenv-ssf',
        data={'agentenv.toml': builder_config}), builder])
    write('08-workers', [o for o in app if o['kind'] == 'DaemonSet'])
    write('09-control', [o for o in app if o['kind'] == 'Deployment'])
    print('Rendered installation overlay only. Follow the guide phase by phase; do not apply this directory wholesale.')


if __name__ == '__main__':
    main()
