#!/usr/bin/env python3
"""Observe the canonical protected native restart window without proving or writing RPCs."""

import argparse
import ctypes
import errno
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import re
import select
import stat
import struct
import subprocess
import sys
import threading
import traceback
import time
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
ARTIFACTS = ROOT / '.artifacts'
READY_FD = 'VIBEFACE_PROTECTED_OBSERVER_READY_FD'
DONE_FD = 'VIBEFACE_PROTECTED_NATIVE_DONE_FD'
IN_OPEN, IN_MOVED_TO = 0x20, 0x80
BAD = 0x2 | 0x4 | 0x400 | 0x800 | 0x2000 | 0x4000 | 0x8000
sequence = 0
identity = lambda s: (s.st_dev, s.st_ino, s.st_size, s.st_mtime_ns, s.st_ctime_ns)


class ObservationIncomplete(RuntimeError):
    pass


def run_binding(run, configuration, repository=ROOT):
    relative = run.resolve().relative_to(repository.resolve()).as_posix()
    value = {'run': relative, 'configurationSha256': hashlib.sha256(configuration).hexdigest()}
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def file_sha(path):
    value = hashlib.sha256()
    with path.open('rb') as stream:
        for part in iter(lambda: stream.read(1024 * 1024), b''): value.update(part)
    return value.hexdigest()

def save():
    tmp = out / 'result.json.tmp'
    with tmp.open('w') as f:
        json.dump(summary, f, indent=2); f.write('\n'); f.flush(); os.fsync(f.fileno())
    os.replace(tmp, out / 'result.json')

def progress():
    return json.loads((public / 'progress.json').read_text())

def rpc_stats():
    value = json.loads(rpc_statistics_file.read_text())
    assert value['rpc_count_source'] == 'ganache method logger, including fixture and client requests'
    return value

def running_provers():
    found = []
    for path in Path('/proc').glob('[0-9]*/comm'):
        try: name = path.read_text().strip()
        except (FileNotFoundError, ProcessLookupError): continue
        if name == 'r0vm' or name.startswith('vibeface-delive'):
            found.append({'pid':int(path.parent.name), 'comm':name})
    return found

def request(url, method, params):
    global sequence
    sequence += 1
    packet = {'jsonrpc':'2.0','id':sequence,'method':method,'params':params}
    req = urllib.request.Request(url, json.dumps(packet).encode(), {'Content-Type':'application/json'})
    with opener.open(req, timeout=2) as response: value = json.load(response)
    assert value['id'] == sequence and 'error' not in value
    return value['result']

def chain_sample():
    p = progress(); deployment = p['deployment']; url = deployment['rpc_url']
    u = urllib.parse.urlparse(url)
    assert u.scheme == 'http' and not u.username and not u.password and not u.query
    assert ipaddress.ip_address(u.hostname).is_loopback
    assert int(request(url,'eth_chainId',[]),16) == 31337
    anchor = request(url,'eth_getBlockByNumber',['latest',False]); tag = anchor['number']
    wallets = {name:p['principals'][name]['wallet'] for name in ('S','R','P')}
    asset = deployment['asset']
    accounts = {}
    for name,address in wallets.items():
        accounts[name] = {'wallet':address,
            'nonce':request(url,'eth_getTransactionCount',[address,tag]),
            'pending_nonce':request(url,'eth_getTransactionCount',[address,'pending']),
            'native_balance':request(url,'eth_getBalance',[address,tag]),
            'token_balance':request(url,'eth_call',[{'to':asset,'data':'0x70a08231'+address[2:].zfill(64)},tag])}
    assert request(url,'eth_getBlockByNumber',[tag,False])['hash'] == anchor['hash']
    coinbase = request(url,'eth_coinbase',[])
    assert coinbase.lower() not in {v.lower() for v in wallets.values()}
    return {'monotonic':time.monotonic(),'anchor':{k:anchor[k] for k in ('number','hash','timestamp')},
        'accounts':accounts,'coinbase':coinbase,'asset':asset}

