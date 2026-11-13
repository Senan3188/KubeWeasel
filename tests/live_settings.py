"""Opt-in live test: creates and removes only a dedicated test application.
Run: python3 tests/live_settings.py http://127.0.0.1:7879 kubeweasel-test
"""
import json
import subprocess
import sys
import urllib.error
import urllib.request

from auth_client import auth_headers
base, namespace = sys.argv[1:]
headers = auth_headers(base)
name = 'kw-settings-check'
created = False

def api(path, body=None, expected=200):
    req = urllib.request.Request(base + '/api/apps/' + path,
        data=None if body is None else json.dumps(body).encode(),
        headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=30) as response:
            status, data = response.status, json.load(response)
    except urllib.error.HTTPError as error:
        status, data = error.code, json.load(error)
    assert status == expected, (path, status, data)
    return data

def kube(*args):
    return subprocess.check_output(['kubectl', '-n', namespace, *args], text=True)

try:
    # Create through the dashboard's existing deployment API.
    req = urllib.request.Request(base + '/api/apps',
        data=json.dumps({'name':name,'image':'nginx:stable','port':80,'replicas':1}).encode(),
        headers=headers)
    with urllib.request.urlopen(req, timeout=30):
        created = True
    api(name+'/general', {'image':'nginx:stable','replicas':1})
    api(name+'/port', {'port':8080})
    assert api(name+'/config')['config']['port'] == 8080
    api(name+'/port', {'port':80})
    api(name+'/environment', {'env':{'APP_MODE':'test','MULTILINE':'one\ntwo'}})
    assert api(name+'/config')['config']['env']['MULTILINE'] == 'one\ntwo'
    api(name+'/environment', {'env':{'APP_MODE':'updated'}})
    assert api(name+'/config')['config']['env'] == {'APP_MODE':'updated'}
    api(name+'/resources', {'cpu_request':'50m','cpu_limit':'500m','memory_request':'32Mi','memory_limit':'128Mi'})
    assert api(name+'/config')['config']['memory_limit'] == '128Mi'
    domain={'host':'kw-settings-check.invalid','path':'/','ingress_class':'kubeweasel-test','tls_secret':''}
    api(name+'/domain', domain)
    assert api(name+'/config')['config']['domain']['host'] == domain['host']
    domain['host']=''
    api(name+'/domain', domain)
    assert api(name+'/config')['config']['domain']['host'] == ''
    api(name+'/resources', {'cpu_request':'2','cpu_limit':'1','memory_request':'32Mi','memory_limit':'128Mi'},400)
    kube('rollout','status','deployment/'+name,'--timeout=60s')
    api(name+'/forward', {'local_port':18081})
    with urllib.request.urlopen('http://127.0.0.1:18081', timeout=20) as response:
        assert b'Welcome to nginx' in response.read()
    api(name+'/forward/stop', {})
    assert api(name+'/config')['forward'] is None
    print('PASS: live settings, environment replacement, resources, Ingress lifecycle, real nginx port forwarding.')
finally:
    if created:
        try:
            api(name+'/forward/stop', {})
        finally:
            kube('delete','deployment,service,ingress',name,'--ignore-not-found=true','--wait=false')
