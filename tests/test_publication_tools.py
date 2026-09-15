from pathlib import Path
import gzip
import importlib.util
import io
import tarfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("publication_audit", ROOT / "scripts/audit_publication.py")
audit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit)

class PublicationAuditTests(unittest.TestCase):
    def test_nested_gzip_content_is_checked(self):
        payload = b"-----BEGIN " + b"PRIVATE KEY-----" + b"\nsynthetic-test-only\n"
        result = audit.Audit()
        result.scan(gzip.compress(payload), "synthetic.gz")
        self.assertTrue(any(f["rule"] == "private-key" and "!gzip" in f["location"] for f in result.findings))

    def test_tar_traversal_is_rejected_without_extraction(self):
        out = io.BytesIO()
        with tarfile.open(fileobj=out, mode="w") as archive:
            member = tarfile.TarInfo("../escape")
            member.size = 1
            archive.addfile(member, io.BytesIO(b"x"))
        result = audit.Audit()
        result.scan(out.getvalue(), "synthetic.tar")
        self.assertTrue(any(f["rule"] == "unsafe-archive-path" for f in result.findings))

    def test_nested_zip_content_is_checked(self):
        out = io.BytesIO()
        with zipfile.ZipFile(out, "w") as archive:
            archive.writestr("text.txt", b"-----BEGIN " + b"PRIVATE KEY-----")
        result = audit.Audit()
        result.scan(out.getvalue(), "synthetic.zip")
        self.assertTrue(any(f["rule"] == "private-key" and "!text.txt" in f["location"] for f in result.findings))

if __name__ == "__main__":
    unittest.main()
