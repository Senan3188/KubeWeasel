"""Disposable kind-only installer lifecycle; never targets an existing cluster."""
import http.cookiejar
import json
import os
from pathlib import Path
import re
import subprocess
import time
import urllib.error
import urllib.request

CTX = 'kind-kubeweasel-access'
NS = 'kw-access-check'
ROOT = Path(__file__).resolve().parents[1]
ORIGIN = 'http://dashboard.example.internal:17887'
HOST = ORIGIN.split('://')[1]
EMAIL = 'owner@example.test'
PASSWORD = 'disposable-private-access-password'


def run(*args, data=None):
    result = subprocess.run(args, input=data, text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(f'{args[0]} {args[1]} failed (output withheld to protect setup credentials)')
    return result.stdout


def kube(*args, data=None):
    return run('kubectl', '--context', CTX, '-n', NS, *args, data=data)


def installer(command, *args):
    return run('bash', str(ROOT / 'scripts/install.sh'), command, '--context', CTX, '--namespace', NS, '--timeout', '5m', *args)


assert os.environ.get('KUBEWEASEL_RUN_PRIVATE_ACCESS_TEST') == '1'
assert run('kubectl', 'config', 'current-context').strip() == CTX
kube('create', 'namespace', NS)  # Fail if a previous namespace exists.
nodes = json.loads(kube('get', 'nodes', '-o', 'json'))['items']
primary = next(n for n in nodes if n['metadata']['name'].endswith('control-plane'))
node = primary['metadata']['name']
ip = next(a['address'] for a in primary['status']['addresses'] if a['type'] == 'InternalIP')
private_args = ('--private-http', ORIGIN, '--listen-address', ip, '--chart', str(ROOT / 'charts/kubeweasel'), '--image-repository', 'kubeweasel', '--image-tag', 'access-test')
cookies = http.cookiejar.CookieJar()
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), urllib.request.HTTPCookieProcessor(cookies))
base = 'http://127.0.0.1:17886'
host = '127.0.0.1:7878'
csrf = ''


def api(path, body=None, extra=None, expected=200, method=None):
    headers = {'Host': host, 'content-type': 'application/json', 'x-kubeweasel-request': 'dashboard', 'x-kubeweasel-csrf': csrf}
    if extra:
        headers.update(extra)
    request = urllib.request.Request(base + path, data=None if body is None else json.dumps(body).encode(), headers=headers, method=method)
    try:
        response = opener.open(request, timeout=15)
    except urllib.error.HTTPError as error:
        assert error.code == expected, (path, error.code, expected)
        return {}
    assert response.status == expected, (path, response.status, expected)
    assert 'Strict-Transport-Security' not in response.headers
    return json.load(response)



def wait_for_entrypoint():
    # Pod readiness precedes kube-proxy/EndpointSlice convergence on other nodes.
    # Poll a safe GET through the actual gateway before sending login mutations.
    for _ in range(60):
        try:
            response = opener.open(urllib.request.Request(base + '/healthz', headers={'Host': host}), timeout=6)
            assert response.status == 200 and response.read() == b'ok'
            return
        except urllib.error.HTTPError as error:
            if error.code not in (502, 503, 504):
                raise
        except OSError:
            pass
        time.sleep(.5)
    raise RuntimeError('Direct entry point did not recover after rollout')


def login():
    global csrf
    csrf = api('/api/auth/login', {'email': EMAIL, 'password': PASSWORD})['csrf']


def uid(kind, name):
    return json.loads(kube('get', kind, name, '-o', 'json'))['metadata']['uid']


