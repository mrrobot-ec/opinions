"""FastAPI webhook tests — PLAN Task 8 Step 5."""

from fastapi.testclient import TestClient

from converse.app import create_app
from converse.graph import PendingStore
from converse.recorder import MemoryRecorder


def make_app():
    """Unit wiring pinned explicitly (DATABASE_URL may be set for integration runs)."""
    return create_app(recorder=MemoryRecorder(), pending_store=PendingStore())


def test_sendblue_webhook_ok():
    app = make_app()
    client = TestClient(app)
    r = client.post(
        "/webhooks/sendblue",
        json={
            "number": "+15551234567",
            "content": "price on the coffee market?",
            "message_id": "m1",
        },
    )
    assert r.status_code == 200
    body = r.json()
    assert "reply" in body and body["reply"]
    assert "run_id" in body and body["run_id"]


def test_sendblue_webhook_rejects_unknown_fields():
    app = make_app()
    client = TestClient(app)
    r = client.post(
        "/webhooks/sendblue",
        json={
            "number": "+15551234567",
            "content": "hi",
            "message_id": "m2",
            "extra": "nope",
        },
    )
    assert r.status_code == 422


def test_sendblue_dedupe_returns_original():
    app = make_app()
    client = TestClient(app)
    payload = {
        "number": "+15551234567",
        "content": "price please",
        "message_id": "dedupe-1",
    }
    r1 = client.post("/webhooks/sendblue", json=payload)
    r2 = client.post("/webhooks/sendblue", json=payload)
    assert r1.status_code == 200 and r2.status_code == 200
    assert r1.json()["run_id"] == r2.json()["run_id"]
    assert r1.json()["reply"] == r2.json()["reply"]
