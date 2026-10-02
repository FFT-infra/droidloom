"""Camera runtime packaging contracts; never start services or open devices."""
import configparser
import json
import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]


def unit(relative):
    result = configparser.ConfigParser(interpolation=None, strict=False)
    result.read(ROOT / relative)
    return result


class CameraRuntimeTests(unittest.TestCase):
    def test_socket_is_owned_by_runtime_not_capture_process(self):
        socket = unit('packaging/systemd/user/droidloom-camera.socket')
        service = unit('packaging/systemd/user/droidloom-camera.service')
        self.assertEqual(socket['Socket']['ListenStream'], '%t/droidloom/camera.sock')
        self.assertEqual(socket['Socket']['SocketMode'], '0666')
        self.assertEqual(socket['Socket']['DirectoryMode'], '0700')
        self.assertEqual(socket['Socket']['RemoveOnStop'], 'true')
        self.assertEqual(socket['Unit']['PartOf'], 'droidloom.service')
        self.assertEqual(service['Unit']['PartOf'], 'droidloom.service')
        self.assertEqual(service['Unit']['Requires'], 'droidloom-camera.socket')
        self.assertEqual(service['Service']['ExecStart'], '/usr/bin/droidloom-camera')

    def test_both_runtime_units_order_after_optional_camera_socket(self):
        for path in ['packaging/systemd/user/droidloom.service', 'packaging/arch/droidloom.service']:
            with self.subTest(path=path):
                config = unit(path)
                self.assertIn('droidloom-camera.socket', config['Unit']['After'].split())
                self.assertIn('droidloom-camera.socket', config['Unit']['Wants'].split())
                self.assertNotIn('droidloom-camera.socket', config['Unit'].get('Requires', '').split())

    def test_both_cell_templates_project_camera_readonly_runtime_tree(self):
        for name in ['cell-spec-arm64-u1001.json', 'cell-spec-x86_64-u1000.json']:
            with self.subTest(name=name):
                spec = json.loads((ROOT / 'packaging' / name).read_text())
                camera = [entry for entry in spec['android_runtime_directories']
                          if entry['target'] == '/droidloom/camera']
                self.assertEqual(camera, [{
                    'source': '/usr/lib/droidloom/current/camera',
                    'target': '/droidloom/camera',
                }])


if __name__ == '__main__':
    unittest.main()
