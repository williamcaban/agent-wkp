"""SQLite + sqlite-vec database setup and connection management."""
import sqlite3
from pathlib import Path

import sqlite_vec

SCHEMA = """
CREATE TABLE IF NOT EXISTS knowledge_items (
    path         TEXT PRIMARY KEY,
    git_blob_sha TEXT NOT NULL DEFAULT '',
    title        TEXT,
    type         TEXT,
    workspace    TEXT,
    visibility   TEXT DEFAULT 'shared',
    tokens       INTEGER,
    tags         TEXT,   -- JSON array as text
    refs         TEXT,   -- JSON array as text (raw OKF; normalised into edges table)
    updated      TEXT,
    content      TEXT,   -- full body text (for FTS and snippet generation)
    tier INTEGER GENERATED ALWAYS AS (
        CASE type
            WHEN 'feedback'      THEN 1
            WHEN 'project-state' THEN 1
            WHEN 'skill'         THEN 1
            ELSE 2
        END
    ) STORED
);

CREATE VIRTUAL TABLE IF NOT EXISTS knowledge_vec USING vec0(
    embedding float[384]
);

CREATE VIRTUAL TABLE IF NOT EXISTS knowledge_fts USING fts5(
    path    UNINDEXED,
    title,
    content,
    tags,
    content=knowledge_items,
    content_rowid=rowid
);

-- Triggers keep FTS in sync with knowledge_items
CREATE TRIGGER IF NOT EXISTS ki_ai AFTER INSERT ON knowledge_items BEGIN
    INSERT INTO knowledge_fts(rowid, path, title, content, tags)
    VALUES (new.rowid, new.path, new.title, new.content, new.tags);
END;
CREATE TRIGGER IF NOT EXISTS ki_au AFTER UPDATE ON knowledge_items BEGIN
    INSERT INTO knowledge_fts(knowledge_fts, rowid, path, title, content, tags)
    VALUES ('delete', old.rowid, old.path, old.title, old.content, old.tags);
    INSERT INTO knowledge_fts(rowid, path, title, content, tags)
    VALUES (new.rowid, new.path, new.title, new.content, new.tags);
END;
CREATE TRIGGER IF NOT EXISTS ki_ad AFTER DELETE ON knowledge_items BEGIN
    INSERT INTO knowledge_fts(knowledge_fts, rowid, path, title, content, tags)
    VALUES ('delete', old.rowid, old.path, old.title, old.content, old.tags);
END;

CREATE TABLE IF NOT EXISTS knowledge_edges (
    source_path TEXT NOT NULL,
    target_path TEXT NOT NULL,
    edge_type   TEXT NOT NULL,   -- 'refs' | 'depends_on' | 'mentions'
    weight      REAL DEFAULT 1.0,
    PRIMARY KEY (source_path, target_path, edge_type)
);
CREATE INDEX IF NOT EXISTS idx_edges_target ON knowledge_edges(target_path);
CREATE INDEX IF NOT EXISTS idx_edges_source ON knowledge_edges(source_path);

-- Cache query embeddings to avoid reloading the model for repeated queries
CREATE TABLE IF NOT EXISTS query_cache (
    query     TEXT PRIMARY KEY,
    embedding BLOB NOT NULL,
    created   TEXT DEFAULT (datetime('now'))
);
"""


def connect(db_path: Path) -> sqlite3.Connection:
    """Open (or create) the index database, load sqlite-vec, apply schema."""
    db_path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(db_path)
    conn.row_factory = sqlite3.Row
    conn.enable_load_extension(True)
    sqlite_vec.load(conn)
    conn.enable_load_extension(False)
    conn.execute("PRAGMA journal_mode=WAL")  # separate from executescript (which issues COMMIT first)
    conn.executescript(SCHEMA)
    conn.commit()
    return conn
