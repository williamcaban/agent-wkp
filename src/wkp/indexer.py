"""Index markdown files into the WKP SQLite database."""
from __future__ import annotations

import sqlite3
from pathlib import Path
from typing import TYPE_CHECKING

import numpy as np

from .okf import extract_edges, git_blob_sha, parse, refs_to_json, tags_to_json

if TYPE_CHECKING:
    from sentence_transformers import SentenceTransformer

_model: SentenceTransformer | None = None

SCAN_GLOBS = [
    "memory/*.md",
    "knowledge/**/*.md",
    "*/memory/*.md",
    "*/knowledge/**/*.md",
]


def _get_model() -> SentenceTransformer:
    global _model
    if _model is None:
        from sentence_transformers import SentenceTransformer

        _model = SentenceTransformer("all-MiniLM-L6-v2")
    return _model


def _embed(text: str) -> bytes:
    vec = _get_model().encode(text, normalize_embeddings=True)
    return np.array(vec, dtype=np.float32).tobytes()


def _needs_reindex(conn: sqlite3.Connection, path: str, current_sha: str) -> bool:
    row = conn.execute(
        "SELECT git_blob_sha FROM knowledge_items WHERE path = ?", (path,)
    ).fetchone()
    return row is None or row["git_blob_sha"] != current_sha


def index_file(
    conn: sqlite3.Connection,
    path: Path,
    workspace_root: Path,
    force: bool = False,
) -> bool:
    """Index a single file. Returns True if (re)indexed, False if skipped."""
    rel = str(path.resolve().relative_to(workspace_root))
    sha = git_blob_sha(path)

    if not force and not _needs_reindex(conn, rel, sha):
        return False

    meta, body = parse(path)

    embed_text = f"{meta.get('title', '')} {meta.get('description', '')} {body}"
    embedding = _embed(embed_text[:2048])  # cap to keep embedding fast

    # Upsert metadata row
    conn.execute(
        """
        INSERT INTO knowledge_items
            (path, git_blob_sha, title, type, workspace, visibility,
             tokens, tags, refs, updated, content)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(path) DO UPDATE SET
            git_blob_sha = excluded.git_blob_sha,
            title        = excluded.title,
            type         = excluded.type,
            workspace    = excluded.workspace,
            visibility   = excluded.visibility,
            tokens        = excluded.tokens,
            tags         = excluded.tags,
            refs         = excluded.refs,
            updated      = excluded.updated,
            content      = excluded.content
        """,
        (
            rel,
            sha,
            meta.get("title"),
            meta.get("type"),
            meta.get("workspace"),
            meta.get("visibility", "shared"),
            meta.get("tokens"),
            tags_to_json(meta.get("tags")),
            refs_to_json(meta.get("refs")),
            meta.get("updated"),
            body[:8192],  # store first 8k chars for FTS
        ),
    )

    # Upsert vector (knowledge_vec rowid = knowledge_items rowid)
    rowid = conn.execute(
        "SELECT rowid FROM knowledge_items WHERE path = ?", (rel,)
    ).fetchone()[0]
    conn.execute("DELETE FROM knowledge_vec WHERE rowid = ?", (rowid,))
    conn.execute(
        "INSERT INTO knowledge_vec(rowid, embedding) VALUES (?, ?)",
        (rowid, embedding),
    )

    # Replace edges for this source
    conn.execute("DELETE FROM knowledge_edges WHERE source_path = ?", (rel,))
    for src, tgt, etype in extract_edges(rel, meta, body, workspace_root):
        conn.execute(
            """
            INSERT OR IGNORE INTO knowledge_edges(source_path, target_path, edge_type)
            VALUES (?, ?, ?)
            """,
            (src, tgt, etype),
        )

    conn.commit()
    return True


def index_workspace(
    conn: sqlite3.Connection,
    workspace_root: Path,
    paths: list[Path] | None = None,
    force: bool = False,
) -> tuple[int, int]:
    """Index all (or specified) markdown files. Returns (indexed, skipped)."""
    if paths is None:
        paths = []
        for glob in SCAN_GLOBS:
            paths.extend(workspace_root.glob(glob))

    indexed = skipped = 0
    for p in paths:
        if not p.is_file():
            continue
        if index_file(conn, p, workspace_root, force=force):
            indexed += 1
        else:
            skipped += 1

    return indexed, skipped