def read_events(wait):
    if not select.select([fd],[],[],wait)[0]: return []
    events = []
    while True:
        try: data = os.read(fd,65536)
        except BlockingIOError: break
        assert data
        offset = 0
        while offset < len(data):
            wd,mask,cookie,size = struct.unpack_from('iIII',data,offset); offset += 16
            name = data[offset:offset+size].split(b'\0',1)[0].decode(); offset += size
            events.append((wd,mask,name))
    return events

def collect_chain():
    sample_start.wait()
    while not sample_stop.is_set():
        try:
            value = chain_sample()
            value['completed_monotonic'] = time.monotonic()
            with sample_lock:
                if not sample_stop.is_set():
                    if samples['first'] is None: samples['first'] = value
                    samples['last'] = value; samples['count'] += 1
        except Exception as error:
            with sample_lock:
                if not sample_stop.is_set():
                    samples['errors'] += 1; samples['last_error_type'] = type(error).__name__
        sample_stop.wait(0.25)

def baseline_before_start(previous, candidate, events, directory_watch):
    start_seen = any(wd == directory_watch and mask & IN_MOVED_TO and name == 'settlement-observation.json'
                     for wd,mask,name in events)
    return previous if candidate is None or start_seen else candidate


def wait_events(wait, done_fd):
    ready, _, _ = select.select([fd, done_fd], [], [], wait)
    done = done_fd in ready
    if done and os.read(done_fd, 1) != b'':
        raise ObservationIncomplete('unexpected native completion data')
    return read_events(0), done


