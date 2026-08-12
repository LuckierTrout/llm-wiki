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
  and skipped, so a leaked address can't fill your wiki with spam. Since
  From headers are spoofable, an optional EMAIL_SECRET_TOKEN adds a
  shared-secret check on the subject line.
- Files are staged outside the watched directory and published with an
  atomic rename, so the auto-watcher never observes partial content.
- A durable (UIDVALIDITY, UID) record makes processing idempotent across
  restarts: messages are recorded after publication and before the IMAP
  acknowledgement, so a crash in between cannot duplicate files.
- Messages are fetched with BODY.PEEK[] and marked \\Seen exactly once,
  whether processing succeeds or fails: a malformed email is logged and
  skipped rather than retried forever, and nothing is ever deleted.
"""

import email
import email.policy
import html
import imaplib
import json
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
PROCESSED_RECORD_LIMIT = 1000


def log(message: str) -> None:
    stamp = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%S")
    print(f"[email-ingest] {stamp} {message}", flush=True)


def env_str(name: str, default: str) -> str:
    """Read an optional env var, treating empty/whitespace as unset so a
    compose file can forward `${VAR:-}` without clobbering defaults."""
    value = os.environ.get(name, "").strip()
    return value or default


class Config:
    def __init__(self) -> None:
        self.imap_host = self._require("IMAP_HOST")
        self.imap_port = int(env_str("IMAP_PORT", "993"))
        self.imap_user = self._require("IMAP_USER")
        self.imap_password = self._require("IMAP_PASSWORD")
        self.imap_folder = env_str("IMAP_FOLDER", "INBOX")
        # Finite socket timeout so a stalled connection or read always
        # returns to the retry loop instead of blocking forever.
        self.imap_timeout = float(env_str("IMAP_TIMEOUT_SECONDS", "60"))
        self.target_dir = Path(self._require("EMAIL_INGEST_DIR"))
        self.poll_seconds = max(15, int(env_str("POLL_SECONDS", "60")))
        self.max_attachment_bytes = int(env_str("MAX_ATTACHMENT_MB", "50")) * 1024 * 1024
        # Total-message cap, enforced via RFC822.SIZE before the body is
        # ever fetched, so one huge email can't exhaust sidecar memory.
        self.max_message_bytes = int(env_str("MAX_MESSAGE_MB", "100")) * 1024 * 1024
        self.ingest_body = env_str("INGEST_BODY", "true").lower() != "false"
        self.processed_folder = env_str("PROCESSED_FOLDER", "")
        # Optional shared secret: when set, only messages whose subject
        # contains the token are ingested. This is the defense against
        # From-header spoofing — the header names an allowlisted sender,
        # but only your own mail carries the token.
        self.secret_token = env_str("EMAIL_SECRET_TOKEN", "")
        self.allowed_senders = {
            sender.strip().lower()
            for sender in os.environ.get("EMAIL_ALLOWED_SENDERS", "").split(",")
            if sender.strip()
        }
        self.allowed_extensions = {
            ext.strip().lstrip(".").lower()
            for ext in env_str("ALLOWED_EXTENSIONS", DEFAULT_EXTENSIONS).split(",")
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

    @property
    def work_dir(self) -> Path:
        """Sidecar working area next to (not inside) the watched directory,
        on the same filesystem so renames into it are atomic."""
        return self.target_dir.parent / ".email-ingest"

    @property
    def staging_dir(self) -> Path:
        return self.work_dir / "staging"

    @property
    def state_path(self) -> Path:
        return self.work_dir / "processed.json"


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


def publish_file(cfg: Config, filename: str, data: bytes) -> Path:
    """Write into staging (outside the watched tree, same filesystem), then
    atomically rename into the target directory, so the wiki's auto-watcher
    only ever observes complete files."""
    cfg.staging_dir.mkdir(parents=True, exist_ok=True)
    cfg.target_dir.mkdir(parents=True, exist_ok=True)
    tmp = cfg.staging_dir / f"{time.time_ns()}-{filename}"
    try:
        tmp.write_bytes(data)
        final = unique_path(cfg.target_dir, filename)
        os.replace(tmp, final)
    except Exception:
        tmp.unlink(missing_ok=True)
        raise
    return final


def frontmatter_escape(value: str) -> str:
    return value.replace("\\", "\\\\").replace('"', '\\"')


def write_note(cfg: Config, subject: str, sender: str, date: str, body: str) -> Path:
    stamp = datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")
    filename = f"email-{stamp}-{slugify(subject)}.md"
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
    return publish_file(cfg, filename, (frontmatter + body + "\n").encode("utf-8"))


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
    if cfg.secret_token:
        if cfg.secret_token not in subject:
            log(f"  rejected: subject lacks the secret token (from {sender!r})")
            return []
        subject = subject.replace(cfg.secret_token, "").strip() or "Email note"

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
        path = publish_file(cfg, sanitize_filename(filename), payload)
        written.append(path)
        log(f"  saved attachment {path.name} ({len(payload)} bytes)")

    # Only turn the body into a note when the mail carried no attachments
    # at all — with attachments (even rejected ones), the body is usually
    # just "see attached" boilerplate.
    if not attachments and cfg.ingest_body:
        body = message_body_text(msg)
        if body:
            path = write_note(cfg, subject, sender, date, body)
            written.append(path)
            log(f"  saved note {path.name}")
        else:
            log("  nothing to ingest (no attachments, empty body)")
    elif not written:
        log("  nothing ingested (all attachments were skipped)")

    return written


# ---------------------------------------------------------------------------
# Durable processed-message record: (UIDVALIDITY, UID) pairs survive restarts
# so a crash between publishing files and acknowledging the message cannot
# ingest the same email twice.
# ---------------------------------------------------------------------------


def load_processed_record(state_path: Path) -> dict:
    try:
        record = json.loads(state_path.read_text(encoding="utf-8"))
        if isinstance(record, dict) and isinstance(record.get("uids"), list):
            return {"uidvalidity": str(record.get("uidvalidity", "")),
                    "uids": [str(uid) for uid in record["uids"]]}
    except (OSError, ValueError):
        pass
    return {"uidvalidity": "", "uids": []}


def is_processed(record: dict, uidvalidity: str, uid: str) -> bool:
    return record["uidvalidity"] == uidvalidity and uid in record["uids"]


def mark_processed(record: dict, state_path: Path, uidvalidity: str, uid: str) -> None:
    if record["uidvalidity"] != uidvalidity:
        # The mailbox re-numbered its messages; old UIDs are meaningless.
        record["uidvalidity"] = uidvalidity
        record["uids"] = []
    record["uids"].append(uid)
    del record["uids"][:-PROCESSED_RECORD_LIMIT]
    state_path.parent.mkdir(parents=True, exist_ok=True)
    tmp = state_path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(record), encoding="utf-8")
    os.replace(tmp, state_path)


def parse_rfc822_size(fetch_data) -> int | None:
    for item in fetch_data or []:
        blob = item if isinstance(item, bytes) else item[0] if isinstance(item, tuple) else None
        if blob:
            match = re.search(rb"RFC822\.SIZE (\d+)", blob)
            if match:
                return int(match.group(1))
    return None


def poll_once(cfg: Config) -> int:
    """One IMAP poll cycle. Returns the number of messages handled."""
    with imaplib.IMAP4_SSL(cfg.imap_host, cfg.imap_port, timeout=cfg.imap_timeout) as imap:
        imap.login(cfg.imap_user, cfg.imap_password)
        status, _ = imap.select(cfg.imap_folder)
        if status != "OK":
            log(f"cannot select folder {cfg.imap_folder!r}")
            return 0
        _, validity_data = imap.response("UIDVALIDITY")
        uidvalidity = (validity_data[0] or b"").decode() if validity_data else ""
        record = load_processed_record(cfg.state_path)

        status, data = imap.uid("SEARCH", None, "UNSEEN")
        if status != "OK":
            return 0
        uids = data[0].split()
        for uid in uids:
            uid_str = uid.decode()

            if is_processed(record, uidvalidity, uid_str):
                # Already ingested in a previous run that crashed before the
                # acknowledgement — just clear the unread flag.
                log(f"uid={uid_str} already processed; marking seen")
                imap.uid("STORE", uid_str, "+FLAGS", "(\\Seen)")
                continue

            # Reject oversized messages by advertised size before fetching
            # the body into memory.
            status, size_data = imap.uid("FETCH", uid_str, "(RFC822.SIZE)")
            size = parse_rfc822_size(size_data) if status == "OK" else None
            if size is not None and size > cfg.max_message_bytes:
                log(f"uid={uid_str} rejected: {size} bytes exceeds message cap")
                imap.uid("STORE", uid_str, "+FLAGS", "(\\Seen)")
                continue

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
                mark_processed(record, cfg.state_path, uidvalidity, uid_str)
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
    if cfg.secret_token:
        log("subject secret token: required")
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
