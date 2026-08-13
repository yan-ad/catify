import importlib.util
import pathlib
import unittest

SCRIPT = pathlib.Path(__file__).parents[1] / "generate-inventory.py"
SPEC = importlib.util.spec_from_file_location("generate_inventory", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class InventoryParserTests(unittest.TestCase):
    def test_flag_short_name_does_not_leak_to_previous_flag(self):
        source = """
        static flags = {
            id: Flags.string({required: true}),
            store: Flags.string({char: 's'}),
        }
        """
        self.assertEqual(
            MODULE.flag_entries(source),
            [{"name": "id", "short": None}, {"name": "store", "short": "s"}],
        )

    def test_node_import_is_not_an_executable_dependency(self):
        source = "import {x} from '@shopify/cli-kit/node/cli'"
        self.assertEqual(MODULE.external_executables(source), [])

    def test_spawned_binary_is_an_executable_dependency(self):
        source = "await spawn('cloudflared', ['tunnel'])"
        self.assertEqual(MODULE.external_executables(source), ["cloudflared"])


if __name__ == "__main__":
    unittest.main()
