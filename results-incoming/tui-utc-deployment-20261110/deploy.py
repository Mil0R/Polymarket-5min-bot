"""Deploy the UTC TUI release to the existing 008 PAPER engine and viewer."""
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import socket
import subprocess as sp
import sys
import time

ROOT = Path('/home/ubuntu/poly')
DEP = Path(__file__).resolve().parent
TARGET = ROOT / 'target/release/late_resolution_winner_trading'
STAGED = DEP / 'late_resolution_winner_trading.next'
BACKUP = DEP / 'late_resolution_winner_trading.previous'
WRAPPER = ROOT / 'scripts/run_late_resolution_winner_price_cap.sh'
BATCH = 'server-low-latency-008'
JOURNAL = ROOT / 'results/late-resolution-winner-trading' / (BATCH+'-5m') / 'price-cap-study.ndjson'
OPERATOR = JOURNAL.with_name('operator-paper.ndjson')
SOCKET = Path('/tmp') / ('lrw-tui-'+str(os.getuid())) / (hashlib.sha256(str(JOURNAL.resolve()).encode()).hexdigest()[:20]+'.sock')
OLD = '0ca5f94dc963e40f27455d0f555f63059bec160426627139104f14d8d6a5901d'
NEW = '63dff6104bcc9809fa17437ac1623645032233186d4f853a0d19140efdf10a82'
PANES = {'dry-run':'0:0.0','tui':'0:1.0'}
os.chdir(ROOT)
sys.path.insert(0, str(ROOT/'scripts'))
import late_resolution_winner_price_cap_report as fixed
import late_resolution_winner_operator_report as operator

def sha(p):
    with Path(p).open('rb') as f:
        return hashlib.file_digest(f, 'sha256').hexdigest()

def cmd(*a):
    return sp.check_output(a, text=True).strip()

def utc():
    return dt.datetime.now(dt.timezone.utc).isoformat()

def procs(mode):
    rows = []
    for p in Path('/proc').iterdir():
        if not p.name.isdigit():
            continue
        try:
            args = (p/'cmdline').read_bytes().rstrip(b'\0').decode().split('\0')
            if len(args) < 2 or Path(args[0]).name != TARGET.name or args[1] != mode:
                continue
            stat = (p/'stat').read_text().rsplit(')',1)[1].split()
            rows.append(dict(pid=int(p.name), args=args, parent=int(stat[1]), start_ticks=int(stat[19])))
        except (OSError,UnicodeError):
            pass
    return rows

def live():
    with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as s:
        s.settimeout(2)
        s.connect(str(SOCKET))
        with s.makefile('rb') as f:
            value=json.loads(f.readline(128*1024))
    assert Path(value['journal']).resolve() == JOURNAL.resolve()
    return value

def wait_for(check, timeout, label):
    until=time.monotonic()+timeout
    last=''
    while time.monotonic()<until:
        try:
            r=check()
            if r:
                return r
        except (AssertionError,OSError,ValueError,sp.CalledProcessError) as e:
            last=str(e)
        time.sleep(.25)
    raise RuntimeError(f'{label}: timed out ({last})')

def capture():
    return cmd('tmux','capture-pane','-p','-t',PANES['tui'])

def keys(mode,*args):
    sp.check_call(['tmux','send-keys','-t',PANES[mode],*args])

def stop_viewer():
    if procs('tui'):
        keys('tui','Escape')
        time.sleep(.2)
        keys('tui','q')
        wait_for(lambda: not procs('tui'),5,'Viewer exit')

def stop_engine():
    rows=procs('dry-run')
    if rows:
        assert len(rows)==1
        os.kill(rows[0]['pid'],signal.SIGINT)
        wait_for(lambda: not procs('dry-run'),45,'Engine graceful exit')

def start(mode, extra=()):
    assert not procs(mode), f'Duplicate {mode}'
    pane=PANES[mode]
    wait_for(lambda: cmd('tmux','display-message','-p','-t',pane,'#{pane_current_command}') in ('bash','zsh','sh'),5,'Shell foreground')
    line=shlex.join(['bash',str(WRAPPER),mode,BATCH,*extra])
    keys(mode,'-l',line)
    keys(mode,'Enter')

def install(src):
    with src.open('rb') as f:
        os.fsync(f.fileno())
    os.replace(src,TARGET)
    fd=os.open(TARGET.parent,os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)

