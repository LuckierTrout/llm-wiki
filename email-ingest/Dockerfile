FROM python:3.12-slim
ENV PYTHONUNBUFFERED=1
WORKDIR /app
COPY ingest.py .
# Runs as root deliberately: the sidecar writes into the wiki-data volume
# shared with the main llm-wiki container, whose process creates project
# directories root-owned. Until the main image drops privileges, a non-root
# UID here could not write raw/sources out of the box. The blast radius is
# bounded by the container itself (stdlib-only code, no shell, no exposed
# ports); revisit together with the main image.
ENTRYPOINT ["python", "/app/ingest.py"]
