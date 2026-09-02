import importlib.util
import pathlib
import unittest


SCRIPT = pathlib.Path(__file__).parents[1] / "generate-cli-matrix.py"
SPEC = importlib.util.spec_from_file_location("generate_cli_matrix", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def runtime(*names):
    return {
        "runtime": {"version": "shopify/4.6.1"},
        "commands": [{"name": name} for name in names],
    }


def entry(status="native", evidence=None):
    return {
        "status": status,
        "owner": 40,
        "implementation": "test implementation",
        "evidence": ["test.rs"] if evidence is None else evidence,
        "live_verified": False,
    }


class CliMatrixTests(unittest.TestCase):
    def test_requires_every_runtime_command(self):
        errors = MODULE.validate(runtime("app info"), {"commands": {}})
        self.assertIn("missing status entry: app info", errors)

    def test_implemented_command_requires_evidence(self):
        errors = MODULE.validate(
            runtime("app info"),
            {"commands": {"app info": entry(evidence=[])}},
        )
        self.assertIn("app info: implemented commands require test evidence", errors)

    def test_report_counts_partial_separately(self):
        data = MODULE.report(
            runtime("app info", "app build"),
            {
                "commands": {
                    "app info": entry(),
                    "app build": entry(status="partial"),
                }
            },
        )
        self.assertEqual(data["summary"]["implemented"], 1)
        self.assertEqual(data["summary"]["by_status"]["partial"], 1)


if __name__ == "__main__":
    unittest.main()
