import datetime, hashlib, json, os, pathlib, shutil, socket, subprocess, sys, time, uuid
ROOT=pathlib.Path('/home/ubuntu/poly')
TARGET=ROOT/'target/release/late_resolution_winner_trading'
JOURNAL=ROOT/'results/late-resolution-winner-trading/server-low-latency-008-5m/price-cap-study.ndjson'
NEW_SHA='60d0f147fa7cd2d4b7d1d21949b6a8d59b1d71244d50ba7081e4b0c7029cfc95'
OLD_SHA='2cb4465c7d9bcf439f99433003937383d4e639dae1c740c685339d9bcb328dfb'
PANE='0:1.0'
def sha(p):
    h=hashlib.sha256()
    with open(p,'rb') as f:
        for b in iter(lambda:f.read(1024*1024),b''): h.update(b)
    return h.hexdigest()
def cmd(*args): return subprocess.check_output(args,text=True).strip()
def process(pid):
    p=pathlib.Path('/proc')/str(pid)
    args=(p/'cmdline').read_bytes().rstrip(b'\0').decode().split('\0')
    stat=(p/'stat').read_text().rsplit(')',1)[1].split()
    return dict(pid=int(pid),args=args,ppid=int(stat[1]),start_ticks=int(stat[19]),exe_sha256=sha(p/'exe'))
def find(mode):
    result=[]
    for p in pathlib.Path('/proc').iterdir():
        if not p.name.isdigit(): continue
        try:
            args=(p/'cmdline').read_bytes().rstrip(b'\0').decode().split('\0')
            if len(args)>1 and pathlib.Path(args[0]).name=='late_resolution_winner_trading' and args[1]==mode:
                result.append(process(p.name))
        except (OSError,UnicodeError): pass
    return result
def snapshot():
    sockpath=pathlib.Path('/tmp')/f'lrw-tui-{os.getuid()}'/(hashlib.sha256(str(JOURNAL.resolve()).encode()).hexdigest()[:20]+'.sock')
    with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as sock:
        sock.settimeout(2); sock.connect(str(sockpath)); raw=b''
        while not raw.endswith(b'\n'):
            chunk=sock.recv(65536-len(raw))
            if not chunk: break
            raw+=chunk
            if len(raw)>=65536: break
    s=json.loads(raw)
    assert pathlib.Path(s['journal']).resolve()==JOURNAL.resolve()
    return {k:s[k] for k in ['pid','run_id','started_ms','captured_ms','loop_ms','scan_ms','scans','phase','market_ws_connected']}
def save(d):
    path=pathlib.Path(d['deployment'])/'receipt.json'
    tmp=path.with_suffix('.json.tmp')
    tmp.write_text(json.dumps(d,ensure_ascii=False,indent=2)+'\n'); os.replace(tmp,path)
def engine_guard(d):
    engines=find('dry-run'); assert len(engines)==1, engines
    assert engines[0]==d['before']['engine'], 'Engine identity changed'
    assert sha(ROOT/'.env')==d['before']['env_sha256'], '.env changed'
    assert sha(ROOT/'scripts/run_late_resolution_winner_price_cap.sh')==d['before']['wrapper_sha256'], 'Wrapper changed'
    return engines[0]
mode=sys.argv[1]
if mode=='prepare':
    assert sha(TARGET)==OLD_SHA, 'Unexpected current binary'
    engines=find('dry-run'); viewers=find('tui')
    assert len(engines)==1 and len(viewers)==1, (engines,viewers)
    pane_pid=int(cmd('tmux','display-message','-p','-t',PANE,'#{pane_pid}'))
    assert viewers[0]['ppid']==pane_pid, 'Viewer is not a child of the expected pane'
    assert any('server-low-latency-008-5m/price-cap-study.ndjson' in a for a in viewers[0]['args'])
    live=snapshot(); assert live['pid']==engines[0]['pid']
    dep=ROOT/'deployments'/('winner-tui-20261104-'+uuid.uuid4().hex[:8]); dep.mkdir(mode=0o700)
    backup=dep/'late_resolution_winner_trading.previous'; shutil.copy2(TARGET,backup); assert sha(backup)==OLD_SHA
    d=dict(deployment=str(dep),stage=str(TARGET.parent/('.late_resolution_winner_trading.tui-'+uuid.uuid4().hex[:8]+'.candidate')),target=str(TARGET),new_sha256=NEW_SHA,old_sha256=OLD_SHA,phase='prepared',prepared_at=datetime.datetime.now(datetime.timezone.utc).isoformat(),before=dict(engine=engines[0],viewer=viewers[0],live=live,env_sha256=sha(ROOT/'.env'),wrapper_sha256=sha(ROOT/'scripts/run_late_resolution_winner_price_cap.sh'),pane=cmd('tmux','display-message','-p','-t',PANE,'#{session_name}:#{window_index}.#{pane_index} #{pane_width}x#{pane_height}')))
    (dep/'viewer.before.txt').write_text(cmd('tmux','capture-pane','-p','-t',PANE)+'\n')
    save(d); print(json.dumps(d))
