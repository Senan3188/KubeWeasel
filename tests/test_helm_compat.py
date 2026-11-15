"""Real Helm CLI parsing check; uses only an intentionally unreachable local API."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


@unittest.skipUnless(os.environ.get('KUBEWEASEL_REAL_HELM_TEST') == '1', 'opt-in real Helm CLI contract check')
class HelmCompatibilityTests(unittest.TestCase):
    def test_release_listing_uses_supported_flags_and_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / 'kubectl'
            binary.write_text('#!/bin/sh\nexit 0\n')
            binary.chmod(0o755)
            config = root / 'config'
            config.write_text('''apiVersion: v1
kind: Config
current-context: installer-test
clusters:
- name: installer-test
  cluster:
    server: http://127.0.0.1:1
contexts:
- name: installer-test
  context:
    cluster: installer-test
    user: installer-test
users:
- name: installer-test
  user: {}
''')
            repo = Path(__file__).resolve().parents[1]
            env = {**os.environ, 'PATH': str(root) + os.pathsep + os.environ['PATH'], 'KUBECONFIG': str(config)}
            result = subprocess.run(['bash', str(repo / 'scripts/install.sh'), 'install', '--context', 'installer-test', '--chart', str(repo / 'charts/kubeweasel')], env=env, capture_output=True, text=True, timeout=30)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('Cannot inspect Helm releases', result.stderr)
            self.assertNotIn('unknown flag', result.stderr)
            self.assertNotIn('Installing release', result.stdout)


if __name__ == '__main__':
    unittest.main()
