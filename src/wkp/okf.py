"""OKF frontmatter parser and edge extractor."""
import json
import os
import re
import subprocess
from pathlib import Path
from typing import Any

import yaml

WIKILINK_RE = re.compile(r'\[\[([^\]|#]+)[|\]#]')
# Matches [[name]], [[name|alias]], [[name#section]]


def parse(path: Path) -> tuple[dict[str, Any], str]:
    """Return (metadata, body) from a markdown file with OKF frontmatter.
    Returns ({}, raw) on any parse error rather than raising."""
    raw = path.read_text(encoding="utf-8", errors="replace")
    if not raw.startswith("---"):
        return {}, raw
    try:
        end = raw.index("---", 3)
    except ValueError:
        return {}, raw
    try:
        meta = yaml.safe_load(raw[3:end]) or {}
        if not isinstance(meta, dict):
            meta = {}
    except yaml.YAMLError:
        meta = {}
    body = raw[end + 3 :].strip()
    return meta, body


def git_blob_sha(path: Path) -> str:
    """Return git blob SHA for path; empty string if untracked or git unavailable."""
    result = subprocess.run(
        ["git", "hash-object", str(path)],
        capture_output=True,
        text=True,
    )
    return result.stdout.strip() if result.returncode == 0 else ""


def extract_edges(
    source_path: str,
    meta: dict[str, Any],
    body: str,
    workspace_root: Path,
) -> list[tuple[str, str, str]]:
    """Return list of (source, target, edge_type) tuples from OKF refs and wikilinks."""
    edges: list[tuple[str, str, str]] = []
    source = Path(source_path)

    # OKF refs (relative paths) — normalise without requiring targets to exist on disk
    for ref in meta.get("refs") or []:
        try:
            # normpath collapses ".." without hitting the filesystem
            raw = Path(source).parent / ref
            normalised = Path(os.path.normpath(raw))
            # Reject paths that escape the workspace root
            normalised.relative_to(workspace_root)
            edges.append((source_path, str(normalised), "refs"))
        except ValueError:
            pass

    # Wikilinks in body: [[name]] resolved via name lookup (best-effort)
    for m in WIKILINK_RE.finditer(body):
        name = m.group(1).strip()
        target = _resolve_wikilink(name, workspace_root)
        if target:
            edges.append((source_path, target, "mentions"))

    return edges


def _resolve_wikilink(name: str, workspace_root: Path) -> str | None:
    """Find a knowledge file whose stem matches the wikilink name."""
    # Search memory/ and knowledge/ directories
    for pattern in (f"**/{name}.md", f"**/memory/{name}.md"):
        matches = list(workspace_root.glob(pattern))
        if matches:
            try:
                return str(matches[0].resolve().relative_to(workspace_root))
            except ValueError:
                pass
    return None


def tags_to_json(tags: Any) -> str:
    if not tags:
        return "[]"
    if isinstance(tags, list):
        return json.dumps([str(t) for t in tags])
    return json.dumps([str(tags)])


def refs_to_json(refs: Any) -> str:
    if not refs:
        return "[]"
    if isinstance(refs, list):
        return json.dumps([str(r) for r in refs])
    return json.dumps([str(refs)])
