"""Tests for db.py — schema creation and sqlite-vec loading."""
from pathlib import Path


def test_connect_creates_db_file(tmp_path: Path) -> None:
    from wkp.db import connect

    db_path = tmp_path / ".wkp" / "index.db"
    assert not db_path.exists()
    conn = connect(db_path)
    conn.close()
    assert db_path.exists()


def test_schema_tables_exist(fresh_db) -> None:
    tables = {
        row["name"]
        for row in fresh_db.execute(
            "SELECT name FROM sqlite_master WHERE type IN ('table', 'shadow')"
        )
    }
    assert "knowledge_items" in tables
    assert "knowledge_edges" in tables
    # FTS5 and vec0 create shadow/content tables; check the virtual tables
    vtables = {
        row["name"]
        for row in fresh_db.execute(
            "SELECT name FROM sqlite_master WHERE type = 'table' OR type = 'shadow'"
        )
    }
    assert any("knowledge_fts" in t for t in vtables)


def test_sqlite_vec_loaded(fresh_db) -> None:
    row = fresh_db.execute("SELECT vec_version()").fetchone()
    assert row is not None
    assert row[0].startswith("v")


def test_wal_mode_set(fresh_db) -> None:
    row = fresh_db.execute("PRAGMA journal_mode").fetchone()
    assert row[0] == "wal"


def test_idempotent_connect(tmp_path: Path) -> None:
    """Connecting twice to the same DB must not raise."""
    from wkp.db import connect

    db_path = tmp_path / ".wkp" / "index.db"
    c1 = connect(db_path)
    c1.close()
    c2 = connect(db_path)
    c2.close()


def test_edges_indexes_created(fresh_db) -> None:
    indexes = {
        row["name"]
        for row in fresh_db.execute(
            "SELECT name FROM sqlite_master WHERE type = 'index'"
        )
    }
    assert "idx_edges_target" in indexes
    assert "idx_edges_source" in indexes
