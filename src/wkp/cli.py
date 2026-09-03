"""WKP command-line interface."""
from __future__ import annotations

from pathlib import Path

import click

WKP_DIR_NAME = ".wkp"
DB_NAME = "index.db"


def _find_workspace_root(start: Path = Path(".")) -> Path:
    """Walk up from start until we find a .git directory (workspace root)."""
    current = start.resolve()
    for parent in [current, *current.parents]:
        if (parent / ".git").exists():
            return parent
    return current  # fallback: cwd


def _open_db(workspace_root: Path):
    from .db import connect

    return connect(workspace_root / WKP_DIR_NAME / DB_NAME)


@click.group()
@click.version_option(package_name="agent-wkp")
def main() -> None:
    """Workspace Knowledge Protocol — progressive disclosure knowledge index."""


# ---------------------------------------------------------------------------
# wkp init
# ---------------------------------------------------------------------------

@main.command()
@click.option("--workspace", default=".", help="Workspace root (default: git root)")
def init(workspace: str) -> None:
    """Initialise WKP index in this workspace."""
    root = _find_workspace_root(Path(workspace))
    wkp_dir = root / WKP_DIR_NAME

    conn = _open_db(root)
    conn.close()

    gitignore = root / ".gitignore"
    entry = f"\n# WKP index (derived, rebuilds from markdown)\n{WKP_DIR_NAME}/\n"
    if gitignore.exists():
        if WKP_DIR_NAME not in gitignore.read_text():
            gitignore.write_text(gitignore.read_text() + entry)
            click.echo(f"Added {WKP_DIR_NAME}/ to .gitignore")
    else:
        gitignore.write_text(entry.lstrip())
        click.echo("Created .gitignore")

    click.echo(f"Initialised WKP at {wkp_dir}")
    click.echo("Next: wkp index  (full index)")
    click.echo("      wkp hooks  (generate git + session hooks)")


# ---------------------------------------------------------------------------
# wkp index
# ---------------------------------------------------------------------------

@main.command()
@click.argument("files", nargs=-1, type=click.Path(exists=True))
@click.option("--force", is_flag=True, default=False,
              help="Re-index all files regardless of SHA")
@click.option("--workspace", default=".", help="Workspace root")
def index(files: tuple[str, ...], force: bool, workspace: str) -> None:
    """Index markdown files into the WKP database.

    With no arguments, indexes all markdown files under the workspace.
    Pass specific files to index only those (used by the post-commit hook).
    Unchanged files are skipped automatically via git blob SHA comparison.
    """
    from .indexer import index_workspace

    root = _find_workspace_root(Path(workspace))
    conn = _open_db(root)

    paths = [Path(f) for f in files] if files else None
    indexed, skipped = index_workspace(conn, root, paths=paths, force=force)
    conn.close()

    click.echo(f"Indexed {indexed} files, skipped {skipped} (unchanged)")


# ---------------------------------------------------------------------------
# wkp search
# ---------------------------------------------------------------------------

@main.command()
@click.argument("query")
@click.option("--tier", default=2, show_default=True, help="Max tier to include")
@click.option("--budget", default=8000, show_default=True, help="Token budget")
@click.option("--workspace", default=".", help="Workspace root")
@click.option("--format", "fmt", default="text",
              type=click.Choice(["text", "json", "paths"]), show_default=True)
@click.option("-k", default=10, show_default=True, help="Max results")
def search(query: str, tier: int, budget: int, workspace: str, fmt: str, k: int) -> None:
    """Hybrid semantic + keyword search over the knowledge index."""
    import json as _json

    from .retriever import search as _search

    root = _find_workspace_root(Path(workspace))
    conn = _open_db(root)
    results = _search(conn, query, max_tier=tier, token_budget=budget, k=k)
    conn.close()

    if fmt == "json":
        click.echo(_json.dumps([
            {"path": r.path, "title": r.title, "score": r.score,
             "tier": r.tier, "tokens": r.tokens}
            for r in results
        ], indent=2))
    elif fmt == "paths":
        for r in results:
            click.echo(r.path)
    else:
        for r in results:
            tier_label = f"T{r.tier}" if r.tier else "T?"
            click.echo(
                f"[{tier_label}] {r.title or r.path}  "
                f"(score={r.score:.3f}, ~{r.tokens or '?'}t)"
            )
            click.echo(f"     {r.path}")


# ---------------------------------------------------------------------------
# wkp context
# ---------------------------------------------------------------------------

@main.command()
@click.argument("topic")
@click.option("--tier", default=2, show_default=True)
@click.option("--budget", default=8000, show_default=True)
@click.option("--workspace", default=".", help="Workspace root")
@click.option("--format", "fmt", default="text",
              type=click.Choice(["text", "paths"]), show_default=True)
