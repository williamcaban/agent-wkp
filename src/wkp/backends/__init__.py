"""VectorBackend protocol — swap sqlite-vec for ChromaDB or pgvector without changing callers.

The default implementation (SqliteVecBackend) is embedded in indexer.py and retriever.py
directly for simplicity. This protocol exists so alternative backends can be built and
validated against the same interface.

Planned backends:
  SqliteVecBackend  — current default (sqlite-vec + FTS5, single file)
  ChromaDBBackend   — for larger corpora (>100k items)
  PGVectorBackend   — for multi-agent / memory-hub cluster deployments
"""
from typing import Protocol, runtime_checkable


@runtime_checkable
class VectorBackend(Protocol):
    def upsert(self, path: str, embedding: list[float], metadata: dict, content: str) -> None: ...
    def search(self, query_embedding: list[float], filters: dict, k: int) -> list[dict]: ...
    def delete(self, path: str) -> None: ...
    def rebuild_from(self, source_dirs: list) -> None: ...