def watch(ready_fd, done_fd):
    global fd
    try:
        info = subprocess.run([str(program),'info'],capture_output=True,timeout=30,
            env={'PATH':'/usr/local/bin:/usr/bin:/bin','RISC0_SERVER_PATH':config['paths']['r0vm'],
                'TOKIO_WORKER_THREADS':'2'})
        assert info.returncode == 0
        identified = json.loads(info.stdout)
        assert identified['elfSha256'] == config['elf_sha256']
        calibration = read_events(0.2)
        opens = sum(wd == binary_watch and bool(mask & IN_OPEN) for wd,mask,_ in calibration)
        assert opens > 0, 'actual official info exec did not produce observable inode opens'
        assert not any(mask & BAD for _,mask,_ in calibration)
        summary['calibration'] = {'status':'PASS','operation':'official info; no prove','exit_code':0,
            'observed_inode_open_events':opens,'elf_sha256':identified['elfSha256']}
        assert not (public / 'settlement-observation.json').exists(), 'restart start marker preceded observation'
        summary['observer_phase'] = 'waiting-for-original-window'
        baseline = None; next_stats = 0
        threading.Thread(target=collect_chain, daemon=True).start()
        save(); print(json.dumps({'event':'restart_observer_ready','calibration':'PASS','proofs_generated':0}),flush=True)
        os.write(ready_fd, b'R')
        os.close(ready_fd); ready_fd = None
        while not summary['window_ended']:
            now = time.monotonic()
            candidate = None
            missing_before = summary['prewindow_rpc_snapshot_missing']['count']
            if not summary['window_started'] and now >= next_stats:
                try:
                    candidate = {'value':rpc_stats(),'read_completed_monotonic':time.monotonic()}
                except FileNotFoundError as error:
                    if (error.errno != errno.ENOENT or error.filename != str(rpc_statistics_file)
                            or not native.is_dir() or not public.is_dir()):
                        raise
                    diagnostic = {'errno':error.errno,'failure_file':rpc_statistics_file.name,
                        'read_started_monotonic':now,'read_failed_monotonic':time.monotonic(),
                        'previous_baseline_read_completed_monotonic':
                            baseline['read_completed_monotonic'] if baseline is not None else None}
                    missing = summary['prewindow_rpc_snapshot_missing']
                    missing['count'] += 1
                    if missing['first'] is None: missing['first'] = diagnostic
                    missing['last'] = diagnostic
                next_stats = now + 0.1
            events, native_done = wait_events(0.02, done_fd)
            baseline = baseline_before_start(baseline,candidate,events,public_watch)
            for wd,mask,name in events:
                if mask & BAD:
                    summary['flags'].append({'code':'inotify_integrity_event','mask':mask,'watch':wd})
                if wd == binary_watch and mask & IN_OPEN:
                    if not summary['window_ended']:
                        field = 'prover_opens_in_window' if summary['window_started'] else 'prover_opens_before_window'
                        summary[field] += 1
                if wd == public_watch and mask & IN_MOVED_TO:
                    if name == 'settlement-observation.json':
                        assert not summary['window_started'] and baseline is not None
                        summary['window_started'] = True
                        summary['observer_phase'] = 'observing-original-restart-window'
                        summary['window_start'] = {'marker':name,'monotonic':time.monotonic(),'rpc_baseline':baseline['value'],
                            'baseline_read_completed_monotonic':baseline['read_completed_monotonic'],
                            'baseline_confirmed_before_start_by_inotify_drain':True,
                            'existing_provers':running_provers(),'prover_identity':list(identity(program.stat()))}
                        sample_start.set(); save()
                    elif name == 'native-result.json':
                        with sample_lock:
                            sample_stop.set()
                            frozen_samples = dict(samples)
                        summary['window_ended'] = True
                        summary['window_end'] = {'marker':name,'monotonic':time.monotonic(),
                            'rpc_final':rpc_stats(),'prover_identity':list(identity(program.stat()))}
                        result = json.loads((public / name).read_text())
                        summary['native_status'] = result['status']
                        summary['native_real_process_restart'] = result.get('real_process_restart')
                        summary['native_result_sha256'] = file_sha(public / name)
            if native_done and not summary['window_ended']:
                raise ObservationIncomplete('native exited before the original end marker')
            if summary['prewindow_rpc_snapshot_missing']['count'] != missing_before:
                save()
        if not summary['window_started']:
            raise ObservationIncomplete('native ended without the original start marker')
        sample_stop.set()
        summary['observer_phase'] = 'checking-complete-window'
        summary['chain_samples'] = frozen_samples['count']; summary['chain_sample_errors'] = frozen_samples['errors']
        summary['first_anchored_snapshot_in_window'] = frozen_samples['first']
        summary['last_anchored_snapshot_in_window'] = frozen_samples['last']
        start,end = summary['window_start'],summary['window_end']
        before,after = start['rpc_baseline'],end['rpc_final']
        assert before['state'] == 'ready' and after['state'] == 'stopped'
        assert before['other_logged_methods'] == after['other_logged_methods'] == 0
        counts_before,counts_after = before['rpc_counts'],after['rpc_counts']
        raw_before,raw_after = counts_before.get('eth_sendRawTransaction'),counts_after.get('eth_sendRawTransaction')
        assert type(raw_before) is int and type(raw_after) is int and raw_before >= 7 and raw_after >= 7
        summary['write_counter_positive_control'] = {'method':'eth_sendRawTransaction',
            'minimum_completed_contract_calls':7,'before':raw_before,'after':raw_after}
        delta = {k:counts_after.get(k,0)-counts_before.get(k,0) for k in set(counts_before)|set(counts_after)}
        assert all(v >= 0 for v in delta.values())
        is_write = lambda k: k.startswith(('eth_send','eth_sign','personal_','evm_','miner_','debug_'))
        summary['write_rpc_delta'] = {k:v for k,v in sorted(delta.items()) if is_write(k)}
        assert all(v == 0 for v in summary['write_rpc_delta'].values()), 'write RPC activity in conservative restart interval'
        assert not start['existing_provers'] and summary['prover_opens_in_window'] == 0
        assert identity(original) == tuple(start['prover_identity']) == tuple(end['prover_identity'])
        assert not summary['flags'] and summary['native_status'] == 'PASS'
        assert summary['native_real_process_restart']['receiver_history_read'] is True
        assert frozen_samples['first'] is not None and frozen_samples['last'] is not None and frozen_samples['count'] >= 2
        assert frozen_samples['last']['accounts'] == frozen_samples['first']['accounts']
        assert all(a['nonce'] == a['pending_nonce'] for a in frozen_samples['first']['accounts'].values())
        os.close(fd); fd = -1
        summary['prover_inode_and_identity_unchanged'] = True
        summary['status'] = 'PASS'
        summary['observer_phase'] = 'complete'
        summary['claim'] = 'bounded original restart marker window: no new official host opens and no write RPC calls; two observed-window anchored account snapshots agree'
    except BaseException as error:
        summary['status'] = 'INCONCLUSIVE'; summary['failure_type'] = type(error).__name__
        summary['failure_file'] = Path(error.filename).name if getattr(error, 'filename', None) else None
        summary['failure_frames'] = [{'function':frame.name,'line':frame.lineno,'file':Path(frame.filename).name}
            for frame in traceback.extract_tb(error.__traceback__)]
    finally:
        sample_stop.set(); sample_start.set()
        if fd >= 0: os.close(fd)
        if ready_fd is not None: os.close(ready_fd)
        os.close(done_fd)
        save()
        print(json.dumps({'event':'restart_observer_result','status':summary['status'],
            'window_started':summary['window_started'],'window_ended':summary['window_ended'],
            'prover_opens_in_window':summary['prover_opens_in_window'],'flags':len(summary['flags'])}),flush=True)
    raise SystemExit(0 if summary['status'] == 'PASS' else 1)



