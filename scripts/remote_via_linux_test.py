import importlib.util, os, unittest
spec = importlib.util.spec_from_file_location("remote", os.path.join(os.path.dirname(__file__), "remote.py"))
remote = importlib.util.module_from_spec(spec); spec.loader.exec_module(remote)

class ViaLinux(unittest.TestCase):
    def test_argv(self):
        a = remote.via_linux_argv("forain@172.16.149.179")
        self.assertEqual(a[0], "ssh")
        self.assertIn("forain@172.16.149.179", a)
        self.assertIn("bootctl set-oneshot leandros.conf", a[-1])
        self.assertIn("systemctl reboot", a[-1])
        self.assertIn("BatchMode=yes", a)

    def test_off_by_default(self):
        self.assertEqual(remote.VIA_LINUX.get("host"), None)

if __name__ == "__main__":
    unittest.main()
