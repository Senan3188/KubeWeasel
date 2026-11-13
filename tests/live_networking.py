"""Read-only checks against an existing saved route and local ingress tunnel.
Usage: python3 tests/live_networking.py http://127.0.0.1:7879 hello-weasel 8082
"""
import json
import sys
import urllib.request

base, name, port = sys.argv[1:]
from urllib.parse import quote
endpoint = base + '/api/apps/' + quote(name, safe='') + '/networking'
with urllib.request.urlopen(endpoint + '/diagnostics', timeout=15) as response:
    diagnostics = json.load(response)
assert not diagnostics['demo']
assert diagnostics['host']
checks = {c['id']: c for c in diagnostics['checks']}
assert checks['controller']['state'] == 'pass', checks['controller']
assert checks['route']['state'] == 'pass', checks['route']
request = urllib.request.Request(endpoint + '/test',
    data=json.dumps({'port': int(port), 'local': True}).encode(),
    headers={'content-type': 'application/json', 'x-kubeweasel-request': 'dashboard'})
with urllib.request.urlopen(request, timeout=15) as response:
    result = json.load(response)
assert result['state'] == 'pass', result
assert result['local'] is True
assert 'does not verify DNS' in result['scope']
print('PASS: class discovery, saved route, and local HTTP connection; browser DNS remains a separate check.')
