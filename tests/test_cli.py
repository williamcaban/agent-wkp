"""Integration tests for the CLI — uses Click's CliRunner (no subprocess)."""
from pathlib import Path

import pytest
from click.testing import CliRunner


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


@pytest.fixture
def isolated_runner(tmp_path: Path):
    """CliRunner with an isolated tmp_path as cwd."""
    runner = CliRunner()
    yield runner, tmp_path


class TestInit:
    def test_creates_wkp_directory(self, runner: CliRunner, tmp_path: Path) -> None:
        from wkp.cli import main

        with runner.isolated_filesystem(temp_dir=tmp_path):
            result = runner.invoke(main, ["init"])
            assert result.exit_code == 0, result.output
            assert Path(".wkp").exists()

    def test_creates_db_file(self, runner: CliRunner, tmp_path: Path) -> None:
        from wkp.cli import main

        with runner.isolated_filesystem(temp_dir=tmp_path):
            runner.invoke(main, ["init"])
            assert Path(".wkp/index.db").exists()

    def test_adds_gitignore_entry(self, runner: CliRunner, tmp_path: Path) -> None:
        from wkp.cli import main

        with runner.isolated_filesystem(temp_dir=tmp_path):
            runner.invoke(main, ["init"])
            gitignore = Path(".gitignore").read_text()
            assert ".wkp/" in gitignore

    def test_does_not_duplicate_gitignore_entry(
        self, runner: CliRunner, tmp_path: Path
    ) -> None:
        from wkp.cli import main

        with runner.isolated_filesystem(temp_dir=tmp_path):
            runner.invoke(main, ["init"])
            runner.invoke(main, ["init"])
            gitignore = Path(".gitignore").read_text()
            assert gitignore.count(".wkp/") == 1

    def test_output_mentions_next_steps(self, runner: CliRunner, tmp_path: Path) -> None:
        from wkp.cli import main

        with runner.isolated_filesystem(temp_dir=tmp_path):
            result = runner.invoke(main, ["init"])
            assert "wkp index" in result.output


class TestIndex:
    def test_indexes_files_in_workspace(
        self, runner: CliRunner, tmp_workspace: Path, patch_model
    ) -> None:
        from wkp.cli import main

        # Init first, then index from workspace root
        runner.invoke(main, ["init", "--workspace", str(tmp_workspace)])
        result = runner.invoke(main, ["index", "--workspace", str(tmp_workspace)])
        assert result.exit_code == 0, result.output
        assert "Indexed" in result.output

    def test_force_flag_reindexes(
        self, runner: CliRunner, tmp_workspace: Path, patch_model
    ) -> None:
        from wkp.cli import main

        runner.invoke(main, ["init", "--workspace", str(tmp_workspace)])
        runner.invoke(main, ["index", "--workspace", str(tmp_workspace)])
        result = runner.invoke(
            main, ["index", "--force", "--workspace", str(tmp_workspace)]
        )
        assert result.exit_code == 0, result.output
        assert "Indexed" in result.output


class TestSearch:
    def test_returns_results(
        self, runner: CliRunner, populated_db, patch_model
    ) -> None:
        from wkp.cli import main

        _, workspace = populated_db
        result = runner.invoke(
            main, ["search", "rfe guidelines", "--workspace", str(workspace)]
        )
        assert result.exit_code == 0, result.output

    def test_json_format(
        self, runner: CliRunner, populated_db, patch_model
    ) -> None:
        import json

        from wkp.cli import main

        _, workspace = populated_db
        result = runner.invoke(
            main,
            ["search", "writing style", "--format", "json", "--workspace", str(workspace)],
        )
        assert result.exit_code == 0, result.output
        data = json.loads(result.output)
        assert isinstance(data, list)

    def test_paths_format(
        self, runner: CliRunner, populated_db, patch_model
    ) -> None:
        from wkp.cli import main

        _, workspace = populated_db
        result = runner.invoke(
            main,
            ["search", "project", "--format", "paths", "--workspace", str(workspace)],
        )
        assert result.exit_code == 0, result.output
        for line in result.output.strip().splitlines():
            assert line.endswith(".md")


class TestMaterialize:
    def test_tier0_creates_file(
        self, runner: CliRunner, populated_db
    ) -> None:
        from wkp.cli import main

        _, workspace = populated_db
        result = runner.invoke(
            main, ["materialize", "--tier", "0", "--workspace", str(workspace)]
        )
        assert result.exit_code == 0, result.output
        assert (workspace / ".wkp" / "tier0.md").exists()

    def test_tier1_creates_file(
        self, runner: CliRunner, populated_db
    ) -> None:
        from wkp.cli import main

        _, workspace = populated_db
        result = runner.invoke(
            main, ["materialize", "--tier", "1", "--workspace", str(workspace)]
        )
        assert result.exit_code == 0, result.output
        assert (workspace / ".wkp" / "tier1.md").exists()


class TestHooks:
    def test_session_hook_created(
        self, runner: CliRunner, tmp_workspace: Path, patch_model
    ) -> None:
        from wkp.cli import main

        runner.invoke(main, ["init", "--workspace", str(tmp_workspace)])
        result = runner.invoke(
            main,
            ["hooks", "--framework", "claude_code", "--workspace", str(tmp_workspace)],
        )
        assert result.exit_code == 0, result.output
        hook = tmp_workspace / ".claude" / "hooks" / "wkp-session-start.sh"
        assert hook.exists()
        assert hook.stat().st_mode & 0o111  # executable

    def test_post_commit_hook_created(
        self, runner: CliRunner, tmp_workspace: Path, patch_model
    ) -> None:
        from wkp.cli import main

        # Create a fake .git/hooks dir so the hook can be installed
        (tmp_workspace / ".git" / "hooks").mkdir(parents=True, exist_ok=True)

        runner.invoke(main, ["init", "--workspace", str(tmp_workspace)])
        result = runner.invoke(
            main,
            ["hooks", "--post-commit", "--workspace", str(tmp_workspace)],
        )
        assert result.exit_code == 0, result.output
        hook = tmp_workspace / ".git" / "hooks" / "post-commit"
        assert hook.exists()
        assert "wkp index" in hook.read_text()


class TestAnalyze:
    def test_runs_without_error(
        self, runner: CliRunner, populated_db
    ) -> None:
        from wkp.cli import main

        _, workspace = populated_db
        result = runner.invoke(
            main, ["analyze", "--workspace", str(workspace)]
        )
        assert result.exit_code == 0, result.output

    def test_empty_db_prints_no_candidates(
        self, runner: CliRunner, tmp_path: Path
    ) -> None:
        from wkp.cli import main

        runner.invoke(main, ["init", "--workspace", str(tmp_path)])
        result = runner.invoke(
            main, ["analyze", "--workspace", str(tmp_path)]
        )
        assert result.exit_code == 0, result.output
        assert "No promotion candidates" in result.output
