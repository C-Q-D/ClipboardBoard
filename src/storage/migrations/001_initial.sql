-- 此 SQL 文件定义 ClipboardBoard 的 v1 文本历史表、唯一内容哈希索引和复合时间索引。
-- 迁移调用方必须在事务中执行本文件，并在提交前完成结构校验。

CREATE TABLE IF NOT EXISTS clipboard_items (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    item_type TEXT NOT NULL,
    text_content TEXT,
    preview_text TEXT NOT NULL,
    content_hash BLOB NOT NULL,
    source_exe TEXT,
    source_app TEXT,
    copy_count INTEGER NOT NULL DEFAULT 1,
    is_pinned INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    copied_at INTEGER NOT NULL,
    last_used_at INTEGER
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_clipboard_items_content_hash
    ON clipboard_items(content_hash);

CREATE INDEX IF NOT EXISTS idx_clipboard_items_copied
    ON clipboard_items(copied_at DESC, id DESC);
