"""Shared fixtures for WKP tests.

The embedding model is mocked throughout — tests must never download weights.
All fixtures that need embeddings use `patch_model` (applied via `patch_model`
fixture, not autouse, so individual tests opt in explicitly).
"""
from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock, patch

import numpy as np
import pytest

# ---------------------------------------------------------------------------
# Sample markdown files used across test modules
# ---------------------------------------------------------------------------

SAMPLE_FILES: dict[str, str] = {
    "memory/feedback_writing.md": """\
---
title: Writing Style Feedback
type: feedback
workspace: root
visibility: shared
tokens: 80
tags: [writing, style]
---

Use plain language. Avoid jargon. Short sentences work best.
""",
    "memory/reference_rfe.md": """\
---
title: RFE Guidelines Reference
type: reference
workspace: root
visibility: shared
tokens: 200
tags: [rfe, guidelines]
refs:
  - memory/feedback_writing.md
---

An RFE must have a problem statement and proposed solution. [[feedback_writing]]
""",
    "memory/project_active.md": """\
---
title: Active Project Alpha
type: project-state
workspace: root
visibility: shared
tokens: 120
---

Project Alpha is in progress. Deliverable due next quarter.
""",
    "knowledge/shared/design.md": """\
---
title: Design Document
type: knowledge
workspace: root
visibility: shared
tokens: 300
tags: [design, architecture]
---

This document describes the system architecture in detail.
""",
    # Edge cases
    "memory/no_frontmatter.md": "Plain markdown with no frontmatter.\n",
    "memory/bad_yaml.md": "---\ntitle: Bad: value: with: colons\n---\nContent here.\n",
}


def _det_embedding(text: str) -> np.ndarray:
    """Deterministic 384-dim unit-norm float32 vector keyed on text hash."""
    rng = np.random.default_rng(abs(hash(str(text))) % (2**32))
    vec = rng.random(384).astype(np.float32)
    norm = np.linalg.norm(vec)
    return vec / norm if norm > 0 else vec


def _det_embedding_bytes(text: str) -> bytes:
    return _det_embedding(text).tobytes()


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture
def tmp_workspace(tmp_path: Path) -> Path:
    """Temporary workspace populated with SAMPLE_FILES."""
    for rel, content in SAMPLE_FILES.items():
        p = tmp_path / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(content, encoding="utf-8")
    return tmp_path


@pytest.fixture
def mock_model() -> MagicMock:
    """A SentenceTransformer mock that returns deterministic embeddings."""
    model = MagicMock()
    model.encode.side_effect = lambda text, **kw: _det_embedding(str(text))
    return model


@pytest.fixture
def patch_model(mock_model: MagicMock):
    """Patch embedding calls in indexer and retriever. Opt in per test."""
    with (
        patch("wkp.indexer._get_model", return_value=mock_model),
        patch("wkp.retriever._embed_query", side_effect=_det_embedding_bytes),
    ):
        yield mock_model


@pytest.fixture
def fresh_db(tmp_path: Path):
    """An initialised, empty WKP database."""
    from wkp.db import connect

    conn = connect(tmp_path / ".wkp" / "index.db")
    yield conn
    conn.close()


@pytest.fixture
def populated_db(tmp_workspace: Path, patch_model):
    """Database with all SAMPLE_FILES indexed."""
    from wkp.db import connect
    from wkp.indexer import index_workspace

    conn = connect(tmp_workspace / ".wkp" / "index.db")
    index_workspace(conn, tmp_workspace)
    yield conn, tmp_workspace
    conn.close()
