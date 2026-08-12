"""Offline tests for the email-ingest parsing pipeline (no IMAP needed).

Run: python3 test_ingest.py
"""

import os
import tempfile
import unittest
from email.message import EmailMessage
from pathlib import Path

os.environ.setdefault("IMAP_HOST", "imap.example.com")
os.environ.setdefault("IMAP_USER", "wiki@example.com")
os.environ.setdefault("IMAP_PASSWORD", "secret")
os.environ.setdefault("EMAIL_ALLOWED_SENDERS", "me@example.com, other@example.com")
os.environ.setdefault("EMAIL_INGEST_DIR", tempfile.mkdtemp())

import ingest  # noqa: E402  (config reads env at import-time use)


def build_message(
    sender="me@example.com",
    subject="Test subject",
    body="Hello wiki",
    attachments=(),
    html_body=None,
):
    msg = EmailMessage()
    msg["From"] = f"Someone <{sender}>"
    msg["To"] = "wiki@example.com"
    msg["Subject"] = subject
    msg["Date"] = "Tue, 12 Aug 2026 10:00:00 +0000"
    if html_body is not None:
        # HTML-only message (no text/plain alternative), so the HTML
        # stripping path is actually exercised.
        msg.set_content(html_body, subtype="html")
    else:
        msg.set_content(body)
    for name, payload in attachments:
        msg.add_attachment(
            payload,
            maintype="application",
            subtype="octet-stream",
            filename=name,
        )
    return msg.as_bytes()


class EmailIngestTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.cfg = ingest.Config()
        self.cfg.target_dir = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def test_attachment_is_saved(self):
        raw = build_message(attachments=[("paper.pdf", b"%PDF-1.4 fake")])
        written = ingest.process_message(raw, self.cfg)
        self.assertEqual(len(written), 1)
        self.assertEqual(written[0].name, "paper.pdf")
        self.assertEqual(written[0].read_bytes(), b"%PDF-1.4 fake")

    def test_multiple_attachments_and_dedup(self):
        raw = build_message(attachments=[("notes.md", b"# a")])
        ingest.process_message(raw, self.cfg)
        raw2 = build_message(attachments=[("notes.md", b"# b")])
        written = ingest.process_message(raw2, self.cfg)
        self.assertEqual(written[0].name, "notes-1.md")

    def test_disallowed_extension_is_skipped(self):
        raw = build_message(attachments=[("malware.exe", b"MZ")])
        written = ingest.process_message(raw, self.cfg)
        self.assertEqual(written, [])

    def test_unlisted_sender_is_rejected(self):
        raw = build_message(
            sender="attacker@evil.com", attachments=[("paper.pdf", b"%PDF")]
        )
        written = ingest.process_message(raw, self.cfg)
        self.assertEqual(written, [])
        self.assertEqual(list(Path(self.tmp.name).iterdir()), [])

    def test_bodyless_attachmentless_writes_nothing(self):
        raw = build_message(body="")
        written = ingest.process_message(raw, self.cfg)
        self.assertEqual(written, [])

    def test_plain_body_becomes_note(self):
        raw = build_message(subject="Reading list", body="Read the lance paper")
        written = ingest.process_message(raw, self.cfg)
        self.assertEqual(len(written), 1)
        content = written[0].read_text()
        self.assertIn('title: "Reading list"', content)
        self.assertIn("origin: email", content)
        self.assertIn("Read the lance paper", content)
        self.assertTrue(written[0].name.startswith("email-"))
        self.assertTrue(written[0].name.endswith("-reading-list.md"))

    def test_html_body_is_stripped(self):
        raw = build_message(
            subject="HTML note",
            html_body="<p>Hello <b>world</b></p><script>evil()</script>",
        )
        written = ingest.process_message(raw, self.cfg)
        content = written[0].read_text()
        self.assertIn("Hello world", content)
        self.assertNotIn("<b>", content)
        self.assertNotIn("evil()", content)

    def test_attachment_present_means_no_note(self):
        raw = build_message(body="cover letter", attachments=[("doc.md", b"# hi")])
        written = ingest.process_message(raw, self.cfg)
        self.assertEqual([p.name for p in written], ["doc.md"])

    def test_oversized_attachment_is_skipped(self):
        self.cfg.max_attachment_bytes = 10
        raw = build_message(attachments=[("big.pdf", b"x" * 100)])
        written = ingest.process_message(raw, self.cfg)
        self.assertEqual(written, [])

    def test_filename_sanitization(self):
        raw = build_message(attachments=[("../../etc/passwd.md", b"nope")])
        written = ingest.process_message(raw, self.cfg)
        self.assertEqual(len(written), 1)
        self.assertEqual(written[0].parent, Path(self.tmp.name))
        self.assertNotIn("/", written[0].name)
        self.assertNotIn("..", written[0].name.split(".")[0])

    def test_subject_quotes_escaped_in_frontmatter(self):
        raw = build_message(subject='He said "hi"', body="quoted subject")
        written = ingest.process_message(raw, self.cfg)
        self.assertIn('title: "He said \\"hi\\""', written[0].read_text())


if __name__ == "__main__":
    unittest.main(verbosity=2)