def context(topic: str, tier: int, budget: int, workspace: str, fmt: str) -> None:
    """Assemble tier-aware context for a topic within a token budget."""
    from .retriever import context_assemble

    root = _find_workspace_root(Path(workspace))
    conn = _open_db(root)
    results = context_assemble(conn, topic, tier=tier, token_budget=budget)
    conn.close()

    if fmt == "paths":
        for r in results:
            click.echo(r.path)
        return

    total = sum(r.tokens or 0 for r in results)
    click.echo(f"# WKP Context: {topic!r}  ({total} tokens)\n")
    for r in results:
        hop = f" +{r.hop_distance}hop" if r.hop_distance else ""
        click.echo(f"## {r.title or r.path}  [T{r.tier}{hop}]")
        full = root / r.path
        if full.exists():
            from .okf import parse
            _, body = parse(full)
            click.echo(body[:1500])
            if len(body) > 1500:
                click.echo("_(truncated)_")
        click.echo()


# ---------------------------------------------------------------------------
# wkp traverse
# ---------------------------------------------------------------------------

@main.command()
@click.argument("path")
@click.option("--depth", default=3, show_default=True)
@click.option("--budget", default=8000, show_default=True)
@click.option("--workspace", default=".", help="Workspace root")
def traverse(path: str, depth: int, budget: int, workspace: str) -> None:
    """Show all items reachable from PATH via explicit edges."""
    from .retriever import traverse as _traverse

    root = _find_workspace_root(Path(workspace))
    conn = _open_db(root)
    results = _traverse(conn, path, max_depth=depth, token_budget=budget)
    conn.close()

    for r in results:
        indent = "  " * r.hop_distance
        click.echo(f"{indent}[+{r.hop_distance}] {r.title or r.path}  (~{r.tokens or '?'}t)")
        click.echo(f"{indent}    {r.path}")


# ---------------------------------------------------------------------------
# wkp materialize
# ---------------------------------------------------------------------------

@main.command()
@click.option("--tier", default=0, type=click.Choice(["0", "1"]), show_default=True)
@click.option("--workspace", default=".", help="Workspace root")
def materialize(tier: str, workspace: str) -> None:
    """Pre-assemble Tier 0 or 1 context into a static file for zero-latency injection."""
    from .materializer import materialize_tier0, materialize_tier1

    root = _find_workspace_root(Path(workspace))
    wkp_dir = root / WKP_DIR_NAME
    conn = _open_db(root)

    if tier == "0":
        out = wkp_dir / "tier0.md"
        tokens = materialize_tier0(conn, root, out)
        click.echo(f"Tier 0 → {out}  (~{tokens} tokens)")
    else:
        out = wkp_dir / "tier1.md"
        tokens = materialize_tier1(conn, root, out)
        click.echo(f"Tier 1 → {out}  (~{tokens} tokens)")

    conn.close()


# ---------------------------------------------------------------------------
# wkp hooks
# ---------------------------------------------------------------------------

@main.command()
@click.option("--framework", default="claude_code",
              type=click.Choice(["claude_code"]), show_default=True)
@click.option("--post-commit", "post_commit", is_flag=True, default=False,
              help="Also install git post-commit hook")
@click.option("--workspace", default=".", help="Workspace root")
def hooks(framework: str, post_commit: bool, workspace: str) -> None:
    """Generate and install hook scripts."""
    from .materializer import generate_post_commit_hook, generate_session_hook

    root = _find_workspace_root(Path(workspace))
    wkp_dir = root / WKP_DIR_NAME

    # Session hook
    if framework == "claude_code":
        hooks_dir = root / ".claude" / "hooks"
        hooks_dir.mkdir(parents=True, exist_ok=True)
        hook_file = hooks_dir / "wkp-session-start.sh"
        hook_file.write_text(generate_session_hook(wkp_dir, framework))
        hook_file.chmod(0o755)
        click.echo(f"Session hook → {hook_file}")

    # Post-commit hook
    if post_commit:
        git_hooks = root / ".git" / "hooks"
        git_hooks.mkdir(parents=True, exist_ok=True)
        pc_file = git_hooks / "post-commit"
        if pc_file.exists():
            existing = pc_file.read_text()
            if "wkp" not in existing:
                pc_file.write_text(existing + "\n" + generate_post_commit_hook())
                click.echo(f"Appended to existing {pc_file}")
        else:
            pc_file.write_text(generate_post_commit_hook())
            pc_file.chmod(0o755)
            click.echo(f"Post-commit hook → {pc_file}")


# ---------------------------------------------------------------------------
# wkp analyze
# ---------------------------------------------------------------------------

@main.command()
@click.option("--top", default=10, show_default=True, help="Number of promotion candidates")
@click.option("--workspace", default=".", help="Workspace root")
def analyze(top: int, workspace: str) -> None:
    """Run PageRank analysis and suggest Tier 1 promotions (requires networkx)."""
    from .graph import suggest_tier_promotions

    root = _find_workspace_root(Path(workspace))
    conn = _open_db(root)
    candidates = suggest_tier_promotions(conn, top_n=top)
    conn.close()

    if not candidates:
        click.echo("No promotion candidates found (index may be empty).")
        return

    click.echo(f"Top {len(candidates)} items by PageRank — consider promoting to Tier 1:\n")
    for c in candidates:
        click.echo(f"  score={c.pagerank_score:.4f}  [{c.reason}]")
        click.echo(f"  {c.title or c.path}")
        click.echo("  → add 'type: feedback' or 'type: project-state' to OKF frontmatter")
        click.echo(f"  path: {c.path}\n")
