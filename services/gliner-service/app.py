"""GLiNER extraction service for DFAE.

A thin, versioned HTTP wrapper around a GLiNER model. The model loads in the
background so the service answers /health immediately (status "loading"); the
Rust engine degrades to rule-based extraction until the model is ready.

    POST /extract   {"text": str, "labels": [str], "threshold": float}
    GET  /health    {"status": "ok"|"loading"|"error", "model_loaded": bool, ...}
    GET  /models
    GET  /metrics   Prometheus text format
"""

from __future__ import annotations

import logging
import os
import threading
import time
from contextlib import asynccontextmanager
from typing import Any

from fastapi import FastAPI, HTTPException, Response
from pydantic import BaseModel, Field

API_VERSION = "1"
MODEL_NAME = os.getenv("GLINER_MODEL", "urchade/gliner_small-v2.1")
# GLiNER's encoder window is ~384 tokens; long inputs are chunked with overlap.
CHUNK_CHARS = int(os.getenv("GLINER_CHUNK_CHARS", "1200"))
CHUNK_OVERLAP = int(os.getenv("GLINER_CHUNK_OVERLAP", "150"))
MAX_TEXT_CHARS = int(os.getenv("GLINER_MAX_TEXT_CHARS", "60000"))
MAX_LABELS = int(os.getenv("GLINER_MAX_LABELS", "40"))

log = logging.getLogger("gliner-service")
logging.basicConfig(level=os.getenv("LOG_LEVEL", "INFO"))


class Runtime:
    """Holds the model and counters (module-level so tests can swap the model)."""

    def __init__(self) -> None:
        self.model: Any = None
        self.status = "loading"
        self.error: str | None = None
        self.loaded_in: float | None = None
        self.requests = 0
        self.errors = 0
        self.latency_sum = 0.0
        self.lock = threading.Lock()


rt = Runtime()


def load_model() -> None:
    started = time.time()
    try:
        from gliner import GLiNER  # imported lazily: heavy

        log.info("loading GLiNER model %s", MODEL_NAME)
        rt.model = GLiNER.from_pretrained(MODEL_NAME)
        rt.loaded_in = time.time() - started
        rt.status = "ok"
        log.info("model ready in %.1fs", rt.loaded_in)
    except Exception as e:  # noqa: BLE001 - surface any load failure via /health
        rt.status = "error"
        rt.error = f"{type(e).__name__}: {e}"
        log.exception("model load failed")


@asynccontextmanager
async def lifespan(_: FastAPI):
    if rt.model is None:  # tests inject a model before startup
        threading.Thread(target=load_model, daemon=True).start()
    yield


app = FastAPI(title="dfae-gliner", version=API_VERSION, lifespan=lifespan)


class ExtractRequest(BaseModel):
    text: str = Field(..., min_length=1)
    labels: list[str] = Field(..., min_length=1)
    threshold: float = Field(0.4, ge=0.0, le=1.0)


class Entity(BaseModel):
    text: str
    label: str
    start: int
    end: int
    score: float


class ExtractResponse(BaseModel):
    entities: list[Entity]
    model: str
    latency_ms: int


def chunk_spans(text: str, size: int, overlap: int) -> list[tuple[int, int]]:
    """Split `text` into overlapping windows, preferring to cut at whitespace."""
    n = len(text)
    if n <= size:
        return [(0, n)]
    spans, start = [], 0
    while start < n:
        end = min(n, start + size)
        if end < n:
            window = text[max(start, end - 80) : end]
            i = max(window.rfind(" "), window.rfind("\n"))
            if i >= 0:
                end = max(start, end - 80) + i
        spans.append((start, end))
        if end >= n:
            break
        start = max(end - overlap, start + 1)
    return spans


def predict(text: str, labels: list[str], threshold: float) -> list[dict]:
    found: dict[tuple[int, int, str], dict] = {}
    for lo, hi in chunk_spans(text, CHUNK_CHARS, CHUNK_OVERLAP):
        for e in rt.model.predict_entities(text[lo:hi], labels, threshold=threshold):
            start, end = lo + int(e["start"]), lo + int(e["end"])
            key = (start, end, e["label"])
            ent = {"text": text[start:end], "label": e["label"], "start": start, "end": end, "score": float(e["score"])}
            if key not in found or ent["score"] > found[key]["score"]:
                found[key] = ent  # overlapping chunks report the same span twice
    return sorted(found.values(), key=lambda x: (x["start"], -x["score"]))


@app.post("/extract", response_model=ExtractResponse)
def extract(req: ExtractRequest) -> ExtractResponse:
    if rt.model is None:
        raise HTTPException(status_code=503, detail=f"model {rt.status}")
    if len(req.text) > MAX_TEXT_CHARS:
        raise HTTPException(status_code=413, detail=f"text exceeds {MAX_TEXT_CHARS} characters")
    if len(req.labels) > MAX_LABELS:
        raise HTTPException(status_code=422, detail=f"at most {MAX_LABELS} labels")
    started = time.time()
    try:
        with rt.lock:  # the model is not guaranteed thread-safe
            entities = predict(req.text, [l.strip() for l in req.labels if l.strip()], req.threshold)
    except Exception as e:  # noqa: BLE001
        rt.errors += 1
        log.exception("extraction failed")
        raise HTTPException(status_code=500, detail=str(e)) from e
    dt = time.time() - started
    rt.requests += 1
    rt.latency_sum += dt
    return ExtractResponse(entities=entities, model=MODEL_NAME, latency_ms=int(dt * 1000))


@app.get("/health")
def health() -> dict:
    # Always 200 so orchestrators treat the *process* as up; callers read
    # `model_loaded` to know whether extraction is available yet.
    return {"status": rt.status, "model_loaded": rt.model is not None, "model": MODEL_NAME, "error": rt.error, "api_version": API_VERSION}


@app.get("/models")
def models() -> dict:
    return {"model": MODEL_NAME, "loaded": rt.model is not None, "max_labels": MAX_LABELS, "chunk_chars": CHUNK_CHARS}


@app.get("/metrics")
def metrics() -> Response:
    avg = (rt.latency_sum / rt.requests) if rt.requests else 0.0
    body = (
        f"gliner_requests_total {rt.requests}\n"
        f"gliner_errors_total {rt.errors}\n"
        f"gliner_latency_seconds_avg {avg:.6f}\n"
        f"gliner_model_loaded {1 if rt.model is not None else 0}\n"
    )
    return Response(content=body, media_type="text/plain; version=0.0.4")
