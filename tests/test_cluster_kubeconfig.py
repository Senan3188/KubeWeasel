import argparse
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("cluster_config", Path(__file__).parents[1] / "scripts/cluster-kubeconfig.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class ConnectionFileTests(unittest.TestCase):
    def args(self, path):
        return argparse.Namespace(context="remote", namespace="apps", service_account="kubeweasel-remote", duration="1h", output=str(path))

    def test_embeds_only_remote_ca_and_scoped_token_in_private_new_file(self):
        source = {"clusters": [{"cluster": {"server": "https://api.example.test", "certificate-authority-data": "test-ca"}}], "users": [{"token": "admin-must-not-copy"}]}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "connection.json"
            with patch.object(module, "kubectl", side_effect=[json.dumps(source), "scoped-test-token\n"]) as command:
                module.create(self.args(path))
            result = json.loads(path.read_text())
            self.assertEqual(result["users"][0]["user"]["token"], "scoped-test-token")
            self.assertNotIn("admin-must-not-copy", path.read_text())
            self.assertEqual(result["contexts"][0]["context"]["namespace"], "apps")
            self.assertEqual(command.call_args.args[-1], "--duration=1h")
            if os.name == "posix":
                self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            with patch.object(module, "kubectl", side_effect=[json.dumps(source), "another-token"]):
                with self.assertRaises(FileExistsError):
                    module.create(self.args(path))
            self.assertEqual(json.loads(path.read_text()), result)

    def test_rejects_unverified_source_before_creating_token(self):
        source = {"clusters": [{"cluster": {"server": "https://api.example.test", "insecure-skip-tls-verify": True}}]}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "connection.json"
            with patch.object(module, "kubectl", return_value=json.dumps(source)) as command:
                with self.assertRaises(ValueError):
                    module.create(self.args(path))
                self.assertEqual(command.call_count, 1)
            self.assertFalse(path.exists())
