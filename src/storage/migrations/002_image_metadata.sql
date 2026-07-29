-- 此 SQL 文件把 v1 文本历史表事务重建为 v2，并增加图片资产根注册表。
-- 图片字段采用 all-or-none 约束；已有非图片记录保持原字段并让新增字段为空。

CREATE TABLE image_asset_roots (
    root_id BLOB PRIMARY KEY
        CHECK (typeof(root_id) = 'blob' AND length(root_id) = 32),
    root_path TEXT NOT NULL UNIQUE
        CHECK (typeof(root_path) = 'text' AND length(root_path) > 0),
    root_kind TEXT NOT NULL
        CHECK (typeof(root_kind) = 'text' AND root_kind IN ('default', 'custom')),
    created_at INTEGER NOT NULL
        CHECK (typeof(created_at) = 'integer')
);

CREATE TABLE clipboard_items_v2 (
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
    last_used_at INTEGER,
    image_root_id BLOB,
    image_path TEXT,
    thumbnail_path TEXT,
    image_width INTEGER,
    image_height INTEGER,
    image_format TEXT,
    content_size INTEGER,
    FOREIGN KEY (image_root_id) REFERENCES image_asset_roots(root_id) ON DELETE RESTRICT,
    CHECK (
        item_type = 'image'
        OR (
            image_root_id IS NULL
            AND image_path IS NULL
            AND thumbnail_path IS NULL
            AND image_width IS NULL
            AND image_height IS NULL
            AND image_format IS NULL
            AND content_size IS NULL
        )
    ),
    CHECK (
        item_type <> 'image'
        OR (
            typeof(content_hash) = 'blob'
            AND length(content_hash) = 32
            AND typeof(image_root_id) = 'blob'
            AND length(image_root_id) = 32
            AND typeof(image_path) = 'text'
            AND length(image_path) = 71
            AND substr(image_path, 1, 2) = substr(lower(hex(content_hash)), 1, 2)
            AND substr(image_path, 3, 1) = '/'
            AND substr(image_path, 4, 64) = lower(hex(content_hash))
            AND substr(image_path, 68, 4) = '.png'
            AND typeof(thumbnail_path) = 'text'
            AND length(thumbnail_path) = 72
            AND substr(thumbnail_path, 1, 2) = substr(lower(hex(content_hash)), 1, 2)
            AND substr(thumbnail_path, 3, 1) = '/'
            AND substr(thumbnail_path, 4, 64) = lower(hex(content_hash))
            AND substr(thumbnail_path, 68, 5) = '.webp'
            AND typeof(image_width) = 'integer'
            AND image_width BETWEEN 1 AND 4294967295
            AND typeof(image_height) = 'integer'
            AND image_height BETWEEN 1 AND 4294967295
            AND typeof(image_format) = 'text'
            AND image_format = 'png'
            AND typeof(content_size) = 'integer'
            AND content_size BETWEEN 1 AND 9223372036854775807
        )
    )
);

INSERT INTO clipboard_items_v2 (
    id, item_type, text_content, preview_text, content_hash, source_exe, source_app,
    copy_count, is_pinned, created_at, copied_at, last_used_at,
    image_root_id, image_path, thumbnail_path, image_width, image_height,
    image_format, content_size
)
SELECT
    id, item_type, text_content, preview_text, content_hash, source_exe, source_app,
    copy_count, is_pinned, created_at, copied_at, last_used_at,
    NULL, NULL, NULL, NULL, NULL, NULL, NULL
FROM clipboard_items;

DROP TABLE clipboard_items;
ALTER TABLE clipboard_items_v2 RENAME TO clipboard_items;

CREATE UNIQUE INDEX idx_clipboard_items_content_hash
    ON clipboard_items(content_hash);

CREATE INDEX idx_clipboard_items_copied
    ON clipboard_items(copied_at DESC, id DESC);
