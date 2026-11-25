import app as svc
from fastapi.testclient import TestClient


class FakeModel:
    """Mimics GLiNER.predict_entities (chunk-relative offsets)."""

    def __init__(self):
        self.calls = []

    def predict_entities(self, text, labels, threshold=0.5):
        self.calls.append((len(text), tuple(labels), threshold))
        out = []
        for word, label in (("Redis", "software"), ("Frankfurt", "location")):
            i = text.find(word)
            while i >= 0:
                out.append({"text": word, "label": label, "start": i, "end": i + len(word), "score": 0.9})
                i = text.find(word, i + 1)
        return out


def client_with(model):
    svc.rt.model = model
    svc.rt.status = "ok" if model else "loading"
    return TestClient(svc.app)


def test_health_reports_loading_without_failing():
    with client_with(None) as c:
        # no model injected -> lifespan would start a loader; stop it from importing gliner
        svc.rt.model, svc.rt.status = None, "loading"
        r = c.get("/health")
        assert r.status_code == 200
        assert r.json()["model_loaded"] is False
        assert c.post("/extract", json={"text": "x", "labels": ["a"]}).status_code == 503


def test_extract_returns_spans_with_offsets():
    m = FakeModel()
    with client_with(m) as c:
        assert c.get("/health").json()["model_loaded"] is True
        text = "Deploy Redis 8 in Frankfurt"
        r = c.post("/extract", json={"text": text, "labels": ["software", "location"], "threshold": 0.3})
        assert r.status_code == 200
        ents = r.json()["entities"]
        assert [(e["label"], e["text"]) for e in ents] == [("software", "Redis"), ("location", "Frankfurt")]
        for e in ents:
            assert text[e["start"] : e["end"]] == e["text"]
        assert m.calls[0][2] == 0.3


def test_long_text_is_chunked_with_absolute_offsets_and_deduped():
    m = FakeModel()
    with client_with(m) as c:
        filler = ("lorem ipsum dolor sit amet " * 200)
        text = filler + " Redis runs in Frankfurt. " + filler
        r = c.post("/extract", json={"text": text, "labels": ["software", "location"]})
        ents = r.json()["entities"]
        assert len(m.calls) > 1, "text longer than the chunk size is split"
        assert sum(e["text"] == "Redis" for e in ents) == 1, "overlap must not duplicate spans"
        e = next(e for e in ents if e["text"] == "Redis")
        assert text[e["start"] : e["end"]] == "Redis"


def test_validation_and_limits():
    with client_with(FakeModel()) as c:
        assert c.post("/extract", json={"text": "", "labels": ["a"]}).status_code == 422
        assert c.post("/extract", json={"text": "x", "labels": []}).status_code == 422
        assert c.post("/extract", json={"text": "x", "labels": ["a"], "threshold": 2}).status_code == 422
        assert c.post("/extract", json={"text": "x", "labels": [str(i) for i in range(500)]}).status_code == 422
        assert c.get("/metrics").text.startswith("gliner_requests_total")
