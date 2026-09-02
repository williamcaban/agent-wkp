"""Tests for materializer.py — Tier 0/1 file generation and hook scripts."""
from pathlib import Path


class TestMaterializeTier0:
    def test_creates_output_file(self, populated_db) -> None:
        from wkp.materializer import materialize_tier0

        conn, workspace = populated_db
        out = workspace / ".wkp" / "tier0.md"
        tokens = materialize_tier0(conn, workspace, out)
        assert out.exists()
        assert isinstance(tokens, int)
        assert tokens >= 0

    def test_file_contains_tier1_content(self, populated_db) -> None:
        from wkp.materializer import materialize_tier0

        conn, workspace = populated_db
        out = workspace / ".wkp" / "tier0.md"
        materialize_tier0(conn, workspace, out)
        content = out.read_text()
        # Tier 1 = feedback + project-state + skill types
        # Our sample has 'feedback' and 'project-state' items
        assert "WKP Tier 0" in content

    def test_respects_token_budget(self, populated_db) -> None:
        from wkp.materializer import materialize_tier0, _TIER0_TOKEN_BUDGET

        conn, workspace = populated_db
        out = workspace / ".wkp" / "tier0.md"
        tokens = materialize_tier0(conn, workspace, out)
        assert tokens <= _TIER0_TOKEN_BUDGET

    def test_creates_parent_directories(self, populated_db) -> None:
        from wkp.materializer import materialize_tier0

        conn, workspace = populated_db
        out = workspace / ".wkp" / "nested" / "dir" / "tier0.md"
        materialize_tier0(conn, workspace, out)
        assert out.exists()


class TestMaterializeTier1:
    def test_creates_output_file(self, populated_db) -> None:
        from wkp.materializer import materialize_tier1

        conn, workspace = populated_db
        out = workspace / ".wkp" / "tier1.md"
        tokens = materialize_tier1(conn, workspace, out)
        assert out.exists()
        assert isinstance(tokens, int)

    def test_file_contains_index_entries(self, populated_db) -> None:
        from wkp.materializer import materialize_tier1

        conn, workspace = populated_db
        out = workspace / ".wkp" / "tier1.md"
        materialize_tier1(conn, workspace, out)
        content = out.read_text()
        assert "WKP Knowledge Index" in content
        # Should list our sample items
        assert "Writing Style Feedback" in content or "feedback_writing" in content

    def test_empty_db_produces_header_only(self, fresh_db, tmp_path: Path) -> None:
        from wkp.materializer import materialize_tier1

        out = tmp_path / "tier1.md"
        materialize_tier1(fresh_db, tmp_path, out)
        assert out.exists()
        content = out.read_text()
        assert "WKP Knowledge Index" in content


class TestGenerateSessionHook:
    def test_returns_bash_script(self, tmp_path: Path) -> None:
        from wkp.materializer import generate_session_hook

        wkp_dir = tmp_path / ".wkp"
        script = generate_session_hook(wkp_dir, "claude_code")
        assert script.startswith("#!/bin/bash")

    def test_contains_absolute_tier0_path(self, tmp_path: Path) -> None:
        from wkp.materializer import generate_session_hook

        wkp_dir = tmp_path / ".wkp"
        script = generate_session_hook(wkp_dir, "claude_code")
        # Should contain absolute path to tier0.md
        assert "tier0.md" in script
        assert str(wkp_dir.resolve()) in script

    def test_unknown_framework_raises(self, tmp_path: Path) -> None:
        from wkp.materializer import generate_session_hook
        import pytest

        with pytest.raises(ValueError, match="Unknown framework"):
            generate_session_hook(tmp_path / ".wkp", "unknown_framework")


class TestGeneratePostCommitHook:
    def test_returns_bash_script(self) -> None:
        from wkp.materializer import generate_post_commit_hook

        script = generate_post_commit_hook()
        assert script.startswith("#!/bin/bash")

    def test_calls_wkp_index(self) -> None:
        from wkp.materializer import generate_post_commit_hook

        script = generate_post_commit_hook()
        assert "wkp index" in script

    def test_calls_wkp_materialize(self) -> None:
        from wkp.materializer import generate_post_commit_hook

        script = generate_post_commit_hook()
        assert "wkp materialize" in script

    def test_does_not_contain_incremental_flag(self) -> None:
        from wkp.materializer import generate_post_commit_hook

        script = generate_post_commit_hook()
        assert "--incremental" not in script
