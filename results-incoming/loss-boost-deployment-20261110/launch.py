"""Start and verify a separate loss-boost PAPER session beside 008."""
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import socket
import subprocess as sp
import sys
import time

root=Path('/home/ubuntu/poly')
dep=Path(__file__).resolve().parent
os.chdir(root)
session='winner-loss-boost'
batch='server-loss-boost-001'
bin_path=root/'target/release/late_resolution_winner_loss_boost'
launcher=root/'scripts/run_late_resolution_winner_loss_boost.sh'
report=root/'scripts/late_resolution_winner_loss_boost_report.py'
original=root/'target/release/late_resolution_winner_trading'
original_launcher=root/'scripts/run_late_resolution_winner_price_cap.sh'
original_j=root/'results/late-resolution-winner-trading/server-low-latency-008-5m/price-cap-study.ndjson'
new_j=root/'results/late-resolution-winner-trading'/f'{batch}-5m'/'loss-boost-study.ndjson'

def sha(path):
    with path.open('rb') as f: return hashlib.file_digest(f,'sha256').hexdigest()

def live(journal):
    key=hashlib.sha256(str(journal.resolve()).encode()).hexdigest()[:20]
    with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as s:
        s.settimeout(2)
        s.connect(f'/tmp/lrw-tui-{os.getuid()}/{key}.sock')
        with s.makefile('rb') as f:
            a=json.loads(f.readline(128*1024))
    assert Path(a['journal']).resolve()==journal.resolve()
    return a

def tmux(*args):
    return sp.check_output(['tmux',*args],text=True)

def send(window,argv):
    tmux('send-keys','-t',f'{session}:{window}','-l',shlex.join(argv))
    tmux('send-keys','-t',f'{session}:{window}','Enter')

def wait_for(fn,secs,label):
    deadline=time.monotonic()+secs
    last=''
    while time.monotonic()<deadline:
        try:
            if v:=fn(): return v
        except (OSError,ValueError,AssertionError) as e: last=str(e)
        time.sleep(.5)
    raise RuntimeError(f'{label} timed out: {last}')

assert not new_j.parent.exists(), 'batch already exists'
assert sp.run(['tmux','has-session','-t','='+session],capture_output=True).returncode != 0
before=live(original_j)
old_pid=before['pid']
start_ticks=lambda pid: Path(f'/proc/{pid}/stat').read_text().rsplit(')',1)[1].split()[19]
original_start=start_ticks(old_pid)
guard={p:sha(p) for p in (original,original_launcher,root/'.env')}
staged=[(dep/'winner.bin',bin_path,0o755),(dep/'launcher.sh',launcher,0o755),(dep/'report.py',report,0o644)]
expected=json.loads((dep/'manifest.json').read_text())
for src,dst,mode in staged:
    assert not dst.exists() and sha(src)==expected[src.name], str(dst)
    src.chmod(mode)
spec=json.loads(sp.check_output([str(staged[0][0]),'loss-boost-info']))
assert spec['paper_only'] and spec['multiplier']==10 and spec['duration_secs']==3600
for mode in ('price-cap-info','operator-info'):
    assert json.loads(sp.check_output([str(staged[0][0]),mode]))==json.loads(sp.check_output([str(original),mode]))
for src,dst,mode in staged:
    # Exclusive installation; the existing 008 files are never targets.
    os.link(src,dst)
    src.unlink()

base=['env',f'LATE_WINNER_BINARY={bin_path}', 'LATE_RESOLUTION_WINNER_LOSS_BOOST_BASE_USDC=5',
      'LATE_RESOLUTION_WINNER_LOSS_BOOST_CAPITAL_USDC=100','bash',str(launcher)]
tmux('new-session','-d','-s',session,'-n','engine','-x','120','-y','34','-c',str(root))
send('engine',base+['loss-boost',batch])
print('new PAPER engine launched; 008 was not restarted',flush=True)
def ready():
    v=live(new_j)
    if not v['market_ws_connected']: return None
    assert v['pid']!=old_pid and v.get('operator') is None
    assert sha(Path(f"/proc/{v['pid']}/exe"))==expected['winner.bin']
    return v
first=wait_for(ready,45,'loss-boost engine / market websocket')
tmux('new-window','-d','-t',session,'-n','tui','-c',str(root))
send('tui',base+['loss-boost-tui',batch,'--lang','zh'])
def viewer():
    v=tmux('capture-pane','-p','-t',f'{session}:tui')
    return v if '亏后加码' in v and 'BTC $5 | ETH $5' in v and 'UTC' in v else None
screen=wait_for(viewer,12,'loss-boost TUI')
after=wait_for(lambda: (v if (v:=ready()) and v['loop_ms']>first['loop_ms'] else None),8,'new engine progress')
old=live(original_j)
assert old['pid']==old_pid and old['run_id']==before['run_id'] and start_ticks(old_pid)==original_start
assert old['loop_ms']>before['loop_ms']
assert all(sha(p)==digest for p,digest in guard.items())
reg=json.loads(new_j.open().readline())
original_config=json.loads(original_j.open().readline())['configuration']
changed={'study_id','arms','base_order_usdc','boost_multiplier','boost_duration_secs','max_order_usdc','initial_cash_per_arm'}
assert all(k in reg['configuration'] and reg['configuration'][k]==v for k,v in original_config.items() if k not in changed)
assert reg['configuration']['base_order_usdc']=='5' and reg['configuration']['max_order_usdc']=='50'
sys.path.insert(0,str(root/'scripts'))
import late_resolution_winner_loss_boost_report as audit
result=wait_for(lambda: audit.analyze(new_j.read_bytes()),5,'new journal audit')
receipt=dict(at=dt.datetime.now(dt.timezone.utc).isoformat(),batch=batch,session=session,
             engine_pid=after['pid'],run_id=after['run_id'],ws_connected=after['market_ws_connected'],
             loop_advanced=True,scans=after['scans'],candidate_pid=old_pid,candidate_run_id=old['run_id'],
             candidate_loop_advanced=True,original_files_unchanged=True,same_candidate_configuration=True,
             window_start_ms=reg['forward_window_start_ms'],base_budget=result['base_budget'],
             boosted_budget=result['boosted_budget'],capital=result['capital'],
             assets={k:v['current_budget'] for k,v in result['by_asset'].items()},
             attempts=result['total']['attempts'],loss_triggers=len(result['loss_triggers']),
             deployment=str(dep),binary_sha256=expected['winner.bin'])
(dep/'receipt.json').write_text(json.dumps(receipt,indent=2)+'\n')
(dep/'viewer.txt').write_text(screen)
print(json.dumps(receipt),flush=True)
