FROM ghcr.io/astral-sh/uv:python3.13-bookworm-slim
RUN useradd --system --uid 10002 --create-home worker
WORKDIR /app
COPY ml/pyproject.toml ml/uv.lock ./
RUN uv sync --frozen --no-dev --no-install-project
COPY ml/src ./src
RUN uv sync --frozen --no-dev
RUN chown -R worker:worker /app
USER 10002
ENV PATH="/app/.venv/bin:$PATH" PYTHONUNBUFFERED=1
ENTRYPOINT ["type-safety-ml-worker"]

