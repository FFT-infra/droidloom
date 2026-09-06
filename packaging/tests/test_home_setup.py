"""Exercise provisioning decisions without Android or administrator access."""
import os
import pathlib
import subprocess
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]


class HomeSetupTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = pathlib.Path(self.tmp.name)
        self.apk = self.root / 'DroidloomHome.apk'
        self.apk.write_bytes(b'first signed apk')
        script = (ROOT / 'packaging/home/setup').read_text()
        script = script.replace('export PATH=/system/bin:/system/xbin', '')
        script = script.replace('/droidloom/ime/DroidloomHome.apk', str(self.apk))
        script = script.replace('/data/local/tmp/droidloom-home.sha256', str(self.root / 'digest'))
        (self.root / 'setup').write_text(script)
        command = self.root / 'cmd'
        command.write_text('''#!/bin/sh
set -eu
echo "$*" >> "$FIXTURE/calls"
if [ "$1" = force-stop ]; then exit 0; fi
case "$2" in
path) test ! -f "$FIXTURE/installed" || echo package:/data/app/home.apk ;;
install)
    if [ "${FAIL_INSTALL:-0}" = 1 ]; then echo Failure; exit 0; fi
    touch "$FIXTURE/installed"; echo Success ;;
enable) ;;
disable-user) ;;
set-home-activity) test "${FAIL_ROLE:-0}" != 1 ;;
resolve-activity)
    if [ "${WRONG_HOME:-0}" = 1 ]; then echo com.android.settings/.FallbackHome
    else echo com.android.droidloom.home/.HomeActivity; fi ;;
*) exit 2 ;;
esac
''')
        command.chmod(0o755)
        (self.root / 'am').symlink_to(command)

    def run_setup(self, **extra):
        env = dict(os.environ, PATH=str(self.root) + ':' + os.environ['PATH'],
                   FIXTURE=str(self.root), **extra)
        return subprocess.run(['sh', str(self.root / 'setup')], env=env,
                              capture_output=True, text=True)

    def test_boot_reasserts_home_without_reinstalling_unchanged_apk(self):
        self.assertEqual(self.run_setup().returncode, 0)
        self.assertEqual(self.run_setup().returncode, 0)
        calls = (self.root / 'calls').read_text()
        self.assertEqual(calls.count('package install '), 1)
        self.assertEqual(calls.count('package set-home-activity --user 0 '), 2)
        self.assertEqual(calls.count('package disable-user --user 0 com.android.launcher3'), 2)
        self.assertLess(calls.index('package resolve-activity'), calls.index('package disable-user'))
        self.assertEqual(calls.count('force-stop --user 0 com.android.launcher3'), 2)
        self.assertLess(calls.index('package disable-user'), calls.index('force-stop'))
        self.apk.write_bytes(b'updated signed apk')
        self.assertEqual(self.run_setup().returncode, 0)
        self.assertEqual((self.root / 'calls').read_text().count('package install '), 2)

    def test_failed_install_never_records_digest_or_changes_home(self):
        self.assertNotEqual(self.run_setup(FAIL_INSTALL='1').returncode, 0)
        self.assertFalse((self.root / 'digest').exists())
        self.assertNotIn('set-home-activity', (self.root / 'calls').read_text())
        self.assertNotIn('disable-user', (self.root / 'calls').read_text())
        self.assertNotIn('force-stop', (self.root / 'calls').read_text())

    def test_reinstalls_if_package_was_removed(self):
        self.assertEqual(self.run_setup().returncode, 0)
        (self.root / 'installed').unlink()
        self.assertEqual(self.run_setup().returncode, 0)
        self.assertEqual((self.root / 'calls').read_text().count('package install '), 2)

    def test_role_failure_and_wrong_resolver_are_reported(self):
        self.assertNotEqual(self.run_setup(FAIL_ROLE='1').returncode, 0)
        self.assertNotEqual(self.run_setup(WRONG_HOME='1').returncode, 0)
        self.assertNotIn('disable-user', (self.root / 'calls').read_text())
        self.assertNotIn('force-stop', (self.root / 'calls').read_text())


if __name__ == '__main__':
    unittest.main()