def observe(configuration, run):
    global outer, native, public, rpc_statistics_file, out, config, program, expected, original, summary
    global opener, sequence, fd, binary_watch, public_watch, sample_start, sample_stop, sample_lock, samples
    os.umask(0o077)
    descriptors = [int(os.environ.pop(name)) for name in (READY_FD, DONE_FD)]
    if len(set(descriptors)) != 2 or any(value < 3 for value in descriptors):
        raise ValueError('invalid observer descriptors')
    for descriptor in descriptors: os.set_inheritable(descriptor, False)
    ready_fd, done_fd = descriptors
    fd = -1
    try:
        if not __debug__ or any('trash' in str(path).lower() for path in (configuration, run)):
            raise ValueError('forbidden observer mode or path')
        outer = run.resolve(strict=True)
        assert outer.is_relative_to(ARTIFACTS.resolve())
        if configuration != outer / 'configuration.json' or configuration.is_symlink():
            raise ValueError('observer requires this run configuration')
        metadata = configuration.stat()
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid() or metadata.st_mode & 0o077 or not 0 < metadata.st_size <= 16384:
            raise ValueError('observer requires an owner-only bounded configuration')
        with os.fdopen(os.open(configuration, os.O_RDONLY | os.O_NOFOLLOW), 'rb') as stream:
            configuration_bytes = stream.read(16385)
        config = json.loads(configuration_bytes)
        if (Path(config['paths']['repository']).resolve(strict=True) != ROOT
                or not re.fullmatch(r'[0-9a-f]{40}', config['source_commit'])
                or sys.platform != 'linux' or os.uname().machine != 'x86_64'
                or os.environ.get('TOKIO_WORKER_THREADS') != '2'):
            raise ValueError('observer requires the canonical source and Linux sandbox')
        native = outer / 'native'; public = native / 'public'
        rpc_statistics_file = native / 'deployment.json.rpc.json'
        out = outer / 'restart-observer'; out.mkdir(mode=0o700)
        program = Path(config['paths']['prover']); expected = config['sha256']['prover']
        if program.is_symlink() or not program.is_file():
            raise ValueError('observer requires the actual regular host')
        assert file_sha(program) == expected
        assert os.getuid() == outer.stat().st_uid
        membership = next(line[3:] for line in Path('/proc/self/cgroup').read_text().splitlines() if line.startswith('0::'))
        cgroup = Path('/sys/fs/cgroup') / membership.lstrip('/')
        assert (cgroup / 'memory.max').read_text().strip() == str(11 * 1024**3)
        assert (cgroup / 'memory.swap.max').read_text().strip() == '0'
        assert (cgroup / 'pids.max').read_text().strip() == '64'
        original = program.stat()
        summary = {'status':'RUNNING', 'source_commit':config['source_commit'],
            'scope':'read-only observation of the original native restart window',
            'uid':os.getuid(), 'gid':os.getgid(), 'prover_sha256':expected,
            'script_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
            'prover_opens_in_window':0, 'prover_opens_before_window':0,
            'calibration':{}, 'flags':[], 'window_started':False, 'window_ended':False,
            'chain_samples':0, 'chain_sample_errors':0, 'write_rpc_delta':None,
            'parameters_or_source_changed':False, 'proofs_generated_by_observer':0,
            'observer_phase':'calibration',
            'prewindow_rpc_snapshot_missing':{'count':0,'first':None,'last':None}}
        summary.update(run_binding_sha256=run_binding(outer, configuration_bytes),
                       configuration_sha256=hashlib.sha256(configuration_bytes).hexdigest(), native_result_sha256=None)
        while not public.is_dir():
            if select.select([done_fd], [], [], 0.02)[0]:
                raise ObservationIncomplete('native exited before creating fresh directories')
        if native.is_symlink() or public.is_symlink() or not native.is_dir():
            raise ValueError('invalid native observation directories')
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({})); sequence = 0
        libc = ctypes.CDLL(None, use_errno=True)
        libc.inotify_init1.argtypes = [ctypes.c_int]; libc.inotify_init1.restype = ctypes.c_int
        libc.inotify_add_watch.argtypes = [ctypes.c_int,ctypes.c_char_p,ctypes.c_uint32]
        libc.inotify_add_watch.restype = ctypes.c_int
        fd = libc.inotify_init1(os.O_NONBLOCK | os.O_CLOEXEC)
        assert fd >= 0, ctypes.get_errno()
        binary_watch = libc.inotify_add_watch(fd, os.fsencode(program), IN_OPEN | BAD)
        public_watch = libc.inotify_add_watch(fd, os.fsencode(public), IN_MOVED_TO | 0x400 | 0x800)
        assert binary_watch >= 0 and public_watch >= 0, ctypes.get_errno()
        sample_start, sample_stop = threading.Event(), threading.Event()
        sample_lock = threading.Lock()
        samples = {'first':None,'last':None,'count':0,'errors':0,'last_error_type':None}
    except BaseException:
        if fd >= 0: os.close(fd)
        os.close(ready_fd); os.close(done_fd)
        raise
    watch(ready_fd, done_fd)



