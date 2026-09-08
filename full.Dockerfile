# Unified Chibi image: chibi-tui terminal client + chibi backend (agent mode).
# The TUI spawns `chibi stdio --tui`, so the backend console script is installed
# in the same image; skills are served from /app/skills.
#
# This image is identical to the one built from the backend repository
# (chibi/full.Dockerfile) — the roles are mirrored: the TUI is built from the
# main context, and the backend source is supplied through a named build
# context (`backend-src`), declared below as an empty scratch stage and
# overridden at build time.
#
# Local checkout:
#   docker buildx build \
#     --load \
#     --build-context backend-src=../chibi \
#     -f full.Dockerfile -t pysergio/chibi:full .
#
# Remote git context (works today — the backend repository is public):
#   docker buildx build \
#     --load \
#     --build-context backend-src=https://github.com/s-nagaev/chibi \
#     -f full.Dockerfile -t pysergio/chibi:full .

# === Backend source (empty placeholder, replaced by --build-context backend-src=...) ===
FROM scratch AS backend-src

# === TUI builder ===
FROM rust:slim-bookworm AS tui-builder

WORKDIR /tui
COPY . .

RUN cargo build --release

# === Backend builder ===
FROM python:3.11-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=backend-src requirements.txt pyproject.toml README.md ./
COPY --from=backend-src chibi ./chibi
COPY --from=backend-src skills ./skills
# data/.keep is excluded from the build context by the backend's .dockerignore;
# recreate it so the poetry includes resolve during wheel build.
RUN mkdir -p data && touch data/.keep

RUN pip install --no-cache-dir --no-compile -r requirements.txt
RUN pip install --no-cache-dir --no-compile --no-deps async-timeout
RUN pip install --no-cache-dir --no-compile --no-deps .

# === Safe cleanup (zero risk) ===
RUN SITE=$(python -c "import site; print(site.getsitepackages()[0])") && \
    echo "Size before cleanup:" && du -sh $SITE && \
    find $SITE -type d -name "__pycache__" -exec rm -rf {} + 2>/dev/null || true && \
    find $SITE -type f \( -name "*.pyc" -o -name "*.pyo" \) -delete 2>/dev/null || true && \
    find $SITE/babel/locale-data -maxdepth 1 -type d ! -name "en" ! -name "ru" ! -name "locale-data" -exec rm -rf {} + 2>/dev/null || true && \
    find $SITE -type f \( -name "LICENSE*" -o -name "COPYING*" -o -name "AUTHORS*" -o -name "NOTICE*" \) -delete 2>/dev/null || true && \
    echo "Size after cleanup:" && du -sh $SITE

# === Runtime stage ===
FROM python:3.11-slim-bookworm

LABEL org.label-schema.schema-version="1.0"
LABEL org.label-schema.name="chibi-full"
LABEL org.label-schema.vendor="nagaev.sv@gmail.com"
LABEL org.label-schema.vcs-url="https://github.com/s-nagaev/chibi"

RUN apt-get update && apt-get upgrade -y --no-install-recommends \
    && rm -rf /var/lib/apt/lists/*

# Node.js for MCP servers
RUN apt-get update && apt-get install -y --no-install-recommends nodejs npm \
    && rm -rf /var/lib/apt/lists/*

# Copy cleaned site-packages and console scripts (chibi)
COPY --from=builder /usr/local/lib/python3.11/site-packages /usr/local/lib/python3.11/site-packages
COPY --from=builder /usr/local/bin /usr/local/bin

# Copy the TUI binary (glibc-compatible: rust:slim-bookworm -> python:slim-bookworm)
COPY --from=tui-builder /tui/target/release/chibi-tui /usr/local/bin/chibi-tui

WORKDIR /app
COPY --from=backend-src skills ./skills
RUN mkdir -p /app/data

# Default environment variables (agent mode)
ENV FILESYSTEM_ACCESS=true
ENV ENABLE_MCP_STDIO=true
ENV SKILLS_DIR=/app/skills

CMD ["chibi-tui"]
