"""Host workaround tests without changing the developer's firewall or services."""
import copy
import importlib.util
import json
from pathlib import Path
import re
import stat
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('netbird_ingress', ROOT / 'scripts/netbird-ingress.py')
fix = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fix)


def guard():
    return {'handle': 1, 'expr': [{'match': {'op': '==', 'left': {'meta': {'key': 'mark'}}, 'right': fix.AUTH_MARK}}, {'accept': None}]}


def setter():
    return {'expr': [{'mangle': {'key': {'meta': {'key': 'mark'}}, 'value': fix.AUTH_MARK}}]}


class FakeRunner:
    def __init__(self):
        self.forward = [guard(), {'handle': 2, 'expr': [{'drop': None}]}]
        self.pre = [setter()]
        self.address = '100.100.0.1'
        self.calls = []
        self.counter = 100
        self.absent = False

    def __call__(self, *args, data=None, optional=False):
        self.calls.append((args, data))
        if args[0] == 'ip':
            return json.dumps([{'addr_info': [{'family': 'inet', 'scope': 'global', 'local': self.address}]}])
        if args[:5] == ('nft', '-j', '-n', 'list', 'chain'):
            if self.absent:
                return None
            entries = self.forward if args[-1] == fix.CHAIN else self.pre
            return json.dumps({'nftables': [{'rule': r} for r in entries]})
        if args[:3] == ('nft', '--check', '-f'):
            return ''
        if args[:3] == ('nft', '-f', '-'):
            for line in data.splitlines():
                if line.startswith('delete rule'):
                    handle = int(line.split()[-1])
                    self.forward = [r for r in self.forward if r['handle'] != handle]
                elif line.startswith('insert rule'):
                    self.counter += 1
                    comment = re.search(r'comment "([^"]+)"', line).group(1)
                    position = int(re.search(r'position ([0-9]+)', line).group(1))
                    index = next(i for i, r in enumerate(self.forward) if r['handle'] == position)
                    self.forward.insert(index, {'handle': self.counter, 'comment': comment, 'expr': []})
                else:
                    raise AssertionError(line)
            return ''
        if args[0] == 'systemctl':
            return '260'
        raise AssertionError(args)


