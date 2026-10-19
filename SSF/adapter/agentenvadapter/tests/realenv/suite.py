#!/usr/bin/env python3
"""Scenario tests against a REAL deployed adapter, Kubernetes and guest.

No mock/dry-run success mode. Persist requests before RPCs; retain PVCs unless
the explicit reclaim case is selected. Failed/unknown operations remain visible.
"""
import argparse
import concurrent.futures
import copy
import hashlib
import json
import math
import os
import pathlib
import shlex
import subprocess
import sys
import threading
import time
import urllib.error
import uuid

ADAPTER = pathlib.Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ADAPTER / 'examples'))
from client import Client

CASES = {
    'durability': 'S01 cold/template RW/RO and retained data',
    'wffc': 'S02 dynamic WFFC before/after first consumption',
    'multi_volume': 'S03 two-volume isolation and remount',
    'idempotency': 'S04 concurrent retry and parameter conflict',
    'writer_lock': 'S05 exclusive writer and handoff',
    'cancel': 'S06 immediate cancellation and tombstone observation',
    'guard': 'S07 admission and active-volume deletion protection',
    'validation': 'S08 invalid/unsupported requests and namespace isolation',
    'shared_readers': 'S09 two readers and first/last holder release',
    'static': 'S10 existing static ext4 PVC consumption',
    'reclaim': 'S11 Retain or explicitly enabled Delete',
    'performance': 'S12 measured warm-volume cold/template concurrency',
    'partial_failure': 'S13 second unformatted volume without initialization grant',
    'uid_mismatch': 'S14 expected PVC UID mismatch',
}
DEFAULT = 'durability,wffc,multi_volume,idempotency,writer_lock,cancel,guard,validation,partial_failure,uid_mismatch'


class Blocked(Exception):
    pass


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def operation_id(tenant, request_id):
    # Matches adapter Store.Create / core.Hash, not the internal consumption ID.
    return hashlib.sha256((tenant + '\0' + request_id).encode()).hexdigest()


def redact(value):
    if isinstance(value, dict):
        return {k: ('[REDACTED]' if any(x in k.lower() for x in
                ('token', 'password', 'secret', 'signedplan', 'ticket')) else redact(v))
                for k, v in value.items()}
    if isinstance(value, list):
        return [redact(x) for x in value]
    return value


def percentiles(samples):
    groups = {}
    for row in samples:
        key = f"{row['case']}/{row['kind']}/{'ro' if row['readonly'] else 'rw'}/{row['path']}"
        group = groups.setdefault(key, [])
        group.append(row)
    result = {}
    for key, rows in groups.items():
        metrics = {}
        for field in {k for r in rows for k in r if k.endswith('_ms')}:
            values = sorted(r[field] for r in rows if isinstance(r.get(field), (int, float)))
            if values:
                metrics[field] = {'n': len(values), **{
                    f'p{p}': values[max(0, math.ceil(len(values)*p/100)-1)] for p in (50, 95, 99)}}
        result[key] = metrics
    return result


