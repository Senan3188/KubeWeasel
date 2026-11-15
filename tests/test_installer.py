"""Installer behavior checks with isolated command doubles; never contacts a cluster."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'scripts/install.sh'
MOCK = r'''#!/usr/bin/env python3
import json, os, pathlib, sys
args=sys.argv[1:]; tool=pathlib.Path(sys.argv[0]).name
with open(os.environ['COMMAND_LOG'],'a') as f: f.write(json.dumps([tool]+args)+'\n')
if tool=='helm':
 if 'version' in args: print('v3.17.3')
 elif 'show' in args: print('name: kubeweasel\nversion: 0.2.0')
 elif 'list' in args and os.environ.get('INSTALLED')=='1': print('kubeweasel')
 elif 'get' in args: print('kind: PersistentVolumeClaim\nmetadata:\n  annotations:\n    helm.sh/resource-policy: '+os.environ.get('POLICY','keep'))
 elif 'upgrade' in args and os.environ.get('FAIL_READY')=='1': sys.exit(1)
elif tool=='kubectl':
 if 'current-context' in args: print('test-context')
 elif '/readyz' in args: print('ok')
 elif 'pvc' in args and os.environ.get('CLAIM')=='1': print('persistentvolumeclaim/kubeweasel-data')
elif tool=='curl':
 target=pathlib.Path(args[args.index('-o')+1]); url=args[args.index('-o')-1]
 if url.endswith('SHA256SUMS'): target.write_text(os.environ['CHECKSUM']+'  kubeweasel-0.2.0.tgz\n')
 else: target.write_bytes(b'chart-fixture')
'''

class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / 'bin'; self.bin.mkdir()
        for tool in ['helm', 'kubectl', 'curl']:
            p = self.bin / tool; p.write_text(MOCK); p.chmod(0o755)
        self.chart = self.root / 'chart'; self.chart.mkdir()
        self.log = self.root / 'commands'
        self.env = {**os.environ, 'PATH': str(self.bin)+os.pathsep+os.environ['PATH'],
                    'COMMAND_LOG': str(self.log), 'CHECKSUM': hashlib.sha256(b'chart-fixture').hexdigest()}
    def run_installer(self, command='install', download=False, extra_args=(), **env):
        args=['bash', str(SCRIPT), command, '--context', 'test-context']
        if not download: args += ['--chart', str(self.chart)]
        args += list(extra_args)
        result=subprocess.run(args, env={**self.env, **env}, capture_output=True, text=True)
        calls=[json.loads(s) for s in self.log.read_text().splitlines()] if self.log.exists() else []
        return result, calls
    def test_install_pins_context_and_waits(self):
        result,calls=self.run_installer()
        self.assertEqual(result.returncode,0,result.stderr)
        upgrade=next(c for c in calls if c[0]=='helm' and 'upgrade' in c)
        self.assertIn('--wait',upgrade); self.assertIn('image.tag=0.2.0',upgrade)
        self.assertIn('--create-namespace',upgrade)
        for c in calls:
            if c[0]=='kubectl': self.assertEqual(c[1:3],['--context','test-context'])
            if c[0]=='helm' and '--kube-context' in c: self.assertEqual(c[c.index('--kube-context')+1],'test-context')
        self.assertIn('127.0.0.1:7878',result.stdout)
        self.assertFalse(any('delete' in c for c in calls))
    def test_namespace_only_does_not_require_namespace_creation(self):
        result,calls=self.run_installer(extra_args=['--namespace-only'])
        self.assertEqual(result.returncode,0,result.stderr)
        upgrade=next(c for c in calls if 'upgrade' in c)
        self.assertNotIn('--create-namespace',upgrade)
        self.assertIn('nodeMonitoring=false',upgrade)
        self.assertIn('nodeManagement=false',upgrade)
    def test_upgrade_preserves_values(self):
        result,calls=self.run_installer('upgrade',INSTALLED='1')
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertIn('--reuse-values',next(c for c in calls if 'upgrade' in c))
    def test_reinstall_reuses_account_claim(self):
        result,calls=self.run_installer(CLAIM='1')
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertIn('persistence.existingClaim=kubeweasel-data',next(c for c in calls if 'upgrade' in c))
    def test_uninstall_never_deletes_namespace_or_data(self):
        result,calls=self.run_installer('uninstall')
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertTrue(any('uninstall' in c for c in calls))
        self.assertFalse(any('delete' in c for c in calls))
    def test_uninstall_blocks_legacy_unretained_claim(self):
        result,calls=self.run_installer('uninstall',POLICY='delete')
        self.assertNotEqual(result.returncode,0)
        self.assertFalse(any('uninstall' in c for c in calls))
    def test_checksum_failure_blocks_mutation(self):
        result,calls=self.run_installer(download=True,CHECKSUM='0'*64)
        self.assertNotEqual(result.returncode,0); self.assertIn('checksum mismatch',result.stderr)
        self.assertFalse(any('upgrade' in c for c in calls))
    def test_download_checks_versioned_chart(self):
        result,calls=self.run_installer(download=True)
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertTrue(any('https://github.com/Senan3188/KubeWeasel/releases/download/v0.2.0/kubeweasel-0.2.0.tgz' in c for c in calls))
    def test_readiness_failure_preserves_resources(self):
        result,calls=self.run_installer(FAIL_READY='1')
        self.assertNotEqual(result.returncode,0)
        self.assertIn('retained for inspection',result.stderr)
        self.assertFalse(any('uninstall' in c or 'delete' in c for c in calls))

if __name__=='__main__': unittest.main()
