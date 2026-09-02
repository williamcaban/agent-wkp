"""Tests for indexer.py — embedding, upsert, SHA-based cache invalidation."""
from pathlib import Path


class TestIndexFile:
    def test_inserts_metadata_row(self, tmp_workspace: Path, fresh_db, patch_model) -> None:
        from wkp.indexer import index_file

        p = tmp_workspace / "memory" / "feedback_writing.md"
        result = index_file(fresh_db, p, tmp_workspace)
        assert result is True

        row = fresh_db.execute(
            "SELECT title, type, tier, visibility FROM knowledge_items WHERE path LIKE '%feedback_writing%'"
        ).fetchone()
        assert row is not None
        assert row["title"] == "Writing Style Feedback"
        assert row["type"] == "feedback"
        assert row["tier"] == 1
        assert row["visibility"] == "shared"

    def test_inserts_embedding(self, tmp_workspace: Path, fresh_db, patch_model) -> None:
        from wkp.indexer import index_file

        p = tmp_workspace / "memory" / "feedback_writing.md"
        index_file(fresh_db, p, tmp_workspace)

        rowid = fresh_db.execute(
            "SELECT rowid FROM knowledge_items WHERE path LIKE '%feedback_writing%'"
        ).fetchone()[0]
        vec_row = fresh_db.execute(
            "SELECT rowid FROM knowledge_vec WHERE rowid = ?", (rowid,)
        ).fetchone()
        assert vec_row is not None

    def test_inserts_refs_edge(self, tmp_workspace: Path, fresh_db, patch_model) -> None:
        from wkp.indexer import index_file

        # reference_rfe.md has refs: [memory/feedback_writing.md] in its frontmatter.
        # The ref is relative to the file's own directory (memory/), so the resolved
        # path becomes memory/feedback_writing.md within the workspace.
        p = tmp_workspace / "memory" / "reference_rfe.md"
        index_file(fresh_db, p, tmp_workspace)

        edges = fresh_db.execute(
            "SELECT source_path, target_path, edge_type FROM knowledge_edges"
        ).fetchall()
        assert len(edges) > 0
        # The file also has a [[feedback_writing]] wikilink, so at minimum 'mentions' will appear.
        # Confirm at least one edge references the expected target.
        targets = [e["target_path"] for e in edges]
        assert any("feedback_writing" in t for t in targets)

    def test_skips_unchanged_file(self, tmp_workspace: Path, fresh_db, patch_model) -> None:
        from wkp.indexer import index_file

        p = tmp_workspace / "memory" / "feedback_writing.md"
        first = index_file(fresh_db, p, tmp_workspace)
        second = index_file(fresh_db, p, tmp_workspace)

        assert first is True
        # Second call: same SHA → skip (unless git unavailable, in which case SHA="" both times)
        # In a non-git tmp_path, SHA="" for both → skipped
        assert second is False

    def test_force_reindexes(self, tmp_workspace: Path, fresh_db, patch_model) -> None:
        from wkp.indexer import index_file

        p = tmp_workspace / "memory" / "feedback_writing.md"
        index_file(fresh_db, p, tmp_workspace)
        result = index_file(fresh_db, p, tmp_workspace, force=True)
        assert result is True

    def test_malformed_frontmatter_does_not_raise(
        self, tmp_workspace: Path, fresh_db, patch_model
    ) -> None:
        from wkp.indexer import index_file

        p = tmp_workspace / "memory" / "bad_yaml.md"
        # Should not raise — malformed YAML results in null metadata
        result = index_file(fresh_db, p, tmp_workspace)
        assert result is True

        row = fresh_db.execute(
            "SELECT type, title FROM knowledge_items WHERE path LIKE '%bad_yaml%'"
        ).fetchone()
        assert row is not None
        assert row["type"] is None  # no valid OKF

    def test_no_frontmatter_file_indexed(
        self, tmp_workspace: Path, fresh_db, patch_model
    ) -> None:
        from wkp.indexer import index_file

        p = tmp_workspace / "memory" / "no_frontmatter.md"
        result = index_file(fresh_db, p, tmp_workspace)
        assert result is True


class TestIndexWorkspace:
    def test_returns_indexed_and_skipped_counts(
        self, tmp_workspace: Path, fresh_db, patch_model
    ) -> None:
        from wkp.indexer import index_workspace

        indexed, skipped = index_workspace(fresh_db, tmp_workspace)
        assert indexed == len(list(tmp_workspace.rglob("*.md")))
        assert skipped == 0

    def test_second_run_skips_all(
        self, tmp_workspace: Path, fresh_db, patch_model
    ) -> None:
        from wkp.indexer import index_workspace

        index_workspace(fresh_db, tmp_workspace)
        _, skipped = index_workspace(fresh_db, tmp_workspace)
        assert skipped == len(list(tmp_workspace.rglob("*.md")))

    def test_specific_paths_only_indexes_those(
        self, tmp_workspace: Path, fresh_db, patch_model
    ) -> None:
        from wkp.indexer import index_workspace

        target = tmp_workspace / "memory" / "feedback_writing.md"
        indexed, _ = index_workspace(fresh_db, tmp_workspace, paths=[target])
        assert indexed == 1

        count = fresh_db.execute("SELECT count(*) FROM knowledge_items").fetchone()[0]
        assert count == 1

    def test_tier1_types_get_tier_1(
        self, tmp_workspace: Path, fresh_db, patch_model
    ) -> None:
        from wkp.indexer import index_workspace

        index_workspace(fresh_db, tmp_workspace)
        rows = fresh_db.execute(
            "SELECT path, type, tier FROM knowledge_items WHERE tier = 1"
        ).fetchall()
        tier1_types = {r["type"] for r in rows}
        assert "feedback" in tier1_types
        assert "project-state" in tier1_types

    def test_tier2_types_get_tier_2(
        self, tmp_workspace: Path, fresh_db, patch_model
    ) -> None:
        from wkp.indexer import index_workspace

        index_workspace(fresh_db, tmp_workspace)
        rows = fresh_db.execute(
            "SELECT type, tier FROM knowledge_items WHERE type = 'reference'"
        ).fetchall()
        assert all(r["tier"] == 2 for r in rows)
