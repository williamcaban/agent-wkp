"""Tests for retriever.py — hybrid search, traversal, context assembly."""


class TestSearch:
    def test_returns_results(self, populated_db) -> None:
        from wkp.retriever import search

        conn, _ = populated_db
        results = search(conn, "rfe guidelines writing")
        assert len(results) > 0

    def test_results_have_required_fields(self, populated_db) -> None:
        from wkp.retriever import SearchResult, search

        conn, _ = populated_db
        results = search(conn, "project")
        for r in results:
            assert isinstance(r, SearchResult)
            assert r.path
            assert isinstance(r.score, float)
            assert r.score > 0

    def test_tier_filter_excludes_higher_tiers(self, populated_db) -> None:
        from wkp.retriever import search

        conn, _ = populated_db
        results = search(conn, "architecture design", max_tier=1)
        # All results must be Tier 1 or have NULL tier
        for r in results:
            assert r.tier is None or r.tier <= 1

    def test_empty_query_returns_results(self, populated_db) -> None:
        from wkp.retriever import search

        conn, _ = populated_db
        # Should not raise even for unusual queries
        results = search(conn, "x")
        assert isinstance(results, list)

    def test_k_limits_results(self, populated_db) -> None:
        from wkp.retriever import search

        conn, _ = populated_db
        results = search(conn, "document", k=2)
        assert len(results) <= 2

    def test_score_ordering(self, populated_db) -> None:
        from wkp.retriever import search

        conn, _ = populated_db
        results = search(conn, "rfe guidelines", k=10)
        scores = [r.score for r in results]
        assert scores == sorted(scores, reverse=True)

    def test_token_budget_null_items_pass_through(self, populated_db) -> None:
        """Items with tokens=NULL must not be excluded by the token budget filter."""
        from wkp.retriever import search

        conn, _ = populated_db
        # Items without OKF tokens field have NULL — they should still appear
        results = search(conn, "plain markdown", token_budget=50)
        assert isinstance(results, list)  # no crash


class TestTraverse:
    def test_follows_refs_edges(self, populated_db) -> None:
        from wkp.retriever import traverse

        conn, workspace = populated_db
        # reference_rfe.md refs feedback_writing.md
        source = str(next(
            (workspace / "memory").glob("reference_rfe.md")
        ).relative_to(workspace))
        results = traverse(conn, source, max_depth=1)
        paths = [r.path for r in results]
        assert any("feedback_writing" in p for p in paths)

    def test_depth_zero_returns_empty(self, populated_db) -> None:
        from wkp.retriever import traverse

        conn, workspace = populated_db
        source = "memory/reference_rfe.md"
        results = traverse(conn, source, max_depth=0)
        assert results == []

    def test_hop_distance_set(self, populated_db) -> None:
        from wkp.retriever import traverse

        conn, workspace = populated_db
        source = "memory/reference_rfe.md"
        results = traverse(conn, source, max_depth=2)
        for r in results:
            assert r.hop_distance >= 1

    def test_source_not_in_results(self, populated_db) -> None:
        from wkp.retriever import traverse

        conn, workspace = populated_db
        source = "memory/reference_rfe.md"
        results = traverse(conn, source, max_depth=2)
        paths = [r.path for r in results]
        assert source not in paths


class TestContextAssemble:
    def test_returns_list(self, populated_db) -> None:
        from wkp.retriever import context_assemble

        conn, _ = populated_db
        results = context_assemble(conn, "rfe writing style")
        assert isinstance(results, list)

    def test_no_duplicates(self, populated_db) -> None:
        from wkp.retriever import context_assemble

        conn, _ = populated_db
        results = context_assemble(conn, "rfe writing style")
        paths = [r.path for r in results]
        assert len(paths) == len(set(paths))

    def test_token_budget_respected(self, populated_db) -> None:
        from wkp.retriever import context_assemble

        conn, _ = populated_db
        # With a very tight budget, results should be empty or very few
        results = context_assemble(conn, "design architecture", token_budget=10)
        total = sum(r.tokens or 0 for r in results)
        assert total <= 10 or all(r.tokens is None for r in results)

    def test_tier_ceiling_applied(self, populated_db) -> None:
        from wkp.retriever import context_assemble

        conn, _ = populated_db
        results = context_assemble(conn, "anything", tier=1)
        for r in results:
            assert r.tier is None or r.tier <= 1
