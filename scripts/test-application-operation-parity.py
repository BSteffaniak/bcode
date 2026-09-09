#!/usr/bin/env python3
"""Regression coverage for the application parity source guards."""

import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location('ipc_guard', ROOT / 'scripts/check-application-ipc-callers.py')
GUARD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GUARD)


class ProductionBoundaryTests(unittest.TestCase):
    def forbidden(self, source):
        return bool(GUARD.RAW_IPC.search(GUARD.production_tokens(source)))

    def test_inline_tests_do_not_hide_following_production(self):
        fixture = '#[cfg(test)] mod fixtures { fn test() { bcode_ipc::Request::Ping; } }'
        self.assertFalse(self.forbidden(fixture))
        self.assertTrue(self.forbidden(fixture + '\nfn production() { Request::Ping; }'))
        self.assertTrue(self.forbidden('fn production() { Request::Ping; }' + fixture))

    def test_unknown_cfg_is_checked(self):
        for cfg in ['feature = "test"', 'any(test, feature = "prod")', 'not(test)']:
            self.assertTrue(self.forbidden(f'#[cfg({cfg})] mod x {{ Request::Ping; }}'))

    def test_literals_and_comments_do_not_change_module_extent(self):
        source = '''#[cfg(test)] mod tests {
            let a = r###"} Request::Ping {"###;
            let b = "}"; let c = '}'; let d = '\\u{7d}';
            /* } /* nested */ { */
            Request::Ping;
        }
        fn prod() { Request::Ping; }
        '''
        self.assertTrue(self.forbidden(source))
        self.assertFalse(self.forbidden(source[:source.index('fn prod')]))

    def test_qualified_imports_and_multiline_variants_are_checked(self):
        for source in ['use bcode_ipc::Request;', 'Request ::\n Ping', 'bcode_ipc :: Request']:
            self.assertTrue(self.forbidden(source))
        self.assertFalse(self.forbidden('Request::builder(); // Request::Ping'))

    def test_unbalanced_test_module_fails_closed(self):
        with self.assertRaises(ValueError):
            GUARD.production_tokens('#[cfg(test)] mod tests {')

    def test_package_and_test_path_exclusions(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ['client/src/lib.rs', 'cli/tests/wire.rs', 'cli/src/lib.rs']:
                path = root / 'packages' / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('Request::Ping;', encoding='utf-8')
            self.assertEqual(list(GUARD.violations(root)), ['packages/cli/src/lib.rs'])


class NestedInventoryTests(unittest.TestCase):
    def test_each_nested_enum_rejects_an_unclassified_variant(self):
        script = (ROOT / 'scripts/check-application-operation-parity.sh').read_text()
        checker = script.split("<<'PY'\n", 1)[1].split('\nPY', 1)[0]
        nested = ['WorkflowCommand', 'WorkflowAuthorCommand', 'WorkflowDraftCommand',
                  'WorkflowRevisionCommand', 'WorkflowPresetCommand', 'WorkflowPackageCommand',
                  'PluginCommand']
        # Run the real source-enum checker with one in-memory mutation, without
        # editing the checkout or weakening other classifications.
        for enum in nested:
            prefix = f'''from pathlib import Path
original_read_text = Path.read_text
def read_text(path, *args, **kwargs):
    text = original_read_text(path, *args, **kwargs)
    if str(path) == "packages/cli/src/lib.rs":
        text = text.replace("enum {enum} {{", "enum {enum} {{\\n    UnclassifiedParityProbe,", 1)
    return text
Path.read_text = read_text
'''
            result = subprocess.run(
                ['python3', '-', 'docs/application-operation-parity.md'],
                input=prefix + checker, text=True, capture_output=True,
                cwd=ROOT, timeout=15, check=False,
            )
            self.assertNotEqual(result.returncode, 0, enum)
            self.assertIn('UnclassifiedParityProbe', result.stderr, enum)
            self.assertIn(enum, result.stderr, enum)


if __name__ == '__main__':
    unittest.main()
