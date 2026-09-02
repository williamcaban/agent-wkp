"""Tests for okf.py — frontmatter parsing and edge extraction."""
from pathlib import Path

import pytest


def write(tmp_path: Path, name: str, content: str) -> Path:
    p = tmp_path / name
    p.write_text(content, encoding="utf-8")
    return p


class TestParse:
    def test_valid_frontmatter(self, tmp_path: Path) -> None:
        from wkp.okf import parse

        p = write(tmp_path, "a.md", "---\ntitle: Hello\ntype: feedback\ntokens: 100\n---\nBody text.\n")
        meta, body = parse(p)
        assert meta["title"] == "Hello"
        assert meta["type"] == "feedback"
        assert meta["tokens"] == 100
        assert body == "Body text."

    def test_no_frontmatter(self, tmp_path: Path) -> None:
        from wkp.okf import parse

        p = write(tmp_path, "b.md", "Just plain content.\n")
        meta, body = parse(p)
        assert meta == {}
        assert "plain content" in body

    def test_malformed_yaml_returns_empty_meta(self, tmp_path: Path) -> None:
        from wkp.okf import parse

        p = write(tmp_path, "c.md", "---\ntitle: Bad: value: with: colons\n---\nContent.\n")
        meta, body = parse(p)
        assert meta == {}
        assert "Content." in body

    def test_empty_file(self, tmp_path: Path) -> None:
        from wkp.okf import parse

        p = write(tmp_path, "empty.md", "")
        meta, body = parse(p)
        assert meta == {}
        assert body == ""

    def test_frontmatter_with_list_tags(self, tmp_path: Path) -> None:
        from wkp.okf import parse

        p = write(tmp_path, "tags.md", "---\ntitle: T\ntags: [a, b, c]\n---\nBody.\n")
        meta, _ = parse(p)
        assert meta["tags"] == ["a", "b", "c"]

    def test_unclosed_frontmatter_treated_as_no_frontmatter(self, tmp_path: Path) -> None:
        from wkp.okf import parse

        p = write(tmp_path, "unclosed.md", "---\ntitle: Missing close\nBody text.\n")
        meta, body = parse(p)
        assert meta == {}


class TestGitBlobSha:
    def test_returns_string(self, tmp_path: Path) -> None:
        from wkp.okf import git_blob_sha

        p = write(tmp_path, "x.md", "content")
        result = git_blob_sha(p)
        # Returns either a 40-char hex string (in a git repo) or empty string
        assert isinstance(result, str)

    def test_nonexistent_path_returns_empty(self, tmp_path: Path) -> None:
        from wkp.okf import git_blob_sha

        result = git_blob_sha(tmp_path / "does_not_exist.md")
        assert result == ""


class TestExtractEdges:
    def test_refs_become_edges(self, tmp_path: Path) -> None:
        from wkp.okf import extract_edges

        # source_path is workspace-relative; refs are relative to the source file
        # "memory/ref.md" refs "memory/other.md" → same directory, ref is "../memory/other.md" or "other.md"
        source = str(tmp_path / "memory" / "ref.md")
        meta = {"refs": ["other.md"]}   # relative to source file's directory
        body = ""
        edges = extract_edges(source, meta, body, tmp_path)
        ref_edges = [e for e in edges if e[2] == "refs"]
        assert len(ref_edges) == 1
        assert "other.md" in ref_edges[0][1]

    def test_wikilinks_become_mention_edges(self, tmp_path: Path) -> None:
        from wkp.okf import extract_edges

        # Create target so wikilink can resolve
        target = tmp_path / "memory" / "feedback_writing.md"
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text("---\ntitle: T\n---\nContent.\n")

        source = "memory/ref.md"
        body = "See [[feedback_writing]] for details."
        edges = extract_edges(source, {}, body, tmp_path)
        mention_targets = [e[1] for e in edges if e[2] == "mentions"]
        assert any("feedback_writing" in t for t in mention_targets)

    def test_empty_refs_produces_no_edges(self, tmp_path: Path) -> None:
        from wkp.okf import extract_edges

        edges = extract_edges("memory/x.md", {}, "No refs here.", tmp_path)
        assert edges == []

    def test_invalid_ref_path_is_skipped(self, tmp_path: Path) -> None:
        from wkp.okf import extract_edges

        meta = {"refs": ["../../../../etc/passwd"]}
        edges = extract_edges("memory/x.md", meta, "", tmp_path)
        # Should not raise; invalid paths outside workspace are silently dropped
        ref_edges = [e for e in edges if e[2] == "refs"]
        assert ref_edges == []


class TestHelpers:
    def test_tags_to_json_list(self) -> None:
        from wkp.okf import tags_to_json
        import json

        result = json.loads(tags_to_json(["a", "b"]))
        assert result == ["a", "b"]

    def test_tags_to_json_none(self) -> None:
        from wkp.okf import tags_to_json

        assert tags_to_json(None) == "[]"

    def test_refs_to_json_list(self) -> None:
        from wkp.okf import refs_to_json
        import json

        result = json.loads(refs_to_json(["x.md", "y.md"]))
        assert result == ["x.md", "y.md"]
