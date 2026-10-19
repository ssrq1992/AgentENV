#!/usr/bin/env python3
"""Explicit Linux test-process fault injection using pidfd and pinned identity.

Run on an isolated test WORKER host as root. Does not discover/select a target,
kill a VMM, remove state, or reboot a node. STOP automatically resumes via pidfd.
"""
import argparse
import json
import os
import pathlib
import signal
import sys
import time

ALLOWED = {'server', 'agentenvadapter', 'ssf-node', 'ssf-guard',
           'ssf-csi-gateway', 'csi-provisioner'}


def start_ticks(pid):
    return pathlib.Path(f'/proc/{pid}/stat').read_text().rsplit(')',1)[1].split()[19]


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--pid', required=True, type=int, help='HOST PID, not container namespace PID')
    p.add_argument('--start-ticks', required=True)
    p.add_argument('--exe', required=True, help='exact /proc/PID/exe target')
    p.add_argument('--boot-id', required=True)
    p.add_argument('--signal', required=True, choices=['TERM','KILL','STOP'])
    p.add_argument('--hold-seconds', type=float, default=5, help='STOP only, 0 < n <= 10')
    p.add_argument('--out', required=True, help='new private JSON result file')
    a = p.parse_args()
    if sys.platform != 'linux' or os.geteuid()!=0: p.error('Linux host root required')
    if not hasattr(os,'pidfd_open') or not hasattr(signal,'pidfd_send_signal'): p.error('Python pidfd APIs required')
    if a.pid <= 1 or pathlib.Path(a.exe).name not in ALLOWED: p.error('target is not an allowed test service')
    if not 0 < a.hold_seconds <= 10: p.error('STOP must auto-resume within 10 seconds')
    os.umask(0o077)
    record = {'time_unix':time.time(), 'pid':a.pid, 'expected_start':a.start_ticks,
              'exe':a.exe, 'boot_id':a.boot_id, 'signal':a.signal, 'status':'INTENT'}
    # Refuse to overwrite evidence and persist intent before the signal.
    with open(a.out,'x') as report:
        def save():
            report.seek(0); json.dump(record,report,indent=2); report.truncate(); report.flush(); os.fsync(report.fileno())
        save(); fd=None; stopped=False
        try:
            if pathlib.Path('/proc/sys/kernel/random/boot_id').read_text().strip()!=a.boot_id:
                raise RuntimeError('host boot identity differs')
            fd=os.pidfd_open(a.pid)
            if start_ticks(a.pid)!=a.start_ticks or os.readlink(f'/proc/{a.pid}/exe')!=a.exe:
                raise RuntimeError('process identity differs; no signal sent')
            signal.pidfd_send_signal(fd,getattr(signal,'SIG'+a.signal)); stopped=a.signal=='STOP'
            record.update(status='SIGNAL_SENT',sent_unix=time.time());save()
            if stopped:
                time.sleep(a.hold_seconds)
            record['status']='INJECTION_COMPLETED'
        except BaseException as e:
            record.update(status='INJECTION_ERROR',error=str(e))
        finally:
            if stopped:
                try:
                    signal.pidfd_send_signal(fd,signal.SIGCONT)
                    record['continued_unix']=time.time()
                except ProcessLookupError:
                    record['resume_result']='process already exited; inspect kubelet/container restart evidence'
                except Exception as e:
                    record.update(status='INJECTION_ERROR',resume_error=str(e))
            if fd is not None:os.close(fd)
            record['finished_unix']=time.time();save()
    print(record['status']+'; this is fault injection evidence, not recovery PASS')
    return 0 if record['status']=='INJECTION_COMPLETED' else 1


if __name__=='__main__':
    raise SystemExit(main())