class Suite:
    def __init__(self, cfg, out, cases, allow_existing=False, allow_delete=False):
        self.cfg = cfg
        self.out = pathlib.Path(out)
        self.out.mkdir(mode=0o700, parents=True, exist_ok=False)
        self.lock = threading.RLock()
        self.run_id = 'rt-' + time.strftime('%Y%m%d-%H%M%S') + '-' + uuid.uuid4().hex[:8]
        self.client = Client(cfg['url'], cfg['ca'], cfg['cert'], cfg['key'])
        self.allow_existing = allow_existing
        self.allow_delete = allow_delete
        self.current = ''
        self.report = {'run_id': self.run_id, 'started_unix': time.time(),
            'target': {k: cfg.get(k) for k in ('url', 'tenant', 'namespace', 'context', 'image',
                        'template', 'immediate_class', 'wffc_class', 'driver_kind', 'concurrency')},
            'status': 'RUNNING', 'approved_for_production': False,
            'cases': {k: {'description': v, 'status': 'NOT_RUN'} for k, v in CASES.items()},
            'selected': cases, 'volumes': [], 'operations': {}, 'samples': [],
            'cleanup': [], 'manual_evidence_required': ['VMM/FD and guest evidence correlation',
                'last-holder Unpublish/Unstage/Detach order', 'backend deletion confirmation',
                'process/node fault matrix', 'vendor/multiworker certification']}
        self.save()

    def save(self):
        with self.lock:
            tmp = self.out / 'report.tmp'
            with tmp.open('w') as f:
                json.dump(redact(self.report), f, indent=2)
                f.flush()
                os.fsync(f.fileno())
            os.replace(tmp, self.out / 'report.json')

    def event(self, kind, **data):
        with self.lock:
            with (self.out / 'events.jsonl').open('a') as f:
                f.write(json.dumps(redact({'time_unix': time.time(), 'case': self.current,
                                          'event': kind, **data})) + '\n')
                f.flush()
                os.fsync(f.fileno())

    def call(self, method, path, body=None):
        # Intent is logged even if no response arrives.
        self.event('http_intent', method=method, path=path, body=body)
        try:
            value = self.client.call(method, path, body)
            self.event('http_response', method=method, path=path, response=value)
            return value
        except urllib.error.HTTPError as e:
            raw = e.read().decode(errors='replace')
            self.event('http_error', path=path, status=e.code, response=raw)
            e.response_body = raw
            raise
        except Exception as e:
            self.event('http_unknown', path=path, error=str(e))
            raise

    def expect_http(self, method, path, body, code, substring):
        try:
            self.call(method, path, body)
        except urllib.error.HTTPError as e:
            require(e.code == code and substring.lower() in e.response_body.lower(),
                    f'expected HTTP {code} with {substring!r}, got {e.code}: {e.response_body}')
            return
        raise AssertionError(f'{method} {path} unexpectedly accepted')

    def kubectl(self, args, body=None, expect_denial=None):
        cmd = ['kubectl', '--context', self.cfg['context'], '--request-timeout=30s', *args]
        p = subprocess.run(cmd, input=None if body is None else json.dumps(body),
                           text=True, capture_output=True, timeout=45)
        self.event('kubectl', args=args, exit_code=p.returncode, stdout=p.stdout, stderr=p.stderr)
        if expect_denial:
            require(p.returncode != 0 and expect_denial in p.stderr,
                    'expected specific Guard denial, got: ' + p.stderr)
            return None
        if p.returncode:
            raise Blocked('kubectl failed: ' + p.stderr)
        return json.loads(p.stdout) if '-o' in args and args[args.index('-o')+1] == 'json' else p.stdout

    def native(self, kind, name, namespace=None):
        args = ['get', kind, name, '-o', 'json', '--ignore-not-found']
        if namespace:
            args += ['-n', namespace]
        # Use a separate invocation to distinguish NotFound from permission/transport errors.
        p = subprocess.run(['kubectl', '--context', self.cfg['context'], '--request-timeout=30s', *args],
                           capture_output=True, text=True, timeout=45)
        if p.returncode:
            raise Blocked(p.stderr)
        return json.loads(p.stdout) if p.stdout.strip() else None

    def volume(self, label, mode='immediate', initialize=True):
        name = self.run_id + '-' + label + '-' + uuid.uuid4().hex[:4]
        q = {'namespace': self.cfg['namespace'], 'name': name,
             'storage_class': self.cfg[mode+'_class'], 'bytes': self.cfg['volume_bytes'],
             'initialize_ext4': initialize}
        record = {'namespace': q['namespace'], 'name': name, 'uid': None,
                  'created_by_run': True, 'request': q}
        with self.lock:
            self.report['volumes'].append(record)
            self.save()
        c = self.call('POST', '/v1/volumes', q)
        require(c['spec']['volumeMode'] == 'Block', 'expected Block PVC')
        with self.lock:
            record['uid'] = c['metadata']['uid']
            self.save()
        return record

    def claim(self, v):
        c = self.call('GET', f"/v1/volumes/{v['namespace']}/{v['name']}")
        require(c['metadata']['uid'] == v['uid'], 'PVC UID changed')
        return c

    def bound(self, v):
        deadline = time.monotonic() + self.cfg['operation_timeout']
        while time.monotonic() < deadline:
            c = self.claim(v)
            if c.get('status', {}).get('phase') == 'Bound':
                pv = self.native('pv', c['spec']['volumeName'])
                require(pv and pv['spec']['claimRef']['uid'] == v['uid'], 'PV claim UID mismatch')
                require(pv['spec']['persistentVolumeReclaimPolicy'] == 'Retain', 'requires Retain')
                with self.lock:
                    v.update(pv=pv['metadata']['name'], pv_uid=pv['metadata']['uid'],
                             driver=pv['spec']['csi']['driver'], handle=pv['spec']['csi']['volumeHandle'])
                self.event('bound', pvc=c, pv=pv)
                self.save()
                return c
            time.sleep(self.cfg['poll_seconds'])
        raise Blocked('PVC did not bind; inspect native provisioner/topology')

    def request(self, volumes, kind='cold', readonly=False):
        launch = {'image': self.cfg['image'], 'cpuCount': self.cfg['cpu'],
                  'memoryMB': self.cfg['memory_mib']} if kind == 'cold' else {'templateID': self.cfg['template']}
        launch['timeout'] = max(1800, self.cfg['operation_timeout'] * 3)
        return {'request_id': self.run_id+'-'+uuid.uuid4().hex[:12], 'kind': kind,
                'launch': launch, 'mounts': [{'namespace': v['namespace'], 'pvc': v['name'],
                 'uid': v['uid'], 'id': f'data{i}', 'path': '/data' if len(volumes)==1 else f'/data{i}',
                 'readonly': readonly} for i, v in enumerate(volumes)]}

    def create(self, q):
        op = operation_id(self.cfg['tenant'], q['request_id'])
        with self.lock:
            self.report['operations'].setdefault(op, {'request': q, 'state': 'INTENT_UNKNOWN'})
            self.save()
        o = self.call('POST', '/v1/sandboxes', q)
        require(o['id'] == op, 'tenant config differs from server mapping; retain response for recovery')
        with self.lock:
            self.report['operations'][op].update(state=o['state'], instance=o['instance'])
            self.save()
        return o

    def inspect(self, op):
        o = self.call('GET', '/v1/operations/'+op)
        with self.lock:
            self.report['operations'][op].update(state=o['state'], instance=o['instance'],
                sandbox_id=o.get('sandbox_id'), stopping=o.get('stopping'), reason=o.get('reason'))
            self.save()
        return o

    def wait(self, op, target='Ready'):
        until = time.monotonic() + self.cfg['operation_timeout']
        while time.monotonic() < until:
            o = self.inspect(op)
            if o['state'] == target:
                return o
            if target == 'Ready' and (o['state'] in ('Released', 'Rejected') or o.get('stopping')):
                raise AssertionError('launch stopped before Ready: '+str(o.get('reason')))
            time.sleep(self.cfg['poll_seconds'])
        raise Blocked(f'{op} not {target}; resource state remains owned/unknown')

    def start(self, q, path='new-volume'):
        start = time.monotonic()
        o = self.create(q)
        ready = self.wait(o['id'])
        row = {'case': self.current, 'kind': q['kind'], 'readonly': q['mounts'][0]['readonly'],
               'path': path, 'operation': o['id'], 'overall_ms': (time.monotonic()-start)*1000,
               **ready.get('timings_ms', {})}
        with self.lock:
            self.report['samples'].append(row)
            self.save()
        return o['id']

    def stop(self, op):
        self.call('DELETE', '/v1/operations/'+op)
        return self.wait(op, 'Released')

    def guest(self, op, command):
        self.event('guest_intent', operation=op, command=command)
        result = self.client.execute(op, command)
        self.event('guest_result', operation=op, exit_code=result[0], stdout=result[1], stderr=result[2])
        return result

    def write_data(self, op, mount, marker):
        filename = mount+'/'+self.run_id+'.bin'
        cmd = (f"head -c 1048576 /dev/zero | tr '\\000' x > {shlex.quote(filename)}; "
               f"printf '%s' {shlex.quote(marker)} >> {shlex.quote(filename)}; sync; sha256sum {shlex.quote(filename)}")
        code, out, err = self.guest(op, cmd)
        expected = hashlib.sha256(b'x'*1048576+marker.encode()).hexdigest()
        require(code == 0 and out.split()[0] == expected, 'guest write/hash failed: '+err)
        return expected

    def read_data(self, op, mount, expected, readonly=False):
        filename = mount+'/'+self.run_id+'.bin'
        code, out, err = self.guest(op, 'sha256sum '+shlex.quote(filename))
        require(code == 0 and out.split()[0] == expected, 'retained data differs: '+err)
        code, out, err = self.guest(op, "awk '$2 == "+json.dumps(mount)+" {print $3, $4}' /proc/mounts")
        fields = out.strip().split()
        require(code == 0 and len(fields) == 2 and fields[0] == 'ext4', 'expected actual ext4 mount')
        require(('ro' if readonly else 'rw') in fields[1].split(','), 'guest mount flags differ')
        if readonly:
            code, _, err = self.guest(op, 'LC_ALL=C; export LC_ALL; printf bad > '+shlex.quote(filename+'.forbidden'))
            require(code != 0 and 'read-only file system' in err.lower(), 'write must fail specifically with EROFS')

    def cycle(self, v):
        marker = uuid.uuid4().hex
        expected = None
        for kind, ro in [('cold', False), ('template', False), ('cold', True), ('template', True)]:
            q = self.request([v], kind, ro)
            op = self.start(q, 'new-volume' if expected is None and v['created_by_run'] else 'reuse')
            self.bound(v)
            if expected is None:
                expected = self.write_data(op, '/data', marker)
            self.read_data(op, '/data', expected, ro)
            repeat = self.create(q)
            require(repeat['instance'] == self.report['operations'][op]['instance'], 'retry changed instance')
            self.stop(op)
            self.claim(v)

    def case_durability(self):
        v = self.volume('durable'); self.bound(v); self.cycle(v)

    def case_wffc(self):
        v = self.volume('wffc', 'wffc')
        c = self.claim(v)
        require(c.get('status', {}).get('phase') != 'Bound' and not c['spec'].get('volumeName'),
                'new WFFC PVC unexpectedly prebound')
        require(not c['metadata'].get('annotations', {}).get('volume.kubernetes.io/selected-node'),
                'selection existed before first consumer')
        self.cycle(v)
        require(bool(self.claim(v)['metadata'].get('annotations', {}).get('volume.kubernetes.io/selected-node')),
                'dynamic WFFC selection not observed; inspect provider behavior')

    def case_multi_volume(self):
        vs = [self.volume('multi0'), self.volume('multi1')]
        op = self.start(self.request(vs))
        hashes = [self.write_data(op, '/data'+str(i), uuid.uuid4().hex) for i in range(2)]
        require(hashes[0] != hashes[1], 'test data must differ')
        self.stop(op)
        op = self.start(self.request(vs, 'template'), 'reuse')
        for i in range(2): self.read_data(op, '/data'+str(i), hashes[i])
        self.stop(op)

    def case_idempotency(self):
        v = self.volume('idem'); self.bound(v); q = self.request([v])
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            results = list(pool.map(lambda _: self.create(q), range(8)))
        require(len({o['id'] for o in results}) == len({o['instance'] for o in results}) == 1,
                'concurrent retries created different operations/instances')
        op = results[0]['id']; self.wait(op)
        changed = copy.deepcopy(q); changed['mounts'][0]['path'] = '/different'
        self.expect_http('POST', '/v1/sandboxes', changed, 409, 'conflicting')
        self.stop(op)
        repeat = self.create(q)
        require(repeat['state'] == 'Released', 'released request ID started another VM')

    def case_writer_lock(self):
        v = self.volume('writer'); a = self.start(self.request([v]))
        expected = self.write_data(a, '/data', uuid.uuid4().hex)
        b = self.create(self.request([v]))['id']
        deadline = time.monotonic()+self.cfg['observation_seconds']; conflict = False
        while time.monotonic() < deadline:
            o = self.inspect(b)
            require(o['state'] != 'Ready' and not o.get('sandbox_id'), 'second writer reached runtime')
            conflict |= 'conflicting operation' in o.get('reason', '')
            time.sleep(self.cfg['poll_seconds'])
        if not conflict:
            raise Blocked('writer conflict not observed; ensure spare compute for second reservation')
        self.read_data(a, '/data', expected)
        self.stop(a); self.wait(b); self.read_data(b, '/data', expected); self.stop(b)

    def case_cancel(self):
        v = self.volume('cancel'); self.bound(v); q = self.request([v])
        op = self.create(q)['id']; self.stop(op)
        deadline = time.monotonic()+self.cfg['observation_seconds']
        while time.monotonic() < deadline:
            require(self.create(q)['state'] == 'Released', 'late retry resurrected cancelled request')
            time.sleep(max(1, self.cfg['poll_seconds']))
        self.claim(v)
        self.event('scope', note='random cancel timing; does not prove deterministic delayed-success injection')

    def case_guard(self):
        v = self.volume('guard'); op = self.start(self.request([v])); self.bound(v)
        pod = {'apiVersion': 'v1', 'kind': 'Pod', 'metadata': {'name': self.run_id+'-probe',
               'namespace': v['namespace']}, 'spec': {'containers': [{'name': 'probe',
               'image': 'busybox:1.37.0', 'command': ['sleep', '60'],
               'volumeDevices': [{'name': 'data', 'devicePath': '/dev/volume'}]}],
               'volumes': [{'name': 'data', 'persistentVolumeClaim': {'claimName': v['name']}}]}}
        self.kubectl(['create', '--dry-run=server', '-f', '-'], pod,
                     expect_denial='SSF-managed volume cannot be consumed by native Pod')
        self.kubectl(['delete', 'pvc', v['name'], '-n', v['namespace'], '--dry-run=server'],
                     expect_denial='drain/reclaim permit required')
        self.kubectl(['label', 'pvc', v['name'], '-n', v['namespace'], 'ssf.dev/managed-', '--dry-run=server'],
                     expect_denial='cannot remove consumption protection')
        # This starts drain intent even when held; use a dedicated disposable claim.
        self.expect_http('DELETE', f"/v1/volumes/{v['namespace']}/{v['name']}?uid={v['uid']}&reclaim=Retain",
                         None, 409, 'operation pending')
        self.claim(v); self.stop(op)

    def case_validation(self):
        v = self.volume('validation'); q = self.request([v])
        tests = []
        for field in ('autoPause', 'autoResume'):
            b = copy.deepcopy(q); b['launch'][field] = True; tests.append((b, 'not supported'))
        for field in ('volumeMounts', 'attachedDrives', 'customExtensionParams'):
            b = copy.deepcopy(q); b['launch'][field] = []; tests.append((b, 'reserved'))
        for path, message in [('/dev/forbidden', 'reserved'), ('/data/../wrong', 'invalid guest path')]:
            b = copy.deepcopy(q); b['mounts'][0]['path'] = path; tests.append((b, message))
        b = copy.deepcopy(q); b['mounts'][0]['namespace'] = self.cfg['forbidden_namespace']; tests.append((b, 'forbidden'))
        for body, message in tests:
            self.expect_http('POST', '/v1/sandboxes', body, 400, message)
            self.expect_http('GET', '/v1/operations/'+operation_id(self.cfg['tenant'], q['request_id']), None, 404, 'not found')
        self.claim(v)

    def case_shared_readers(self):
        v = self.volume('readers'); w = self.start(self.request([v]))
        expected = self.write_data(w, '/data', uuid.uuid4().hex); self.stop(w)
        a = self.start(self.request([v], readonly=True), 'reuse')
        b = self.start(self.request([v], readonly=True), 'shared-reuse')
        self.read_data(a, '/data', expected, True); self.read_data(b, '/data', expected, True)
        self.stop(a)
        self.read_data(b, '/data', expected, True); self.stop(b)
        self.event('scope', note='remaining guest readability checked; exact CSI last-holder order needs node/driver evidence')

    def case_static(self):
        if not self.allow_existing or not self.cfg.get('static_pvc'):
            raise Blocked('static requires static_pvc and --allow-existing-pvc-write')
        c = self.call('GET', f"/v1/volumes/{self.cfg['namespace']}/{self.cfg['static_pvc']}")
        v = {'name': c['metadata']['name'], 'namespace': c['metadata']['namespace'],
             'uid': c['metadata']['uid'], 'created_by_run': False}
        self.report['volumes'].append(v); self.save()
        sc = self.native('storageclass', c['spec']['storageClassName'])
        require(sc and sc['provisioner'] == 'kubernetes.io/no-provisioner', 'static requires no-provisioner class')
        if self.cfg.get('static_mode') == 'wffc':
            require(sc['volumeBindingMode'] == 'WaitForFirstConsumer' and not c['spec'].get('volumeName'),
                    'static WFFC must begin unbound without volumeName')
        else:
            require(c.get('status', {}).get('phase') == 'Bound', 'prebound PVC must already be Bound')
        self.cycle(v)

    def case_reclaim(self):
        actions = ['Retain', 'Delete'] if self.allow_delete else ['Retain']
        for action in actions:
            v = self.volume('reclaim-'+action.lower()); op = self.start(self.request([v]))
            self.bound(v); self.stop(op)
            deadline = time.monotonic()+self.cfg['operation_timeout']
            done = False
            while time.monotonic() < deadline:
                self.call('DELETE', f"/v1/volumes/{v['namespace']}/{v['name']}?uid={v['uid']}&reclaim={action}")
                c = self.native('pvc', v['name'], v['namespace']); pv = self.native('pv', v['pv'])
                if c is None and ((action == 'Delete' and pv is None) or
                    (action == 'Retain' and pv and pv['metadata']['uid'] == v['pv_uid'] and
                     pv['status']['phase'] == 'Released' and pv['spec']['persistentVolumeReclaimPolicy'] == 'Retain')):
                    done = True; break
                time.sleep(1)
            require(done, 'native reclaim did not reach expected state')
            v['reclaim_action'] = action; v['backend_confirmation'] = 'MANUAL_REQUIRED'; self.save()
        if not self.allow_delete:
            self.event('scope', note='Delete branch NOT_RUN; pass only covers Retain')

    def case_partial_failure(self):
        vs = [self.volume('partial0'), self.volume('partial1', initialize=False)]
        for v in vs: self.bound(v)
        op = self.create(self.request(vs))['id']
        until = time.monotonic()+self.cfg['operation_timeout']
        saw_format_denial = False
        while time.monotonic() < until:
            o = self.inspect(op)
            require(o['state'] != 'Ready', 'raw unauthorized filesystem unexpectedly became Ready')
            saw_format_denial |= 'blank volume requires explicit new-volume initialization' in o.get('reason', '')
            if o['state'] == 'Released': break
            time.sleep(self.cfg['poll_seconds'])
        else: raise Blocked('partial failure did not compensate; inspect Node WAL and runtime evidence')
        require(saw_format_denial, 'expected filesystem authorization failure was not observed; unrelated failure is not PASS')
        # The correctly initialized first volume must be usable after compensation.
        self.cycle(vs[0])
        self.event('scope', note='post-Prepare filesystem failure; NOT an injected CSI second-NodePublish RPC failure')

    def case_uid_mismatch(self):
        v = self.volume('uid'); self.bound(v); q = self.request([v])
        q['mounts'][0]['uid'] = str(uuid.uuid4()); op = self.create(q)['id']
        until = time.monotonic()+self.cfg['observation_seconds']; saw_conflict = False
        while time.monotonic() < until:
            o = self.inspect(op)
            require(o['state'] != 'Ready' and not o.get('sandbox_id'), 'wrong UID consumed volume')
            saw_conflict |= 'conflicting' in o.get('reason', '')
            time.sleep(self.cfg['poll_seconds'])
        require(saw_conflict, 'UID conflict not observed')
        self.stop(op); self.claim(v)

    def case_performance(self):
        def task(_):
            v = self.volume('perf'); self.bound(v)
            warm = self.start(self.request([v]), 'warmup-initialize')
            expected = self.write_data(warm, '/data', uuid.uuid4().hex); self.stop(warm)
            for kind in ('cold', 'template'):
                op = self.start(self.request([v], kind), 'measured-reuse')
                self.read_data(op, '/data', expected); self.stop(op)
        pool = concurrent.futures.ThreadPoolExecutor(max_workers=self.cfg['concurrency'])
        futures = [pool.submit(task, i) for i in range(self.cfg['performance_runs'])]
        try:
            for future in concurrent.futures.as_completed(futures): future.result()
        finally:
            # Let already-dispatched tasks finish recording their outcomes;
            # avoid starting queued tasks after a failure/interruption.
            pool.shutdown(wait=True, cancel_futures=True)

    def cleanup(self):
        # Stop only this run's known operations. NEVER delete PVCs or infer release from 404.
        for op, record in list(self.report['operations'].items()):
            if record['state'] == 'Released': continue
            try:
                o = self.inspect(op)
                require(o['request']['request_id'] == record['request']['request_id'], 'cleanup identity mismatch')
                self.stop(op)
                self.report['cleanup'].append({'operation': op, 'state': 'Released'})
            except Exception as e:
                self.report['cleanup'].append({'operation': op, 'state': 'UNKNOWN_RETAINED', 'error': str(e)})
            self.save()

    def run(self):
        try:
            for case in self.report['selected']:
                self.current = case
                record = self.report['cases'][case]
                record.update(status='RUNNING', started_unix=time.time()); self.save()
                print(case+': running', flush=True)
                try:
                    getattr(self, 'case_'+case)()
                    record['status'] = 'PASS'
                except (Blocked, urllib.error.URLError, TimeoutError, OSError) as e:
                    record.update(status='BLOCKED', error=str(e))
                except Exception as e:
                    record.update(status='FAIL', error=str(e))
                record['finished_unix'] = time.time(); self.save()
                print(case+': '+record['status'], flush=True)
                if record['status'] != 'PASS': break
        except KeyboardInterrupt:
            self.report['status'] = 'INTERRUPTED'
        finally:
            self.current = 'cleanup'
            self.cleanup()
            states = [self.report['cases'][k]['status'] for k in self.report['selected']]
            clean = not any(r['state'] == 'UNKNOWN_RETAINED' for r in self.report['cleanup'])
            if self.report['status'] != 'INTERRUPTED':
                self.report['status'] = 'PASS_SELECTED_CASES' if all(s == 'PASS' for s in states) and clean else 'INCOMPLETE_OR_FAILED'
            self.report['finished_unix'] = time.time()
            self.report['percentiles'] = percentiles(self.report['samples'])
            self.save()
        return 0 if self.report['status'] == 'PASS_SELECTED_CASES' else 1


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--config', required=True)
    p.add_argument('--out', required=True, help='new output directory; never overwrites a prior run')
    p.add_argument('--cases', default=DEFAULT, help=','.join(CASES))
    p.add_argument('--allow-existing-pvc-write', action='store_true')
    p.add_argument('--allow-backend-delete', action='store_true')
    a = p.parse_args(); cfg = json.loads(pathlib.Path(a.config).read_text())
    cases = a.cases.split(',')
    if not cases or len(set(cases)) != len(cases) or any(c not in CASES for c in cases): p.error('invalid or duplicate cases')
    for key in ('url','ca','cert','key','tenant','namespace','context','immediate_class','wffc_class','image','template','forbidden_namespace'):
        if not isinstance(cfg.get(key), str) or not cfg[key] or 'REPLACE_' in cfg[key]: p.error('configure '+key)
    if not cfg['url'].startswith('https://'): p.error('mTLS HTTPS URL required')
    for key in ('operation_timeout','observation_seconds','poll_seconds','volume_bytes','cpu','memory_mib','concurrency','performance_runs'):
        if not isinstance(cfg.get(key), (int,float)) or cfg[key] <= 0: p.error('positive '+key+' required')
    for key in ('concurrency','performance_runs','volume_bytes','cpu','memory_mib'):
        if not isinstance(cfg[key],int): p.error(key+' must be an integer')
    os.umask(0o077)
    return Suite(cfg, a.out, cases, a.allow_existing_pvc_write, a.allow_backend_delete).run()


if __name__ == '__main__':
    sys.exit(main())