def prefix(p):
    raw=p.read_bytes()
    return {'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest()}

def check_prefix(p, record):
    with p.open('rb') as f:
        raw=f.read(record['bytes'])
    assert len(raw)==record['bytes'] and hashlib.sha256(raw).hexdigest()==record['sha256'], str(p)

def safe_operator(s):
    return {k:s[k] for k in ['settings','capital','deposits']}

def save(phase, **kwargs):
    receipt.update(phase=phase,updated_at=utc(),**kwargs)
    tmp=DEP/'receipt.tmp'
    tmp.write_text(json.dumps(receipt,ensure_ascii=False,indent=2)+'\n')
    os.replace(tmp,DEP/'receipt.json')
    print(phase, flush=True)

assert sha(TARGET)==OLD and sha(STAGED)==NEW
assert STAGED.stat().st_dev==TARGET.stat().st_dev
assert sha(WRAPPER)=='5c6f57f16cf21758c235ece16c8e83b2dd66ed4918c25a853b365adca28e0caf'
STAGED.chmod(0o755)
for mode in ('price-cap-info','operator-info'):
    assert json.loads(cmd(str(STAGED),mode))==json.loads(cmd(str(TARGET),mode))
assert 'Times and ledger dates use UTC' in cmd(str(STAGED),'tui','--help')
before={m:procs(m) for m in PANES}
for m,rows in before.items():
    assert len(rows)==1 and rows[0]['parent']==int(cmd('tmux','display-message','-p','-t',PANES[m],'#{pane_pid}'))
    assert sha(f"/proc/{rows[0]['pid']}/exe")==OLD
engine=before['dry-run'][0]
old_viewer=before['tui'][0]['args']
assert Path(old_viewer[old_viewer.index('--journal')+1]).resolve()==JOURNAL
assert old_viewer[2]=='--journal'
viewer_args=old_viewer[4:]
def engine_env(pid):
    env=dict(kv.split(b'=',1) for kv in Path(f'/proc/{pid}/environ').read_bytes().split(b'\0') if b'=' in kv)
    return env[b'DRY_RUN'],Path(os.fsdecode(env[b'LATE_RESOLUTION_WINNER_PRICE_CAP_STUDY_FILE'])).resolve()
assert engine_env(engine['pid'])==(b'true',JOURNAL)
oldlive=live()
assert oldlive['pid']==engine['pid'] and not oldlive['operator']['failed']
shutil.copy2(TARGET,BACKUP)
assert sha(BACKUP)==OLD
(DEP/'viewer.before.txt').write_text(capture()+'\n')
receipt=dict(deployment=str(DEP),batch=BATCH,binary_sha256=NEW,old_binary_sha256=OLD,
             before=before,old_run_id=oldlive['run_id'],env_sha256=sha(ROOT/'.env'),
             prior_prefixes={p.name:prefix(p) for p in (JOURNAL,OPERATOR)})
save('prepared')
installed=False
engine_ready=False
try:
    assert {m:procs(m) for m in PANES}==before
    stop_viewer()
    stop_engine()
    save('stopped')
    raw=JOURNAL.read_bytes()
    fixed.analyze(raw)
    rawop=OPERATOR.read_bytes()
    op=operator.analyze(rawop)
    expected=safe_operator(op)
    assert expected==safe_operator(oldlive['operator'])
    stopped={p.name:prefix(p) for p in (JOURNAL,OPERATOR)}
    for p in (JOURNAL,OPERATOR):
        check_prefix(p,receipt['prior_prefixes'][p.name])
    install(STAGED)
    installed=True
    assert sha(TARGET)==NEW
    save('installed',stopped_prefixes=stopped,operator_before_restart=expected)
    start('dry-run')
    def ready():
        a=live()
        p=procs('dry-run')
        assert len(p)==1 and p[0]['pid']==a['pid'] and a['run_id']!=oldlive['run_id']
        assert sha(f"/proc/{a['pid']}/exe")==NEW
        assert engine_env(a['pid'])==(b'true',JOURNAL)
        assert safe_operator(a['operator'])==expected and not a['operator']['failed']
        assert a['market_ws_connected'] and a['loop_ms']>=a['started_ms']
        return a
    first=wait_for(ready,35,'New engine ready')
    engine_ready=True
    save('engine-ready',engine_pid=first['pid'],new_run_id=first['run_id'])
    start('tui',viewer_args)
    pattern=re.compile(r'(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}) UTC')
    def viewer_ready():
        text=capture()
        m=pattern.search(text)
        if m and 'PAPER | Operator' in text and 'Q退出' in text:
            return text,m[1]
    text,clock=wait_for(viewer_ready,15,'UTC viewer ready')
    text,new_clock=wait_for(lambda: (v if (v:=viewer_ready()) and v[1]!=clock else None),6,'UTC clock advancing')
    clock_dt=dt.datetime.strptime(new_clock,'%Y-%m-%d %H:%M:%S').replace(tzinfo=dt.timezone.utc)
    assert abs((dt.datetime.now(dt.timezone.utc)-clock_dt).total_seconds())<5
    rows=procs('tui')
    assert len(rows)==1 and sha(f"/proc/{rows[0]['pid']}/exe")==NEW
    after=wait_for(lambda: (v if (v:=ready())['loop_ms']>first['loop_ms'] else None),8,'Engine loop advancing')
    for p in (JOURNAL,OPERATOR):
        check_prefix(p,stopped[p.name])
    assert sha(ROOT/'.env')==receipt['env_sha256']
    (DEP/'viewer.after.txt').write_text(text+'\n')
    save('complete',viewer_pid=rows[0]['pid'],utc_clock=[clock,new_clock],operator_after_restart=safe_operator(after['operator']),
         market_ws_connected=after['market_ws_connected'],loop_advanced=True,scans=after['scans'],
         journal_prefixes_unchanged=True,env_unchanged=True)
except Exception as error:
    save('failed',error=str(error))
    if not engine_ready:
        if installed:
            stop_engine()
            restore=DEP/'rollback'
            shutil.copy2(BACKUP,restore)
            install(restore)
        if not procs('dry-run'):
            start('dry-run')
            wait_for(live,35,'Original engine recovery')
    if not procs('tui'):
        start('tui',viewer_args)
    raise