def self_test():
    import ast
    import copy
    import socket
    import tempfile
    from types import SimpleNamespace
    from unittest.mock import patch
    def forbidden(*_args, **_kwargs):
        raise AssertionError('observer self-test attempted a process or socket')
    def stats(raw=7, state='ready', **counts):
        return {'state':state, 'rpc_count_source':'ganache method logger, including fixture and client requests',
                'rpc_counts':{'eth_sendRawTransaction':raw, **counts}, 'other_logged_methods':0}
    missing, unchanged = object(), object()
    tree = ast.parse(Path(__file__).read_text())
    initialize = next(node for node in tree.body if isinstance(node, ast.FunctionDef) and node.name == 'observe')
    assignment = next(node for node in ast.walk(initialize) if isinstance(node, ast.Assign)
                      and any(isinstance(target, ast.Name) and target.id == 'summary' for target in node.targets))
    results = []
    with patch.object(subprocess, 'Popen', forbidden), patch.object(socket, 'socket', forbidden):
        ARTIFACTS.mkdir(exist_ok=True)
        def run(name, initial, steps, *, expected_pass=False, read_error=None, boundaries=True,
                sample_count=2, changed_accounts=False, pending_nonce=False, native_status='PASS', history=True,
                statistics_mode=0o600, wrong_owner=False, statistics_kind='regular'):
            with tempfile.TemporaryDirectory(prefix='restart-observer-selftest-', dir=ARTIFACTS) as temporary:
                outer = Path(temporary); native = outer / 'native'; public = native / 'public'; public.mkdir(parents=True)
                target = native / 'deployment.json.rpc.json'; program = outer / 'inert-program'
                program.write_bytes(b'offline observer fixture, never executed')
                def replace(value):
                    if value is unchanged: return
                    if value is missing: target.unlink(missing_ok=True)
                    else:
                        target.write_text(value if isinstance(value, str) else json.dumps(value))
                        target.chmod(statistics_mode)
                replace(initial)
                if not boundaries: public.rmdir(); native.rmdir()
                tick, cursor, timeline = [0], [0], []
                def now(): tick[0] += 1; return float(tick[0])
                local_samples = {'first':None,'last':None,'count':0,'errors':0,'last_error_type':None}
                def populate():
                    accounts = {key:{'nonce':'0x1','pending_nonce':'0x2' if pending_nonce else '0x1',
                                     'native_balance':'0x1','token_balance':'0x5'} for key in ('S','R','P')}
                    first = {'monotonic':now(), 'accounts':accounts}; last = copy.deepcopy(first)
                    if changed_accounts: last['accounts']['P']['token_balance'] = '0x6'
                    local_samples.update(first=first if sample_count else None,
                                         last=last if sample_count else None, count=sample_count)
                class Event:
                    def __init__(self, callback=lambda: None): self.callback=callback
                    def set(self): self.callback()
                def events(_wait, _done):
                    assert cursor[0] < len(steps), 'offline event fixture exhausted'
                    item = steps[cursor[0]]; cursor[0] += 1
                    replace(item.get('snapshot', unchanged)); batch=[]
                    for event in item.get('events', ()):
                        if event == 'start': batch.append((102, IN_MOVED_TO, 'settlement-observation.json'))
                        elif event == 'end':
                            (public / 'native-result.json').write_text(json.dumps({'status':native_status,
                                'real_process_restart':{'receiver_history_read':history}}))
                            batch.append((102, IN_MOVED_TO, 'native-result.json'))
                        elif event == 'open': batch.append((101, IN_OPEN, ''))
                        elif event == 'overflow': batch.append((-1, 0x4000, ''))
                        elif event == 'watch-lost': batch.append((102, 0x8000, ''))
                        elif event == 'replace-host': program.write_bytes(b'changed offline host identity')
                        else: raise AssertionError('unknown offline event')
                    timeline.append(('drain',cursor[0],tuple(item.get('events',()))))
                    return batch, item.get('done',False)
                def save_stub():
                    value = globals()['summary']
                    timeline.append(('save',value['prewindow_rpc_snapshot_missing']['count'],value['window_started']))
                def info(arguments, **_options):
                    assert arguments == [str(program),'info']
                    return SimpleNamespace(returncode=0,stdout=json.dumps({'elfSha256':'offline-elf'}))
                original_lstat=Path.lstat
                def lstat(path,*args,**kwargs):
                    metadata=original_lstat(path,*args,**kwargs)
                    if path != target: return metadata
                    fields=list(metadata)
                    if wrong_owner: fields[4]=metadata.st_uid+1
                    if statistics_kind != 'regular':
                        kinds={'fifo':stat.S_IFIFO,'directory':stat.S_IFDIR,'symlink':stat.S_IFLNK}
                        fields[0]=kinds[statistics_kind] | statistics_mode
                        fields[6]=0
                    return type(metadata)(fields)
                values = {'outer':outer,'native':native,'public':public,'rpc_statistics_file':target,'program':program,
                    'original':program.stat(),'config':{'source_commit':'a'*40,'elf_sha256':'offline-elf','paths':{'r0vm':'inert'}},
                    'expected':'b'*64,'fd':99,'binary_watch':101,'public_watch':102,
                    'sample_lock':threading.Lock(),'samples':local_samples,
                    'sample_start':Event(populate),'sample_stop':Event(),
                    'threading':SimpleNamespace(Thread=lambda **_kwargs: SimpleNamespace(start=lambda:None)),
                    'time':SimpleNamespace(monotonic=now),'os':SimpleNamespace(getuid=os.getuid,getgid=os.getgid,
                        write=lambda descriptor,data:len(data),close=lambda descriptor:timeline.append(('close',descriptor))),
                    'subprocess':SimpleNamespace(run=info),'wait_events':events,
                    'read_events':lambda wait:[(101,IN_OPEN,'')],'running_provers':lambda:[],
                    'save':save_stub,'print':lambda *args,**kwargs:None}
                with patch.dict(globals(), values), patch.object(Path,'lstat',lstat):
                    exec(compile(ast.Module(body=[assignment],type_ignores=[]),'<observer-summary-fixture>','exec'),globals())
                    if read_error:
                        def fail():
                            timeline.append(('rpc_stats_refused',statistics_kind))
                            raise read_error(target)
                        globals()['rpc_stats'] = fail
                    try: watch(97,98)
                    except SystemExit as error: code=error.code
                    else: raise AssertionError('observer did not execute the real exit mapping')
                    result=copy.deepcopy(globals()['summary'])
                assert result['status'] == ('PASS' if expected_pass else 'INCONCLUSIVE'), (name,result)
                assert code == (0 if expected_pass else 1), (name,code)
                results.append({'case':name,'synthetic_status':result['status'],'synthetic_exit_code':code})
                return result,timeline
        start={'events':['start']}; end={'events':['end'],'snapshot':stats(state='stopped'),'done':True}
        normal=[{},start,end]
        result,_=run('exact_ENOENT_then_recovery',missing,[{'snapshot':stats()},{},start,end],expected_pass=True)
        assert result['prewindow_rpc_snapshot_missing']['count']==1
        result,timeline=run('ENOENT_and_start_keep_earlier_baseline',stats(),[{'snapshot':missing},
            {'events':['start'],'snapshot':stats()},end],expected_pass=True)
        assert result['window_start']['baseline_read_completed_monotonic'] == result['prewindow_rpc_snapshot_missing']['last']['previous_baseline_read_completed_monotonic']
        drain=next(i for i,item in enumerate(timeline) if item==('drain',2,('start',)))
        saved=next(i for i,item in enumerate(timeline) if item[0]=='save' and item[1:]==(1,True))
        assert drain<saved
        result,_=run('start_batch_cannot_mask_write',stats(),[{'snapshot':stats(8)},start,
            {'events':['end'],'snapshot':stats(8,'stopped'),'done':True}])
        assert result['window_start']['rpc_baseline']['rpc_counts']['eth_sendRawTransaction']==7
        assert result['write_rpc_delta']['eth_sendRawTransaction']==1
        run('no_earlier_baseline',missing,[start,end])
        run('first_candidate_and_start_same_batch',stats(),[start,end])
        for name,make_error in (
            ('other_path',lambda path: FileNotFoundError(errno.ENOENT,'missing',str(path.with_name('other.json')))),
            ('other_errno',lambda path: FileNotFoundError(errno.EIO,'io',str(path))),
            ('permission',lambda path: PermissionError(errno.EACCES,'denied',str(path)))):
            run(name,stats(),normal,read_error=make_error)
        run('bad_json','{',normal)
        run('bad_structure',[],normal)
        run('boundaries_missing',missing,normal,boundaries=False)
        run('final_ENOENT',stats(),[{},start,{'events':['end'],'snapshot':missing,'done':True}])
        run('final_bad_json',stats(),[{},start,{'events':['end'],'snapshot':'{','done':True}])
        run('window_missing_until_done',missing,[{'done':True}])
        run('end_before_start',stats(),[end])
        run('done_before_end',stats(),[{},start,{'done':True}])
        run('start_end_done_same_batch',stats(),[{}, {'events':['start','end'],'snapshot':stats(state='stopped'),'done':True}],expected_pass=True)
        for event in ('open','overflow','watch-lost','replace-host'):
            run(event,stats(),[{}, {'events':['start',event]},end])
        for name,options in (('one_sample',{'sample_count':1}),('no_samples',{'sample_count':0}),
                             ('changed_accounts',{'changed_accounts':True}),('pending_nonce',{'pending_nonce':True}),
                             ('native_fail',{'native_status':'FAIL'}),('missing_history',{'history':False})):
            run(name,stats(),normal,**options)
        run('raw_positive_control_missing',stats(6),[{},start,{'events':['end'],'snapshot':stats(6,'stopped'),'done':True}])
        run('other_write_method',stats(),[{},start,{'events':['end'],'snapshot':stats(state='stopped',eth_sign=1),'done':True}])
        run('decreasing_counter',stats(eth_call=2),[{},start,{'events':['end'],'snapshot':stats(state='stopped',eth_call=1),'done':True}])
        with patch.dict(globals(), fd=99), patch.object(select,'select',return_value=([98],[],[])), \
             patch.object(os,'read',return_value=b''), patch.dict(globals(),read_events=lambda wait:[('drained',wait)]):
            assert wait_events(0.02,98)==([('drained',0)],True)
        data=struct.pack('iIII',101,IN_OPEN,0,0)+struct.pack('iIII',-1,0x4000,0,0)
        with patch.dict(globals(),fd=99), patch.object(select,'select',return_value=([99],[],[])), \
             patch.object(os,'read',side_effect=[data,BlockingIOError()]):
            assert read_events(0)==[(101,IN_OPEN,''),(-1,0x4000,'')]
        assert run_binding(Path('/host/.artifacts/a'),b'config',Path('/host')) == run_binding(Path('/work/.artifacts/a'),b'config',Path('/work'))
        assert run_binding(Path('/work/.artifacts/a'),b'config',Path('/work')) != run_binding(Path('/work/.artifacts/b'),b'config',Path('/work'))
        first_empty_case=len(results)
        result,_=run('empty_after_valid_baseline',stats(),[{'snapshot':''},{'snapshot':stats()},start,end])
        assert result['failure_type']=='JSONDecodeError' and not result['window_started']
        result,_=run('empty_at_final_marker',stats(),[{},start,{'events':['end'],'snapshot':'','done':True}])
        assert result['failure_type']=='JSONDecodeError' and result['window_started']
        result,_=run('window_empty_through_final',stats(),[{},start,{'snapshot':''},{'events':['end'],'done':True}])
        assert result['failure_type']=='JSONDecodeError' and result['window_started']
        for name,invalid in (('malformed','{'),('whitespace',' \n')):
            run('nonempty_'+name+'_before_baseline',invalid,[{'snapshot':stats()},{},start,end])
            run('empty_then_nonempty_'+name,'',[{'snapshot':invalid},{'snapshot':stats()},{},start,end])
        for name,options in (('world_readable_empty',{'statistics_mode':0o644}),
                             ('group_readable_empty',{'statistics_mode':0o640}),
                             ('wrong_owner_empty',{'wrong_owner':True})):
            _,timeline=run(name,'',[{'snapshot':stats()},{},start,end],**options)
            assert not any(item[0]=='drain' for item in timeline), 'unsafe empty file entered the startup wait'
        for kind in ('fifo','directory','symlink'):
            _,timeline=run('unsafe_metadata_'+kind,'',[{'snapshot':stats()},{},start,end],statistics_kind=kind,
                read_error=lambda path:ObservationIncomplete('inert unsafe-file reader refused access'))
            assert not any(item[0]=='drain' for item in timeline), 'unsafe file type entered the startup wait'
        result,_=run('empty_without_baseline_and_start_same_batch','',[{'events':['start'],'snapshot':stats()},end])
        assert not result['window_started']
        print(json.dumps({'status':'F19_REJECTION_CONTROLS_PASS','scope':'synthetic empty-reservation rejection checks only',
            'legacy_case_count':first_empty_case,'cases':results[first_empty_case:],'external_processes':0,'network_sockets':0}))
        result,_=run('initial_empty_reservation_then_ready_recovers','',[{'snapshot':stats()},{},start,end],expected_pass=True)
        assert result['window_started'] and result['window_ended'] and result['prewindow_rpc_snapshot_missing']['count']==0
        assert result['window_start']['rpc_baseline']['state']=='ready' and result['window_end']['rpc_final']['state']=='stopped'
        for kind in ('fifo','directory','symlink'):
            _,timeline=run('unsafe_metadata_not_read_'+kind,'',[{'snapshot':stats()},{},start,end],statistics_kind=kind,
                read_error=lambda path:AssertionError('unsafe metadata must be rejected before rpc_stats'))
            assert not any(item[0] in ('rpc_stats_refused','drain') for item in timeline), 'unsafe object was read or entered the startup wait'
    print(json.dumps({'status':'SELF_TEST_PASS','scope':'synthetic observer behavior only',
        'external_processes':0,'network_sockets':0,'genuine_receipts_generated':0,'cases':results}))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test', action='store_true')
    parser.add_argument('--configuration', type=Path, help=argparse.SUPPRESS)
    parser.add_argument('--run-dir', type=Path, help=argparse.SUPPRESS)
    arguments = parser.parse_args()
    if arguments.self_test and not (arguments.configuration or arguments.run_dir):
        self_test()
    elif arguments.configuration and arguments.run_dir and not arguments.self_test:
        observe(arguments.configuration, arguments.run_dir)
    else:
        parser.error('provide --self-test or the canonical private observer invocation')
