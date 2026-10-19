#!/usr/bin/env python3
"""Read-only Linux /proc scan for open block FDs of one saved major:minor.

Run in the HOST PID namespace as root, before and after release. This is
supplementary evidence, not a fencing proof (races and kernel references exist).
"""
import argparse
import json
import os
import pathlib
import stat
import sys
import time


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--major', required=True, type=int)
    p.add_argument('--minor', required=True, type=int)
    p.add_argument('--out', required=True)
    a = p.parse_args()
    if sys.platform != 'linux': p.error('real Linux /proc required')
    if min(a.major, a.minor) < 0: p.error('device numbers must be nonnegative')
    os.umask(0o077)
    result = {'started_unix': time.time(), 'major': a.major, 'minor': a.minor,
              'boot_id': pathlib.Path('/proc/sys/kernel/random/boot_id').read_text().strip(),
              'scanner_pid_namespace': os.readlink('/proc/self/ns/pid'),
              'root': os.geteuid() == 0, 'matches': [], 'inaccessible': [],
              'disappeared_during_scan': 0, 'fencing_proof': False}
    for process in pathlib.Path('/proc').iterdir():
        if not process.name.isdigit(): continue
        try:
            start = (process/'stat').read_text().rsplit(')', 1)[1].split()[19]
            ns = os.readlink(process/'ns/pid')
            for fd in (process/'fd').iterdir():
                try:
                    info = fd.stat()
                    if stat.S_ISBLK(info.st_mode) and (os.major(info.st_rdev),os.minor(info.st_rdev))==(a.major,a.minor):
                        fdinfo = (process/'fdinfo'/fd.name).read_text()
                        flags = next((s.split()[1] for s in fdinfo.splitlines() if s.startswith('flags:')), None)
                        result['matches'].append({'pid': int(process.name), 'start_ticks': start, 'pid_namespace': ns,
                            'exe': os.readlink(process/'exe'), 'fd': fd.name, 'target': os.readlink(fd),
                            'flags_octal': flags, 'access_mode': int(flags,8)&os.O_ACCMODE if flags else None})
                except FileNotFoundError: result['disappeared_during_scan'] += 1
                except PermissionError: result['inaccessible'].append(str(fd))
        except (FileNotFoundError, ProcessLookupError): result['disappeared_during_scan'] += 1
        except PermissionError: result['inaccessible'].append(str(process))
    result['finished_unix'] = time.time()
    result['scan_status'] = 'PARTIAL' if result['inaccessible'] or not result['root'] else 'OBSERVED'
    with open(a.out, 'x') as f: json.dump(result, f, indent=2)
    print(result['scan_status']+': '+str(len(result['matches']))+' matching FDs; not a fencing verdict')
    return 0 if result['scan_status'] == 'OBSERVED' else 2


if __name__ == '__main__':
    raise SystemExit(main())
