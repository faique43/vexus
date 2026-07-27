CREATE TABLE meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE files (
  id INTEGER PRIMARY KEY,
  path TEXT NOT NULL UNIQUE,
  lang TEXT NOT NULL,
  hash BLOB NOT NULL,
  indexed_at INTEGER NOT NULL
);

CREATE TABLE symbols (
  id INTEGER PRIMARY KEY,
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  qualname TEXT NOT NULL,
  kind TEXT NOT NULL,
  sig TEXT,
  start_line INTEGER NOT NULL,
  end_line INTEGER NOT NULL,
  parent_id INTEGER REFERENCES symbols(id),
  arity INTEGER
);
CREATE INDEX idx_symbols_name ON symbols(name);
CREATE INDEX idx_symbols_file ON symbols(file_id);

CREATE TABLE edges (
  id INTEGER PRIMARY KEY,
  src_id INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  dst_name TEXT NOT NULL,
  dst_arity INTEGER,
  dst_id INTEGER REFERENCES symbols(id) ON DELETE SET NULL,
  confidence TEXT NOT NULL
);
CREATE INDEX idx_edges_src ON edges(src_id);
CREATE INDEX idx_edges_dst ON edges(dst_id);
CREATE INDEX idx_edges_dst_name ON edges(dst_name);

CREATE TABLE chunks (
  id INTEGER PRIMARY KEY,
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  symbol_id INTEGER REFERENCES symbols(id) ON DELETE SET NULL,
  start_line INTEGER NOT NULL,
  end_line INTEGER NOT NULL,
  content TEXT NOT NULL,
  content_hash BLOB NOT NULL,
  token_count INTEGER NOT NULL,
  -- 1 once this chunk's vector has been written to vec_chunks. Kept here,
  -- in an ordinary indexed column, rather than derived by anti-joining
  -- vec_chunks: vec0 is a virtual table, so `LEFT JOIN vec_chunks ... WHERE
  -- chunk_id IS NULL` degrades to a scan of the whole vector table per
  -- candidate row. That made "is anything left to embed?" — asked after
  -- every single-file watcher update — cost ~1.1s on a 500-file repo.
  embedded INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_chunks_embedded ON chunks(embedded) WHERE embedded = 0;

CREATE VIRTUAL TABLE fts_chunks USING fts5(
  content, content='chunks', content_rowid='id'
);

CREATE TRIGGER chunks_ai AFTER INSERT ON chunks BEGIN
  INSERT INTO fts_chunks(rowid, content) VALUES (new.id, new.content);
END;
CREATE TRIGGER chunks_ad AFTER DELETE ON chunks BEGIN
  INSERT INTO fts_chunks(fts_chunks, rowid, content) VALUES ('delete', old.id, old.content);
END;

CREATE TABLE embed_cache (
  content_hash BLOB PRIMARY KEY,
  embedding BLOB NOT NULL
);
