"""Connection checks called only by the isolated kind installer lifecycle."""
import json
import os
import time


def exercise(api, kube, project):
    assert os.environ.get('KUBEWEASEL_RUN_PRIVATE_ACCESS_TEST') == '1'
    for app in ['secrets-backend', 'secrets-worker']:
        api('/api/apps', {'name': app, 'image': 'nginx:stable', 'port': 80, 'replicas': 1, 'project_id': project})
    connections = []
    for name, kind in [('Postgres', 'postgresql'), ('MySQL', 'mysql')]:
        value = 'disposable-' + kind + '-credential'
        connection = api('/api/projects/' + project + '/connections', {'name': name, 'kind': kind, 'project_id': project, 'values': {'password': value}})['connection']
        assert value not in json.dumps(connection)
        connections.append(connection)
        for app in (['secrets-backend', 'secrets-worker'] if kind == 'postgresql' else ['secrets-backend']):
            current = next(c for c in api('/api/apps/' + app + '/connections')['connections'] if c['id'] == connection['id'])
            api('/api/apps/' + app + '/connections/' + connection['id'], {'version': current['version'], 'workload_version': current['workload_version'], 'mappings': {kind.upper() + '_PASSWORD': 'password'}})
    for app in ['secrets-backend', 'secrets-worker']:
        kube('rollout', 'status', 'deployment/' + app, '--timeout=3m')
    assert kube('exec', 'deployment/secrets-backend', '--', 'printenv', 'POSTGRESQL_PASSWORD').strip() == 'disposable-postgresql-credential'
    assert kube('exec', 'deployment/secrets-backend', '--', 'printenv', 'MYSQL_PASSWORD').strip() == 'disposable-mysql-credential'
    pg = connections[0]
    endpoint = '/api/projects/' + project + '/connections/' + pg['id']
    rotated = api(endpoint, {'name': 'Postgres', 'kind': 'postgresql', 'project_id': project, 'version': pg['version'], 'values': {'password': 'disposable-rotated-credential'}})['connection']
    catalog = api('/api/projects/' + project + '/connections')['connections']
    current = next(c for c in catalog if c['id'] == pg['id'])
    assert len(current['consumers']) == 2 and all(c['restart_required'] for c in current['consumers'])
    assert not any(c['restart_required'] for c in next(c for c in catalog if c['id'] == connections[1]['id'])['consumers'])
    api(endpoint, {'version': rotated['version']}, expected=409, method='DELETE')
    result = api(endpoint + '/restart', {'version': rotated['version']})
    assert not result['failed'] and set(result['restarted']) == {'secrets-backend', 'secrets-worker'}
    for app in ['secrets-backend', 'secrets-worker']:
        kube('rollout', 'status', 'deployment/' + app, '--timeout=3m')
        assert kube('exec', 'deployment/' + app, '--', 'printenv', 'POSTGRESQL_PASSWORD').strip() == 'disposable-rotated-credential'
    assert kube('exec', 'deployment/secrets-backend', '--', 'printenv', 'MYSQL_PASSWORD').strip() == 'disposable-mysql-credential'
    # Controller status may follow readiness; only the observed current rollout clears pending.
    for _ in range(40):
        catalog = api('/api/projects/' + project + '/connections')['connections']
        if all(c['status'] == 'Current' for connection in catalog for c in connection['consumers']):
            break
        time.sleep(.5)
    else:
        raise AssertionError('Latest connection rollout did not become current')
    assert 'disposable-rotated-credential' not in json.dumps(catalog)
    state = [(c['id'], json.loads(kube('get', 'secret', c['id'], '-o', 'json'))['metadata']['uid']) for c in connections]
    print('Live Secrets: multiple databases per app, shared consumers, write-only responses, rotation, guarded deletion and ready rollout status passed.', flush=True)
    return state


def verify_retained(api, kube, project, state):
    catalog = api('/api/projects/' + project + '/connections')['connections']
    for name, uid in state:
        assert json.loads(kube('get', 'secret', name, '-o', 'json'))['metadata']['uid'] == uid
        connection = next(c for c in catalog if c['id'] == name)
        assert connection['consumers'] and not connection['usage_error']
        assert all(not c['restart_required'] for c in connection['consumers'])
    print('Connection Secrets and applied revision markers survived dashboard restart/reinstall.', flush=True)
