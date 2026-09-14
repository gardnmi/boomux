"""Exercise the real gateway with a fake Tailscale CLI; never publish a live route.

Requires a running compatible Boomux daemon and BOOMUX_WEB_GATEWAY pointing to
its built webgpu_gateway example. No Shells or Workspaces are created/attached.
"""
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import urllib.request
import urllib.error


def request(port, host, origin=None):
    headers = {'Host': host}
    if origin:
        headers['Origin'] = origin
    req = urllib.request.Request(f'http://127.0.0.1:{port}/api/desktop' if origin else f'http://127.0.0.1:{port}/',
                                 headers=headers, data=b'{}' if origin else None)
    try:
        return urllib.request.urlopen(req, timeout=3).status
    except urllib.error.HTTPError as error:
        return error.code


with tempfile.TemporaryDirectory() as temporary:
    root = Path(temporary)
    existing = {'Web': {
        'test.tailnet.ts.net:443': {'Handlers': {'/': {'Proxy': 'http://127.0.0.1:3737'}}},
        'test.tailnet.ts.net:4097': {'Handlers': {'/': {'Proxy': 'http://127.0.0.1:4097'}}},
    }}
    (root / 'state').write_text(json.dumps(existing))
    fake = root / 'tailscale' 
    fake.write_text('''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
root = Path(os.environ['FAKE_TAILSCALE'])
args = sys.argv[1:]
with (root / 'commands').open('a') as log: log.write(json.dumps(args) + '\\n')
if args == ['status', '--json']:
    print(json.dumps({'BackendState':'Running', 'Self':{'Online':True, 'DNSName':'test.tailnet.ts.net.'}}))
elif args == ['serve', 'status', '--json']:
    print((root / 'state').read_text() if (root / 'state').exists() else '{}')
elif '--bg' in args:
    state = json.loads((root / 'state').read_text())
    port = next(arg.split('=')[1] for arg in args if arg.startswith('--https='))
    state['Web'][f'test.tailnet.ts.net:{port}'] = {'Handlers':{'/':{'Proxy':args[-1]}}}
    (root / 'state').write_text(json.dumps(state))
elif args[-1] == 'off':
    state = json.loads((root / 'state').read_text())
    port = next(arg.split('=')[1] for arg in args if arg.startswith('--https='))
    del state['Web'][f'test.tailnet.ts.net:{port}']
    (root / 'state').write_text(json.dumps(state))
else: sys.exit(1)
''')
    fake.chmod(0o700)
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    env = dict(os.environ, PATH=f'{root}:{os.environ["PATH"]}', FAKE_TAILSCALE=str(root), POC_PORT=str(port))
    process = subprocess.Popen([os.environ['BOOMUX_WEB_GATEWAY'], '--desktop', '--tailscale'],
                               env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    try:
        ready = json.loads(process.stdout.readline())
        assert ready == {'url': 'https://test.tailnet.ts.net:8443'}, ready
        assert request(port, f'127.0.0.1:{port}') == 200
        assert request(port, 'test.tailnet.ts.net:8443') == 200
        assert request(port, 'attacker.example') == 403
        assert request(port, 'test.tailnet.ts.net:8443', 'https://attacker.example') == 403
        assert request(port, 'test.tailnet.ts.net:8443', f'http://127.0.0.1:{port}') == 403
        assert request(port, 'test.tailnet.ts.net:8443', 'https://test.tailnet.ts.net:8443') != 403
        # A second publisher must fail before touching the live route.
        before = (root / 'commands').read_text()
        second = subprocess.run([os.environ['BOOMUX_WEB_GATEWAY'], '--desktop', '--tailscale'],
                                env=env, input='', capture_output=True, text=True, timeout=10)
        assert second.returncode != 0
        assert (root / 'commands').read_text() == before
    finally:
        process.stdin.close()
        process.wait(timeout=10)
    assert process.returncode == 0
    assert json.loads((root / 'state').read_text()) == existing
    commands = [json.loads(line) for line in (root / 'commands').read_text().splitlines()]
    assert ['serve', '--https=8443', '--set-path=/', 'off'] in commands
    assert not any('reset' in command or 'funnel' in command for command in commands)
print('Tailscale sharing: readiness, Host/Origin, duplicate startup, and EOF cleanup passed')