elif mode=='replace':
    d=json.loads((pathlib.Path(sys.argv[2])/'receipt.json').read_text()); assert d['phase']=='prepared'
    engine_guard(d); assert sha(TARGET)==OLD_SHA
    stage=pathlib.Path(d['stage']); assert sha(stage)==NEW_SHA; stage.chmod(0o755)
    assert '--lang en|zh' in cmd(str(stage),'tui','--help')
    assert json.loads(cmd(str(stage),'price-cap-info'))==json.loads(cmd(str(TARGET),'price-cap-info')), 'Study specification changed'
    with stage.open('rb') as f: os.fsync(f.fileno())
    os.replace(stage,TARGET)
    dir_fd=os.open(str(TARGET.parent),os.O_RDONLY); os.fsync(dir_fd); os.close(dir_fd)
    assert sha(TARGET)==NEW_SHA; engine_guard(d)
    d['phase']='replaced'; d['replaced_at']=datetime.datetime.now(datetime.timezone.utc).isoformat(); save(d)
    print(json.dumps({'phase':d['phase'],'sha256':sha(TARGET),'engine_pid':d['before']['engine']['pid'],'backup':str(pathlib.Path(d['deployment'])/'late_resolution_winner_trading.previous')}))
elif mode=='restart-viewer':
    d=json.loads((pathlib.Path(sys.argv[2])/'receipt.json').read_text()); assert d['phase']=='replaced'; engine_guard(d)
    viewers=find('tui'); assert viewers==[d['before']['viewer']], 'Viewer identity changed'
    assert int(cmd('tmux','display-message','-p','-t',PANE,'#{pane_pid}'))==viewers[0]['ppid']
    subprocess.check_call(['tmux','send-keys','-t',PANE,'Escape']); time.sleep(.25)
    subprocess.check_call(['tmux','send-keys','-t',PANE,'q'])
    deadline=time.monotonic()+5
    while find('tui') and time.monotonic()<deadline: time.sleep(.15)
    assert not find('tui'), 'Old viewer did not exit'
    engine_guard(d)
    command='bash /home/ubuntu/poly/scripts/run_late_resolution_winner_price_cap.sh tui server-low-latency-008'
    subprocess.check_call(['tmux','send-keys','-t',PANE,'-l',command]); subprocess.check_call(['tmux','send-keys','-t',PANE,'Enter'])
    deadline=time.monotonic()+10
    while time.monotonic()<deadline:
        viewers=find('tui')
        capture=cmd('tmux','capture-pane','-p','-t',PANE)
        if len(viewers)==1 and viewers[0]['exe_sha256']==NEW_SHA and '1 Overview' in capture and 'Q Exit' in capture: break
        time.sleep(.3)
    else: raise AssertionError('New English viewer not ready: '+capture)
    (pathlib.Path(d['deployment'])/'viewer.en.txt').write_text(capture+'\n')
    subprocess.check_call(['tmux','send-keys','-t',PANE,'l']); time.sleep(.5)
    chinese=cmd('tmux','capture-pane','-p','-t',PANE); assert '累计时间' in chinese and 'Q退出' in chinese, chinese
    (pathlib.Path(d['deployment'])/'viewer.zh.txt').write_text(chinese+'\n')
    subprocess.check_call(['tmux','send-keys','-t',PANE,'l']); time.sleep(.5)
    capture=cmd('tmux','capture-pane','-p','-t',PANE); assert 'Cumulative period' in capture and 'Q Exit' in capture
    engine_guard(d)
    live=snapshot(); assert live['run_id']==d['before']['live']['run_id'] and live['pid']==d['before']['engine']['pid']
    assert live['loop_ms']>=d['before']['live']['loop_ms']
    d.update(phase='complete',completed_at=datetime.datetime.now(datetime.timezone.utc).isoformat(),after=dict(viewer=find('tui')[0],live=live,binary_sha256=sha(TARGET),env_sha256=sha(ROOT/'.env'),default_language='en'))
    save(d); print(json.dumps(d))
else: raise AssertionError(mode)
