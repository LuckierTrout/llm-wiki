#!/usr/bin/env python3
"""Email-to-wiki ingest sidecar for LLM Wiki (web deployment).

Polls a dedicated mailbox over IMAP and drops message attachments into a
wiki project's ``raw/sources/`` directory, where LLM Wiki's Source Folder
Auto-Watch picks them up and queues them for ingestion. Emails without
attachments become markdown notes (subject = title, body = content), so
you can mail quick notes to your wiki too.

Design notes:
- stdlib only (imaplib/email); no third-party dependencies to patch.
- Sender allowlist is mandatory — mail from anyone else is marked seen
  and skipped, so a leaked address can't fill your wiki with spam.
- Messages are fetched with BODY.PEEK[] and marked \\Seen exactly once,
  whether processing succeeds or fails: a malformed email is logged and
  skipped rather than retried forever, and nothing is ever deleted.
"""

import email
import email.policy
import html
import imaplib
import os
import re
import sys
import time
from datetime import datetime, timezone
from email.message import EmailMessage
from email.utils import parseaddr
from pathlib import Path

DEFAULT_EXTENSIONS = (
    "md,markdown,txt,org,pdf,doc,docx,xls,xlsx,ppt,pptx,epub,mobi,"
    "html,htm,csv,tsv,json,yaml,yml,png,jpg,jpeg,webp,gif"
)


def log(message: str) -> None:
    stamp = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%S")
    print(f"[email-ingest] {stamp} {message}", flush=True)


class Config:
    def __init__(self) -> None:
        self.imap_host = self._require("IMAP_HOST")
        self.imap_port = int(os.environ.get("IMAP_PORT", "993"))
        self.imap_user = self._require("IMAP_USER")
        self.imap_password = self._require("IMAP_PASSWORD")
        self.imap_folder = os.environ.get("IMAP_FOLDER", "INBOX")
        self.target_dir = Path(self._require("EMAIL_INGEST_DIR"))
        self.poll_seconds = max(15, int(os.environ.get("POLL_SECONDS", "60")))
        self.max_attachment_bytes = (
            int(os.environ.get("MAX_ATTACHMENT_MB", "50")) * 1024 * 1024
        )
        self.ingest_body = os.environ.get("INGEST_BODY", "true").lower() != "false"
        self.processed_folder = os.environ.get("PROCESSED_FOLDER", "").strip()
        self.allowed_senders = {
            sender.strip().lower()
            for sender in os.environ.get("EMAIL_ALLOWED_SENDERS", "").split(",")
            if sender.strip()
        }
        self.allowed_extensions = {
            ext.strip().lstrip(".").lower()
            for ext in os.environ.get("ALLOWED_EXTENSIONS", DEFAULT_EXTENSIONS).split(",")
            if ext.strip()
        }
        if not self.allowed_senders:
            log(
                "FATAL: EMAIL_ALLOWED_SENDERS is not set. Refusing to ingest "
                "mail from arbitrary senders — set it to a comma-separated "
                "list of addresses allowed to feed your wiki."
            )
            sys.exit(1)

    @staticmethod
    def _require(name: str) -> str:
        value = os.environ.get(name, "").strip()
        if not value:
            log(f"FATAL: required environment variable {name} is not set")
            sys.exit(1)
        return value


def sanitize_filename(name: str) -> str:
    cleaned = re.sub(r'[/\\:*?"<>|\x00-\x1f]', "_", name).strip().strip(".")
    cleaned = re.sub(r"\s+", " ", cleaned)
    return cleaned[:160] or "attachment"


def unique_path(directory: Path, filename: str) -> Path:
    candidate = directory / filename
    if not candidate.exists():
        return candidate
    stem, dot, suffix = filename.rpartition(".")
    if not dot:
        stem, suffix = filename, ""
    for counter in range(1, 1000):
        candidate = directory / (f"{stem}-{counter}.{suffix}" if dot else f"{stem}-{counter}")
        if not candidate.exists():
            return candidate
    raise RuntimeError(f"could not find a free filename for {filename}")


def slugify(text: str, max_length: int = 60) -> str:
    slug = re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")
    return slug[:max_length].rstrip("-") or "note"


def html_to_text(markup: str) -> str:
    markup = re.sub(r"(?is)<(script|style)\b.*?</\1>", "", markup)
    markup = re.sub(r"(?i)<br\s*/?>", "\n", markup)
    markup = re.sub(r"(?i)</(p|div|li|h[1-6]|tr)>", "\n", markup)
    text = re.sub(r"<[^>]+>", "", markup)
    text = html.unescape(text)
    return re.sub(r"\n{3,}", "\n\n", text).strip()


def message_body_text(msg: EmailMessage) -> str:
    plain = msg.get_body(preferencelist=("plain",))
    if plain is not None:
        return plain.get_content().strip()
    rich = msg.get_body(preferencelist=("html",))
    if rich is not None:
        return html_to_text(rich.get_content())
    return ""