forward = None
try:
    # Place the old account volume on the worker, then expose through the primary.
    # This proves enabling access does not try to move node-local account storage.
    kube('cordon', node)
    installer('install', '--version', '0.2.1')
    print('Published v0.2.1 installed on worker.', flush=True)
    kube('uncordon', node)
    old_pod = json.loads(kube('get', 'pods', '-l', 'app=kubeweasel', '-o', 'json'))['items'][0]
    storage_node = old_pod['spec']['nodeName']
    assert storage_node != node
    forward = subprocess.Popen(['kubectl', '--context', CTX, '-n', NS, 'port-forward', 'deployment/kubeweasel', '17886:7878'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(60):
        try:
            api('/api/auth/status')
            break
        except OSError:
            time.sleep(.5)
    setup_token = re.search(r'first-run setup token: ([a-f0-9]+)', kube('logs', 'deployment/kubeweasel', '--container', 'dashboard')).group(1)
    csrf = api('/api/auth/setup', {'email': EMAIL, 'password': PASSWORD, 'setup_token': setup_token})['csrf']
    project = api('/api/projects', {'name': 'Preserved project', 'description': 'Access upgrade check'})['project']['id']
    claim = uid('pvc', 'kubeweasel-data')
    key_path = '/data/kubeweasel.db.cluster-key'
    def key_digest():
        return kube('exec', 'deployment/kubeweasel', '-c', 'dashboard', '--', 'sha256sum', key_path).split()[0]
    original_key = key_digest()
    # Reproduce storage drivers adding fsGroup read/write bits on remount.
    kube('exec', 'deployment/kubeweasel', '-c', 'dashboard', '--', 'chmod', '660', key_path)

    api('/api/apps', {'name': 'retained-app', 'image': 'nginx:stable', 'port': 8080, 'replicas': 0, 'project_id': project})
    workload = uid('deployment', 'retained-app')
    public_route = {'host': 'public.example.test', 'path': '/', 'ingress_class': 'traefik', 'tls_mode': 'existing', 'tls_secret': 'retained-route-tls'}
    api('/api/apps/retained-app/domain', public_route)
    legacy_ingress = uid('ingress', 'retained-app')
    kube('apply', '-f', '-', data=json.dumps({'apiVersion': 'v1', 'kind': 'PersistentVolumeClaim', 'metadata': {'name': 'retained-app-data'}, 'spec': {'storageClassName': '', 'accessModes': ['ReadWriteOnce'], 'resources': {'requests': {'storage': '1Mi'}}}}))
    app_claim = uid('pvc', 'retained-app-data')
    forward.terminate()
    forward.wait(timeout=10)
    forward = None
    output = installer('upgrade', *private_args)
    print('Private gateway upgrade reached readiness.', flush=True)
    assert 'Dashboard ready: ' + ORIGIN in output
    assert 'No port-forward or SSH tunnel is needed.' in output
    assert uid('pvc', 'kubeweasel-data') == claim
    assert key_digest() == original_key
    assert kube('exec', 'deployment/kubeweasel', '-c', 'dashboard', '--', 'stat', '-c', '%a', key_path).strip() == '600'
    new_pod = json.loads(kube('get', 'pods', '-l', 'app=kubeweasel', '-o', 'json'))['items'][0]
    assert new_pod['spec']['nodeName'] == storage_node
    base = f'http://{ip}:17887'
    host = HOST
    cookies.clear()
    api('/api/projects', expected=401)
    login()
    assert any(p['id'] == project for p in api('/api/projects')['projects'])
    api('/api/projects', extra={'Host': 'evil.example:17887'}, expected=403)
    api('/api/projects', {'name': 'Blocked'}, {'Origin': 'http://evil.example:17887'}, expected=403)
    api('/api/projects', {'name': 'Blocked'}, {'x-kubeweasel-csrf': ''}, expected=403)
    def config():
        return api('/api/apps/retained-app/config')['config']
    cfg = config()
    assert len(cfg['domains']) == 1 and cfg['domains'][0]['id'] == 'retained-app'
    assert cfg['domains'][0]['tls_secret'] == 'retained-route-tls'
    original_spec = json.loads(kube('get', 'deployment', 'retained-app', '-o', 'json'))['spec']
    internal = {'host': 'app.saturn.internal', 'path': '/', 'ingress_class': 'traefik', 'tls_mode': 'http', 'tls_secret': ''}
    route_id = api('/api/apps/retained-app/domains', internal)['id']
    assert len(config()['domains']) == 2
    api('/api/apps', {'name': 'other-app', 'image': 'nginx:stable', 'port': 8080, 'replicas': 0, 'project_id': project})
    api('/api/apps/other-app/domains/' + route_id, internal, expected=404)
    api('/api/apps/other-app/domains/' + route_id, {}, method='DELETE', expected=404)

    assert uid('ingress', 'retained-app') == legacy_ingress
    assert json.loads(kube('get', 'deployment', 'retained-app', '-o', 'json'))['spec'] == original_spec
    api('/api/apps/retained-app/domains', internal, expected=400)
    internal['host'] = 'internal.example.test'
    api('/api/apps/retained-app/domains/' + route_id, internal)
    assert api('/api/apps/retained-app/https?domain=' + route_id)['host'] == internal['host']
    assert api('/api/apps/retained-app/networking/diagnostics?domain=' + route_id)['host'] == internal['host']
    assert config()['domain']['tls_secret'] == 'retained-route-tls'
    api('/api/apps/retained-app/port', {'port': 3000})
    service = json.loads(kube('get', 'service', 'retained-app', '-o', 'json'))
    assert service['spec']['ports'][0]['port'] == 8080
    assert service['spec']['ports'][0]['targetPort'] == 3000
    for route in ['retained-app', route_id]:
        ingress = json.loads(kube('get', 'ingress', route, '-o', 'json'))
        backend = ingress['spec']['rules'][0]['http']['paths'][0]['backend']['service']
        assert backend == {'name': 'retained-app', 'port': {'number': 8080}}
    api('/api/apps/retained-app/domains/' + route_id, {}, method='DELETE')
    assert len(config()['domains']) == 1 and config()['domain']['host'] == public_route['host']
    assert uid('ingress', 'retained-app') == legacy_ingress
    print('Legacy public TLS route preserved; second-domain create/edit/delete, per-domain diagnostics and stable Service port passed.', flush=True)
    assert ORIGIN in installer('open')
    assert 'port-forward' not in installer('open')
    # The gateway and backend can both restart without a terminal or tunnel.
    kube('rollout', 'restart', 'deployment/kubeweasel-access')
    kube('rollout', 'status', 'deployment/kubeweasel-access', '--timeout=3m')
    kube('exec', 'deployment/kubeweasel', '-c', 'dashboard', '--', 'chmod', '660', key_path)
    kube('rollout', 'restart', 'deployment/kubeweasel')
    kube('rollout', 'status', 'deployment/kubeweasel', '--timeout=3m')
    wait_for_entrypoint()
    assert key_digest() == original_key
    assert kube('exec', 'deployment/kubeweasel', '-c', 'dashboard', '--', 'stat', '-c', '%a', key_path).strip() == '600'
    login()
    print('Direct login, guards and independent restarts passed.', flush=True)
    installer('uninstall')
    assert uid('pvc', 'kubeweasel-data') == claim
    assert uid('deployment', 'retained-app') == workload
    assert uid('pvc', 'retained-app-data') == app_claim
    assert uid('ingress', 'retained-app') == legacy_ingress
    installer('install', *private_args)
    cookies.clear()
    login()
    assert any(p['id'] == project for p in api('/api/projects')['projects'])
    assert uid('pvc', 'kubeweasel-data') == claim
    assert uid('deployment', 'retained-app') == workload
    assert uid('pvc', 'retained-app-data') == app_claim
    assert uid('ingress', 'retained-app') == legacy_ingress
    assert key_digest() == original_key
    print('Private access: old-release upgrade, cross-node retained storage, direct login, Host/Origin/CSRF guards, restarts and retained-data reinstall passed.')
    # Explicit account reset: only the disposable account PVC, never app data.
    installer('uninstall')
    kube('delete', 'pvc', 'kubeweasel-data', '--wait=true')
    installer('install', *private_args)
    cookies.clear()
    assert uid('pvc', 'kubeweasel-data') != claim
    assert uid('deployment', 'retained-app') == workload
    assert uid('pvc', 'retained-app-data') == app_claim
    assert uid('ingress', 'retained-app') == legacy_ingress
    assert api('/api/auth/status')['setup_required'] is True
    fresh_key = key_digest()
    assert fresh_key != original_key
    setup_token = re.search(r'first-run setup token: ([a-f0-9]+)', kube('logs', 'deployment/kubeweasel', '--container', 'dashboard')).group(1)
    csrf = api('/api/auth/setup', {'email': EMAIL, 'password': PASSWORD, 'setup_token': setup_token})['csrf']
    kube('exec', 'deployment/kubeweasel', '-c', 'dashboard', '--', 'chmod', '660', key_path)
    kube('rollout', 'restart', 'deployment/kubeweasel')
    kube('rollout', 'status', 'deployment/kubeweasel', '--timeout=3m')
    wait_for_entrypoint()
    login()
    assert key_digest() == fresh_key
    assert kube('exec', 'deployment/kubeweasel', '-c', 'dashboard', '--', 'stat', '-c', '%a', key_path).strip() == '600'
    print('Explicit fresh account install and private-key permission recovery after restart passed; app workload retained.')

except Exception:
    print(kube('get', 'pods,pvc', '-o', 'wide'))
    print(kube('get', 'events', '--sort-by=.lastTimestamp'))
    # This tokenless gateway never runs owner setup or logs request bodies.
    diagnostic = subprocess.run(['kubectl', '--context', CTX, '-n', NS, 'logs', 'deployment/kubeweasel-access', '--container', 'access', '--tail=30'], text=True, capture_output=True)
    print(diagnostic.stdout)
    raise
finally:
    if forward:
        forward.terminate()
        forward.wait(timeout=10)
    # Only this explicitly disposable namespace is ever removed.
    kube('delete', 'namespace', NS, '--wait=false')