class NetbirdIngressTests(unittest.TestCase):
    def test_scope_preserves_authorization_marks_and_kubernetes_bits(self):
        config = fix.validate('wt0', [443, 80, 80])
        rule = fix.rule_text(config, '100.100.0.1', 1)
        for text in ('iifname "wt0"', 'ct status dnat', 'ct original ip daddr 100.100.0.1', 'meta l4proto tcp', 'ct original proto-dst { 80, 443 }', 'meta mark and 0xfffcbfff == 0x0000bd20'):
            self.assertIn(text, rule)
        self.assertNotIn('ip saddr', rule)  # Membership comes from NetBird's current authorization.
        self.assertNotIn('mark set', rule)
        for mark in (0x1bd20, 0xbd20, 0x2bd20, 0x1fd20, 0x2fd20, 0x3fd20):
            self.assertEqual(mark & fix.MASK, fix.EXPECTED)
        for mark in (0, 0x10000, 0x20000, 0x1bd10, 0x1bd11, 0x1bd21, 0x1bd22):
            self.assertNotEqual(mark & fix.MASK, fix.EXPECTED)
        self.assertNotEqual(fix.comment_for(config, '100.100.0.1'), fix.comment_for(config, '100.100.1.1'))

    def test_idempotence_recovery_and_atomic_legacy_replacement(self):
        run = FakeRunner()
        original = copy.deepcopy(run.forward)
        run.forward.append({'handle': 71, 'comment': 'kubeweasel-http80', 'expr': []})
        config = fix.validate('wt0', [80, 443])
        self.assertEqual(fix.reconcile(run, config), 'active')
        self.assertEqual(len([r for r in run.forward if fix.owned(r)]), 1)
        self.assertNotIn(71, [r['handle'] for r in run.forward])
        for unchanged in original:
            self.assertIn(unchanged, run.forward)
        batches = [d for a, d in run.calls if a[:3] == ('nft', '-f', '-')]
        self.assertIn('delete rule ip netbird '+fix.CHAIN+' handle 71', batches[0])
        self.assertIn('insert rule', batches[0])
        before = copy.deepcopy(run.forward)
        fix.reconcile(run, config)
        self.assertEqual(run.forward, before)
        self.assertEqual(len([d for a, d in run.calls if a[:3] == ('nft', '-f', '-')]), 1)
        run.forward = original  # NetBird reload removes the helper's rule.
        fix.reconcile(run, config)
        self.assertEqual(len([r for r in run.forward if fix.owned(r)]), 1)
        run.address = '100.100.1.1'
        fix.reconcile(run, config)
        self.assertEqual(len([r for r in run.forward if fix.owned(r)]), 1)
        self.assertIn('100.100.1.1', [d for a, d in run.calls if a[:3] == ('nft', '-f', '-')][-1])

    def test_preceding_native_firewall_rules_keep_their_precedence(self):
        run = FakeRunner()
        deny = {'handle': 33, 'comment': 'native-policy-denial', 'expr': [{'drop': None}]}
        run.forward.insert(0, deny)
        config = fix.validate('wt0', [80])
        fix.reconcile(run, config)
        self.assertEqual(run.forward[0], deny)
        self.assertTrue(fix.owned(run.forward[1]))
        self.assertEqual(run.forward[2], guard())
        later = {'handle': 34, 'comment': 'new-native-policy', 'expr': [{'drop': None}]}
        run.forward.insert(2, later)
        fix.reconcile(run, config)
        self.assertEqual(run.forward[:2], [deny, later])
        self.assertTrue(fix.owned(run.forward[2]))
        self.assertEqual(run.forward[3], guard())

    def test_unknown_netbird_semantics_remove_only_managed_rules(self):
        run = FakeRunner()
        config = fix.validate('wt0', [80])
        fix.reconcile(run, config)
        foreign = {'handle': 22, 'comment': 'administrator-rule', 'expr': [{'accept': None}]}
        run.forward.append(foreign)
        run.pre = []
        self.assertEqual(fix.reconcile(run, config), 'unsupported')
        self.assertFalse(any(fix.owned(r) for r in run.forward))
        self.assertIn(foreign, run.forward)
        self.assertIn(guard(), run.forward)
        run.absent = True
        self.assertEqual(fix.reconcile(run, config), 'waiting')

    def test_scope_input_and_files_fail_closed(self):
        for interface, ports in [('wt0;reboot', [80]), ('wt0', [0]), ('wt0', [65536]), ('wt0', [True]), ('wt0', [])]:
            with self.assertRaises(fix.Failure):
                fix.validate(interface, ports)
        run = FakeRunner()
        for address in ('8.8.8.8', '0.0.0.0'):
            run.address = address
            with self.assertRaises(fix.Failure):
                fix.peer_address(run, 'wt0')
        for owner, mode in [(1000, stat.S_IFREG | 0o644), (0, stat.S_IFREG | 0o666), (0, stat.S_IFLNK | 0o777)]:
            with patch.object(Path, 'lstat', return_value=SimpleNamespace(st_uid=owner, st_mode=mode)):
                with self.assertRaises(fix.Failure):
                    fix.private_file(Path('/test'))

    def test_installer_uses_managed_files_and_timer_without_netbird_restart(self):
        run = FakeRunner()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            args = SimpleNamespace(interface=None, ports=None)
            with patch.multiple(fix, CONFIG=root/'config.json', EXECUTABLE=root/'helper', UNIT_DIR=root, RUNTIME=root/'runtime'), patch.object(fix.shutil, 'which', return_value='/usr/bin/tool'), patch.object(fix, 'private_file'):
                fix.install(run, args)
                self.assertTrue((root/fix.TIMER).exists())
                self.assertIn('OnUnitInactiveSec=15s', (root/fix.TIMER).read_text())
                self.assertNotIn('RemainAfterExit', (root/fix.SERVICE).read_text())
                self.assertIn('CAP_NET_ADMIN', (root/fix.SERVICE).read_text())
                self.assertEqual(json.loads((root/'config.json').read_text())['ports'], [80,443])
                self.assertFalse(any('restart' in a for a, _ in run.calls))
                self.assertFalse(any('nftables.service' in a for a, _ in run.calls))
                self.assertFalse(any('flush' in (d or '') for _, d in run.calls))
                (root/fix.SERVICE).write_text('Unrelated existing service')
                with self.assertRaises(fix.Failure):
                    fix.install(run, args)
