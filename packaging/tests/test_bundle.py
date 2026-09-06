"""Regression tests at the package boundary; no device access or root required."""
import importlib.machinery
import importlib.util
import json
import pathlib
import subprocess
import tempfile
import unittest
import zipfile

ROOT = pathlib.Path(__file__).resolve().parents[2]
loader = importlib.machinery.SourceFileLoader('bundle', str(ROOT / 'packaging/verify-bundle'))
spec = importlib.util.spec_from_loader(loader.name, loader)
bundle = importlib.util.module_from_spec(spec)
loader.exec_module(bundle)


def elf(version=5, machine=62):
    header = bytearray(64)
    header[:6] = b'\x7fELF\x02\x01'
    header[18:20] = machine.to_bytes(2, 'little')
    return bytes(header) + f'DROIDLOOM_INPUT_ABI={version:05d};'.encode()


class BundleTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = pathlib.Path(self.tmp.name)
        self.put(bundle.COMPOSER, elf())
        for name in ['droidloomd', 'droidloomctl', 'droidloom-supervisor', 'droidloom-wayland', 'droidloom-applications']:
            self.put(pathlib.Path('usr/bin') / name, elf())
        self.jar(bundle.BRIDGE, 5)

    def put(self, path, data):
        path = self.root / path
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)

    def jar(self, path, version):
        path = self.root / path
        path.parent.mkdir(parents=True, exist_ok=True)
        with zipfile.ZipFile(path, 'w') as z:
            z.writestr('classes.dex', f'DROIDLOOM_INPUT_ABI={version};')

    def test_matching_bundle(self):
        bundle.seal(self.root)
        self.assertEqual(bundle.verify(self.root)['input_protocol'], 5)

    def test_exact_reported_regression_old_sender_new_bridge(self):
        self.put(bundle.COMPOSER, elf(4))
        with self.assertRaisesRegex(ValueError, 'Composer v4.*v5'):
            bundle.seal(self.root)
        self.assertFalse((self.root / bundle.MANIFEST).exists())

    def test_legacy_binary_cannot_be_stamped_from_current_sources(self):
        self.put(bundle.COMPOSER, elf().split(b'DROIDLOOM_')[0])
        with self.assertRaisesRegex(ValueError, 'missing.*metadata'):
            bundle.seal(self.root)

    def test_shadow_copy_must_also_match(self):
        self.jar(bundle.RUNTIME / 'ime/droidloom-input-bridge.jar', 4)
        with self.assertRaisesRegex(ValueError, 'protocol mismatch'):
            bundle.seal(self.root)

    def test_modification_missing_extra_and_mode_are_rejected(self):
        for mutation in ['replace', 'remove', 'extra', 'mode']:
            with self.subTest(mutation=mutation):
                self.put('fixture', b'original')
                bundle.seal(self.root)
                path = self.root / 'fixture'
                if mutation == 'replace': path.write_bytes(b'changed')
                elif mutation == 'remove': path.unlink()
                elif mutation == 'extra': self.put('unexpected', b'extra')
                else: path.chmod(0o700)
                with self.assertRaisesRegex(ValueError, 'changed after sealing'):
                    bundle.verify(self.root)
                if (self.root / 'unexpected').exists(): (self.root / 'unexpected').unlink()

    def test_forged_contract_disagrees_with_artifacts(self):
        bundle.seal(self.root)
        path = self.root / bundle.MANIFEST
        manifest = json.loads(path.read_text())
        manifest['compatibility']['input_protocol'] = 4
        path.write_text(json.dumps(manifest))
        with self.assertRaisesRegex(ValueError, 'disagrees'):
            bundle.verify(self.root)

    def test_host_and_android_architectures_match(self):
        self.put('usr/bin/droidloomctl', elf(machine=183))
        with self.assertRaisesRegex(ValueError, 'architectures'):
            bundle.seal(self.root)

    def test_external_symlink_is_rejected(self):
        (self.root / 'escape').symlink_to('/etc/passwd')
        with self.assertRaisesRegex(ValueError, 'symlink'):
            bundle.seal(self.root)

    def test_cli_reports_failure(self):
        result = subprocess.run(['python3', str(ROOT / 'packaging/verify-bundle'), 'verify', str(self.root)], capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b'bundle rejected', result.stderr)


if __name__ == '__main__':
    unittest.main()
