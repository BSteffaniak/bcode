#!/usr/bin/env python3
"""Offline PTY acceptance: native first-use with immutable symlink configuration.
Build with cargo build -p bcode --bin bcode --features app,distribution first.
Optional argument selects an explicit test executable; state and HOME are isolated.
BCODE_TEST_DIFFERENT_CWD=1 starts the daemon in another workspace.
BCODE_TEST_NATIVE_LOGIN=1 exercises signed-out native terminal login.
"""
import os, pathlib, tempfile, subprocess, sys, json, pty, fcntl, termios, struct, select, time, signal, hashlib
repo=pathlib.Path(__file__).resolve().parents[1]
binary=pathlib.Path(sys.argv[1]).resolve() if len(sys.argv)>1 else repo/'target/debug/bcode'
root=pathlib.Path(tempfile.mkdtemp(prefix='bcode-picker-smoke-')).resolve()
native=root/'native';native.mkdir();(native/'scenario').write_text('multi_turn');(native/'application-idle-seconds').write_text('120')
exe=root/'claude';source=(repo/'packages/bcode/tests/fixtures/harness/claude_native.py').read_text();source=source.replace('session = next(arg.split("=", 1)[1] for arg in sys.argv\n                   if arg.startswith(("--session-id=", "--resume=")))','session = next((arg.split("=", 1)[1] for arg in sys.argv if arg.startswith(("--session-id=", "--resume="))), str(uuid.uuid4()))');exe.write_text('#!'+sys.executable+'\n'+source.split('\n',1)[1]);exe.chmod(0o700)
login_test=os.environ.get('BCODE_TEST_NATIVE_LOGIN')=='1'
if login_test: (native/'auth-login-required').touch()
q=lambda x:json.dumps(str(x))
config=root/'selected.toml'; config.write_text(f'''[plugins]
default = "none"
enabled = ["bcode.claude-code", "bcode.harness", "bcode.fake-provider", "bcode.default-agents"]
[model]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-echo"
[model.prompt_cache]
mode = "off"
''')
# Mirror file-backed provider presets: relative shared policy beside logical symlinks.
pool_target=root/'immutable-pools.toml'
pool_target.write_text('schema_version=1\n[auth.pools]\n')
pool_target.chmod(0o444)
(root/'pools.toml').symlink_to(pool_target)
config.write_text("auth_pool_config='pools.toml'\n"+config.read_text())
env={k:v for k,v in os.environ.items() if not k.startswith(('BCODE_','ANTHROPIC_','OPENAI_'))}
env.update(PATH=str(root)+':/usr/bin:/bin',HOME=str(root/'home'),CLAUDE_CONFIG_DIR=str(native),BCODE_NO_SESSION_RESURRECTION='1',BCODE_CONFIG=str(config),XDG_CONFIG_HOME=str(root/'config'),BCODE_NO_ONBOARD='1',TERM='xterm-256color')
real_config=root/'immutable.toml';config.rename(real_config);real_config.chmod(0o444);config.symlink_to(real_config);original_config=real_config.read_bytes()
(root/'home').mkdir()
args=[str(binary),'--state-root',str(root/'state')]
print('isolated state',root,flush=True);print('test artifact sha256',hashlib.sha256(binary.read_bytes()).hexdigest(),flush=True)
proc=None
try:
 if os.environ.get('BCODE_TEST_DIFFERENT_CWD') == '1':
  bootstrap=subprocess.run(args+['server','start'],env=env,cwd=root/'home',capture_output=True,text=True,timeout=45)
  assert bootstrap.returncode == 0, 'daemon bootstrap failed: '+bootstrap.stderr
 master,slave=pty.openpty();fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',38,140,0,0))
 proc=subprocess.Popen(args+['--new'],env=env,cwd=root,stdin=slave,stdout=slave,stderr=slave,start_new_session=True);os.close(slave)
 data=bytearray()
 def drain(seconds):
  end=time.monotonic()+seconds
  while time.monotonic()<end:
   if select.select([master],[],[],.1)[0]:
    try:data.extend(os.read(master,65536))
    except OSError:break
 drain(45)
 if os.environ.get('BCODE_TEST_DIFFERENT_CWD') == '1':
  probe=subprocess.run(args+['plugin','call','--daemon','bcode.managed-session/v1','registration_snapshot',json.dumps({'schema_version':1,'operation':'registration_snapshot','request_json':'{}'}),'--json'],env=env,cwd=root/'home',capture_output=True,text=True,timeout=30)
  assert probe.returncode == 0, 'cross-workspace registration read failed: '+probe.stderr
  print('PASS: registration snapshot from another workspace',flush=True)
 os.write(master,b'\x1b[200~/model\x1b[201~');drain(1);os.write(master,b'\x1b');drain(.5);os.write(master,b'\r');drain(22)
 (root/'terminal.raw').write_bytes(data)
 text=data.decode(errors='replace')
 assert 'setup/sign in' in text or 'Checking available models' in text, text[-4000:]
 print('PASS: /model reaches execution picker from provider chat',flush=True)
 os.write(master,b'\x1bOQ');drain(12)
 (root/'setup.raw').write_bytes(data)
 os.write(master,b'\r');drain(12)
 (root/'review.raw').write_bytes(data)
 assert 'local registration state' in data.decode(errors='replace'), 'review missing'
 os.write(master,b'\x1b');drain(15)
 assert not (root/'state/plugins/bcode.harness/native-registrations.json').exists(), 'cancel saved registration'
 assert real_config.read_bytes()==original_config and config.is_symlink(), 'cancel changed configuration'
 print('PASS: cancelling registration leaves configuration and registration state untouched',flush=True)
 os.write(master,b'\x1bOQ');drain(12);os.write(master,b'\r');drain(12)
 os.write(master,b'\r');drain(12)
 (root/'saved.raw').write_bytes(data)
 assert (root/'state/plugins/bcode.harness/native-registrations.json').exists(), 'registration not saved'
 assert config.is_symlink(), 'config link replaced'
 assert real_config.read_text().encode()==original_config, 'immutable base changed'
 print('PASS: F2 registers into durable state with immutable symlink base',flush=True)
 if login_test:
  (native/'scenario').write_text('terminal_auth')
  os.write(master,b'\x1b[B');drain(.5);os.write(master,b'\r');drain(12)
  (root/'login.raw').write_bytes(data)
  assert (native/'terminal-login.effect').exists(), 'native terminal login not started'
  (native/'terminal-login.release').touch();drain(18)
  assert (native/'terminal-login.finished').exists(), 'native terminal login did not complete'
  assert not (native/'auth-login-required').exists(), 'native login readiness did not change'
  (native/'scenario').write_text('multi_turn')
  print('PASS: native terminal sign-in completes without editing declarative configuration',flush=True)
 else:
  os.write(master,b'\r');drain(15)
 os.write(master,b'/');drain(.4);os.write(master,b'\x1b[200~claude-fixture\x1b[201~');drain(3);text=data.decode(errors='replace')
 assert 'Claude Code' in text, text[-4000:]
 print('PASS: configured Claude Code appears alongside provider',flush=True)
 os.write(master,b'\r');drain(22)
 native_text=data.decode(errors='replace')
 (root/'terminal-selected.raw').write_bytes(data)
 assert 'could not be resolved' not in native_text and 'Could not select' not in native_text, native_text[-7000:]
 print('PASS: native model selection admitted and returned to conversation UI',flush=True)
 os.write(master,b'\x1b[200~turn-1\x1b[201~');drain(1);os.write(master,b'\r');drain(8)
 (root/'terminal-turn.raw').write_bytes(data)
 assert (native/'turn-1.effect').exists(), 'native submission was not admitted'
 assert 'reply turn-1' in data.decode(errors='replace'), 'native result missing'
 print('PASS: native prompt and result',flush=True)
 os.write(master,b'\x1b[200~/model\x1b[201~');drain(1);os.write(master,b'\x1b');drain(.5);os.write(master,b'\r');drain(8)
 assert data.decode(errors='replace').count('setup/sign in') >= 2, 'native picker did not reopen'
 print('PASS: picker reachable again after native transition',flush=True)
 os.write(master,b'\x1b');drain(1);os.write(master,b'\x04');drain(2)
 if proc.poll() is None: proc.terminate()
 proc.wait(timeout=8)
 subprocess.run(args+['server','stop','--force','--yes'],env=env,cwd=root,capture_output=True,timeout=30,check=True)
 master,slave=pty.openpty();fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',38,140,0,0))
 proc=subprocess.Popen(args+['--new'],env=env,cwd=root,stdin=slave,stdout=slave,stderr=slave,start_new_session=True);os.close(slave)
 data=bytearray();drain(35)
 os.write(master,b'\x1b[200~/model\x1b[201~');drain(1);os.write(master,b'\x1b');drain(.5);os.write(master,b'\r');drain(18)
 (root/'restart.raw').write_bytes(data)
 assert 'claude-fixture' in data.decode(errors='replace'), 'saved registration not available after daemon restart'
 print('PASS: durable registration rediscovered after daemon restart',flush=True)
finally:
 if proc and proc.poll() is None:
  os.killpg(proc.pid,signal.SIGTERM)
  try:proc.wait(timeout=8)
  except subprocess.TimeoutExpired:os.killpg(proc.pid,signal.SIGKILL)
 subprocess.run(args+['server','stop','--force','--yes'],env=env,cwd=root,capture_output=True,timeout=30)
 print('smoke artifacts',root,flush=True)
