"""Render direct-access boundaries using the real Helm CLI (3 and 4 in CI)."""
import os
import subprocess
import unittest
from pathlib import Path

CHART = str(Path(__file__).resolve().parents[1] / 'charts/kubeweasel')

@unittest.skipUnless(os.environ.get('KUBEWEASEL_REAL_HELM_TEST') == '1', 'real Helm checks are opt-in')
class PrivateAccess(unittest.TestCase):
    def render(self, *extra):
        return subprocess.run(['helm', 'template', 'dashboard', CHART, '--namespace', 'access-test', *extra], text=True, capture_output=True)

    def private(self, *extra):
        return self.render('--set', 'privateHttp.enabled=true,privateHttp.origin=http://deploy.example.internal:17887,privateHttp.bindAddress=100.73.28.50,privateHttp.nodeName=saturn-primary,privateHttp.port=17887', *extra)

    def test_default_stays_local(self):
        result = self.render()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn('hostNetwork: true', result.stdout)
        self.assertNotIn('KUBEWEASEL_PRIVATE_ORIGIN', result.stdout)
        self.assertIn('127.0.0.1:7878', result.stdout)

    def test_gateway_is_separate_from_account_storage(self):
        result = self.private()
        self.assertEqual(result.returncode, 0, result.stderr)
        documents = result.stdout.split('---')
        gateway = next(d for d in documents if 'kind: Deployment' in d and 'name: dashboard-access' in d)
        backend = next(d for d in documents if 'kind: Deployment' in d and 'name: dashboard-access' not in d)
        self.assertIn('hostNetwork: true', gateway)
        self.assertIn('automountServiceAccountToken: false', gateway)
        self.assertNotIn('persistentVolumeClaim:', gateway)
        self.assertNotIn('hostNetwork:', backend)
        self.assertNotIn('nodeAffinity:', backend)
        self.assertIn('claimName:', backend)
        self.assertIn('$(KUBEWEASEL_POD_IP):7878', backend)
        self.assertIn('proxy_set_header Host $http_host;', result.stdout)
        self.assertIn('listen 100.73.28.50:17887;', result.stdout)
        self.assertIn('ghcr.io/nginx/nginx-unprivileged@sha256:', gateway)
        self.assertIn('kind: Service\n', result.stdout)

    def test_invalid_access_fails_closed(self):
        for value in ('privateHttp.bindAddress=0.0.0.0', 'privateHttp.bindAddress=8.8.8.8', 'privateHttp.port=7878', 'publicOrigin=https://public.example.com'):
            with self.subTest(value=value):
                self.assertNotEqual(self.private('--set', value).returncode, 0)
