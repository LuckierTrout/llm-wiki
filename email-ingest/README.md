# Email documents to your wiki

This sidecar lets you **email documents (or plain notes) straight into
LLM Wiki**. It polls a dedicated mailbox over IMAP; attachments from
allowlisted senders are saved into your project's `raw/sources/` folder,
where the app's **Source Folder Auto-Watch** picks them up and queues them
for ingestion. Emails without attachments become markdown notes (subject
as title, body as content).

```text
you ──email──▶ dedicated mailbox ──IMAP poll──▶ raw/sources/ ──auto-watch──▶ wiki
```

## Setup

**1. Create a dedicated mailbox.** Any IMAP provider works. Don't reuse
your personal inbox — every unread mail in the polled folder is a
candidate for ingestion.

- **Gmail**: create the account, enable 2-step verification, then create
  an [App password](https://myaccount.google.com/apppasswords) — that's
  the `IMAP_PASSWORD`. Host `imap.gmail.com`.
- **Fastmail**: host `imap.fastmail.com`; use an app password.
- Providers that allow **only OAuth2** for IMAP (notably Outlook.com)
  are not supported — this sidecar authenticates with a plain app
  password (`LOGIN`), and Microsoft no longer accepts that.

**2. Find your project's sources path.** If you created project
"My Wiki" under the default location, it's
`/data/projects/My Wiki/raw/sources` (inside the container's data
volume).

**3. Configure and start.** In the repo directory on your server, add to
your environment (or a `.env` file next to `docker-compose.yml`):

```bash
IMAP_HOST=imap.gmail.com
IMAP_USER=mywiki.inbox@gmail.com
IMAP_PASSWORD=abcd efgh ijkl mnop        # the app password
EMAIL_ALLOWED_SENDERS=you@example.com    # comma-separated
EMAIL_INGEST_DIR=/data/projects/My Wiki/raw/sources
```

Then:

```bash
docker compose --profile email up --build -d
docker compose logs -f email-ingest      # watch it work
```

**4. Enable Source Folder Auto-Watch** for the project in the app
(Settings → source watching; it is on by default). New files are
detected and queued; the actual LLM ingestion runs while the app is open
in a browser tab, so mailed documents are processed the next time you
open your wiki (or immediately if it's already open).

## Behavior and options

| Variable | Default | Meaning |
|---|---|---|
| `IMAP_HOST` / `IMAP_USER` / `IMAP_PASSWORD` | — | Mailbox credentials (required) |
| `EMAIL_ALLOWED_SENDERS` | — | **Required.** Only mail whose From address is listed is ingested; everything else is marked read and skipped |
| `EMAIL_INGEST_DIR` | — | Target `raw/sources` directory (required) |
| `IMAP_FOLDER` | `INBOX` | Folder to poll |
| `IMAP_TIMEOUT_SECONDS` | `60` | Socket timeout so stalled connections recover |
| `POLL_SECONDS` | `60` | Poll interval (min 15) |
| `MAX_ATTACHMENT_MB` | `50` | Per-attachment size cap |
| `MAX_MESSAGE_MB` | `100` | Whole-message cap, checked via `RFC822.SIZE` before the body is downloaded |
| `EMAIL_SECRET_TOKEN` | *(unset)* | Shared secret that must appear in the subject line; stripped from note titles. **Recommended** — defeats From-header spoofing |
| `ALLOWED_EXTENSIONS` | docs/images/text | Comma-separated attachment extensions to accept |
| `INGEST_BODY` | `true` | Turn attachment-less emails into markdown notes |
| `PROCESSED_FOLDER` | *(unset)* | If set, ingested mail is moved here instead of just marked read |

Notes:

- Only **unread** messages are processed; each is marked read exactly
  once, success or failure, so a malformed email can't wedge the loop.
  Nothing is ever deleted (unless `PROCESSED_FOLDER` moves it).
- Files are staged in `raw/.email-ingest/` and moved into `raw/sources/`
  with an atomic rename, so the wiki's watcher never sees partial files.
  A durable processed-message record in the same directory makes
  ingestion idempotent across restarts.
- Attachment filenames are sanitized and de-duplicated (`report-1.pdf`).
- **The `From`-header allowlist is a convenience filter, not
  authentication** — SMTP senders control that header, so a spoofed mail
  can name an allowlisted address. For real protection set
  `EMAIL_SECRET_TOKEN` (a private string like `wk-7f3q9` that you include
  in every subject line), keep the mailbox address private, and/or add a
  provider-side filter that discards mail failing SPF/DKIM checks.

## Testing

```bash
cd email-ingest
python3 test_ingest.py    # offline tests for parsing, allowlist, sanitization
```