def extract_attachments(msg: EmailMessage) -> list[tuple[str, bytes]]:
    attachments = []
    for part in msg.iter_attachments():
        filename = part.get_filename()
        if not filename:
            continue
        try:
            payload = part.get_content()
        except Exception as error:  # malformed part — skip it, keep the rest
            log(f"  skipping undecodable attachment {filename!r}: {error}")
            continue
        if isinstance(payload, str):
            payload = payload.encode("utf-8", errors="replace")
        if not isinstance(payload, bytes):
            continue
        attachments.append((filename, payload))
    return attachments


def frontmatter_escape(value: str) -> str:
    return value.replace("\\", "\\\\").replace('"', '\\"')


def write_note(
    target_dir: Path, subject: str, sender: str, date: str, body: str
) -> Path:
    stamp = datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")
    path = unique_path(target_dir, f"email-{stamp}-{slugify(subject)}.md")
    frontmatter = "\n".join(
        [
            "---",
            f'title: "{frontmatter_escape(subject or "Email note")}"',
            "type: clip",
            "origin: email",
            f'source: "{frontmatter_escape(sender)}"',
            f'date: "{frontmatter_escape(date)}"',
            "tags: [email]",
            "---",
            "",
        ]
    )
    path.write_text(frontmatter + body + "\n", encoding="utf-8")
    return path


def process_message(raw: bytes, cfg: Config) -> list[Path]:
    """Parse one RFC822 message and write its content into the wiki.

    Returns the list of files written. Raises nothing on a per-attachment
    basis — bad parts are logged and skipped.
    """
    msg = email.message_from_bytes(raw, policy=email.policy.default)
    sender = parseaddr(msg.get("From", ""))[1].lower()
    subject = str(msg.get("Subject", "")).strip()
    date = str(msg.get("Date", "")).strip()

    if sender not in cfg.allowed_senders:
        log(f"  rejected: sender {sender!r} not in allowlist (subject {subject!r})")
        return []

    cfg.target_dir.mkdir(parents=True, exist_ok=True)
    written: list[Path] = []

    attachments = extract_attachments(msg)
    for filename, payload in attachments:
        extension = filename.rpartition(".")[2].lower()
        if extension not in cfg.allowed_extensions:
            log(f"  skipping {filename!r}: extension .{extension} not allowed")
            continue
        if len(payload) > cfg.max_attachment_bytes:
            log(f"  skipping {filename!r}: {len(payload)} bytes exceeds cap")
            continue
        path = unique_path(cfg.target_dir, sanitize_filename(filename))
        path.write_bytes(payload)
        written.append(path)
        log(f"  saved attachment {path.name} ({len(payload)} bytes)")

    # Only turn the body into a note when the mail carried no attachments
    # at all — with attachments (even rejected ones), the body is usually
    # just "see attached" boilerplate.
    if not attachments and cfg.ingest_body:
        body = message_body_text(msg)
        if body:
            path = write_note(cfg.target_dir, subject, sender, date, body)
            written.append(path)
            log(f"  saved note {path.name}")
        else:
            log("  nothing to ingest (no attachments, empty body)")
    elif not written:
        log("  nothing ingested (all attachments were skipped)")

    return written


def poll_once(cfg: Config) -> int:
    """One IMAP poll cycle. Returns the number of messages handled."""
    with imaplib.IMAP4_SSL(cfg.imap_host, cfg.imap_port) as imap:
        imap.login(cfg.imap_user, cfg.imap_password)
        status, _ = imap.select(cfg.imap_folder)
        if status != "OK":
            log(f"cannot select folder {cfg.imap_folder!r}")
            return 0
        status, data = imap.uid("SEARCH", None, "UNSEEN")
        if status != "OK":
            return 0
        uids = data[0].split()
        for uid in uids:
            uid_str = uid.decode()
            log(f"processing message uid={uid_str}")
            status, fetched = imap.uid("FETCH", uid_str, "(BODY.PEEK[])")
            raw = None
            if status == "OK" and fetched:
                for item in fetched:
                    if isinstance(item, tuple) and len(item) >= 2:
                        raw = item[1]
                        break
            if raw is None:
                log("  fetch failed; leaving unread for next cycle")
                continue
            try:
                process_message(raw, cfg)
            except Exception as error:
                log(f"  ERROR processing message: {error!r} (marking seen, skipping)")
            # Mark seen exactly once, success or failure, so a poison
            # message can't wedge the loop.
            imap.uid("STORE", uid_str, "+FLAGS", "(\\Seen)")
            if cfg.processed_folder:
                status, _ = imap.uid("COPY", uid_str, cfg.processed_folder)
                if status == "OK":
                    imap.uid("STORE", uid_str, "+FLAGS", "(\\Deleted)")
        if cfg.processed_folder and uids:
            imap.expunge()
        return len(uids)


def main() -> None:
    cfg = Config()
    log(
        f"watching {cfg.imap_user} on {cfg.imap_host}:{cfg.imap_port} "
        f"every {cfg.poll_seconds}s → {cfg.target_dir}"
    )
    log(f"allowed senders: {', '.join(sorted(cfg.allowed_senders))}")
    while True:
        try:
            handled = poll_once(cfg)
            if handled:
                log(f"cycle complete: {handled} message(s)")
        except Exception as error:
            log(f"poll cycle failed: {error!r} (retrying in {cfg.poll_seconds}s)")
        time.sleep(cfg.poll_seconds)


if __name__ == "__main__":
    main()
