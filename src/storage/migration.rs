//! 此模块封装 SQLite v1/v2 schema 的事务迁移、结构校验和版本读取。
//!
//! v2 以事务重建表并保留文本与自增高水位；发现坏 schema 或未来版本必须回滚。

use rusqlite::{Connection, OptionalExtension};

use super::StorageError;

/// 当前代码能够读取和验证的最高 schema 版本。
pub(crate) const CURRENT_SCHEMA_VERSION: i64 = 2;

/// v1 表字段的最小结构约束；默认值和索引由 SQL 与后续校验共同固定。
const V1_COLUMNS: [(&str, &str, i64, i64, Option<&str>); 12] = [
    ("id", "INTEGER", 0, 1, None),
    ("item_type", "TEXT", 1, 0, None),
    ("text_content", "TEXT", 0, 0, None),
    ("preview_text", "TEXT", 1, 0, None),
    ("content_hash", "BLOB", 1, 0, None),
    ("source_exe", "TEXT", 0, 0, None),
    ("source_app", "TEXT", 0, 0, None),
    ("copy_count", "INTEGER", 1, 0, Some("1")),
    ("is_pinned", "INTEGER", 1, 0, Some("0")),
    ("created_at", "INTEGER", 1, 0, None),
    ("copied_at", "INTEGER", 1, 0, None),
    ("last_used_at", "INTEGER", 0, 0, None),
];

const V1_SQL: &str = include_str!("migrations/001_initial.sql");
const V2_SQL: &str = include_str!("migrations/002_image_metadata.sql");

/// v2 历史表在 v1 字段后追加图片根、相对路径、尺寸、格式和原图字节数。
const V2_COLUMNS: [(&str, &str, i64, i64, Option<&str>); 19] = [
    ("id", "INTEGER", 0, 1, None),
    ("item_type", "TEXT", 1, 0, None),
    ("text_content", "TEXT", 0, 0, None),
    ("preview_text", "TEXT", 1, 0, None),
    ("content_hash", "BLOB", 1, 0, None),
    ("source_exe", "TEXT", 0, 0, None),
    ("source_app", "TEXT", 0, 0, None),
    ("copy_count", "INTEGER", 1, 0, Some("1")),
    ("is_pinned", "INTEGER", 1, 0, Some("0")),
    ("created_at", "INTEGER", 1, 0, None),
    ("copied_at", "INTEGER", 1, 0, None),
    ("last_used_at", "INTEGER", 0, 0, None),
    ("image_root_id", "BLOB", 0, 0, None),
    ("image_path", "TEXT", 0, 0, None),
    ("thumbnail_path", "TEXT", 0, 0, None),
    ("image_width", "INTEGER", 0, 0, None),
    ("image_height", "INTEGER", 0, 0, None),
    ("image_format", "TEXT", 0, 0, None),
    ("content_size", "INTEGER", 0, 0, None),
];

/// 图片资产根表字段契约；根 ID 绑定图片行，路径可在根移动后按 ID 更新。
const V2_ROOT_COLUMNS: [(&str, &str, i64, i64, Option<&str>); 4] = [
    ("root_id", "BLOB", 0, 1, None),
    ("root_path", "TEXT", 1, 0, None),
    ("root_kind", "TEXT", 1, 0, None),
    ("created_at", "INTEGER", 1, 0, None),
];

/// SQLite `PRAGMA table_info` 返回的字段契约，集中表达名称、类型、约束和默认值。
type ColumnDefinition = (String, String, i64, i64, Option<String>);

/// 在任何迁移事务前启用外键，再将版本 0/1 升级到 v2 或复核现有 v2。
pub(crate) fn migrate(connection: &mut Connection) -> Result<i64, StorageError> {
    enable_foreign_keys(connection)?;
    let version = read_schema_version(connection)?;

    if version > CURRENT_SCHEMA_VERSION {
        return Err(StorageError::UnsupportedSchemaVersion(version));
    }

    let transaction = connection.transaction()?;

    if version == 0 {
        transaction.execute_batch(V1_SQL)?;
    }

    if version <= 1 {
        validate_v1_schema(&transaction)?;
        reject_v1_image_rows(&transaction)?;
        let old_sequence = read_clipboard_sequence(&transaction)?;
        transaction.execute_batch(V2_SQL)?;
        restore_clipboard_sequence(&transaction, old_sequence)?;
    }

    validate_v2_schema(&transaction)?;
    validate_foreign_key_integrity(&transaction)?;

    if version < CURRENT_SCHEMA_VERSION {
        transaction.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)?;
    }

    transaction.commit()?;
    Ok(CURRENT_SCHEMA_VERSION)
}

/// v1 尚未发布图片能力；存在伪图片行时以稳定不兼容错误拒绝，而不是泄漏 CHECK 错误。
fn reject_v1_image_rows(connection: &Connection) -> Result<(), StorageError> {
    let image_count = connection.query_row(
        "SELECT COUNT(*) FROM clipboard_items WHERE item_type = 'image'",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    if image_count != 0 {
        return Err(StorageError::IncompatibleSchema(
            "v1 数据库包含未受契约支持的图片行".to_owned(),
        ));
    }
    Ok(())
}

/// 外键必须在事务外启用；读回值避免驱动或连接配置静默忽略设置。
fn enable_foreign_keys(connection: &Connection) -> Result<(), StorageError> {
    connection.pragma_update(None, "foreign_keys", "ON")?;
    let enabled = connection.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))?;
    if enabled != 1 {
        return Err(StorageError::IncompatibleSchema(
            "SQLite 连接未能启用外键约束".to_owned(),
        ));
    }
    Ok(())
}

/// 读取 SQLite 内置的 schema 版本载体；该函数只在存储线程内被调用。
pub(crate) fn read_schema_version(connection: &Connection) -> Result<i64, StorageError> {
    Ok(connection.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

/// 校验 v1 的表、字段约束和两个稳定查询索引，避免把不兼容库误判为幂等。
fn validate_v1_schema(connection: &Connection) -> Result<(), StorageError> {
    let actual_columns = read_columns(connection, "clipboard_items")?;
    let expected_columns = expected_columns(&V1_COLUMNS);

    if actual_columns != expected_columns {
        return Err(StorageError::IncompatibleSchema(format!(
            "clipboard_items 字段不匹配，实际为 {actual_columns:?}"
        )));
    }

    let create_sql = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'clipboard_items'",
            [],
            |row| row.get::<_, String>(0),
        )?
        .split_whitespace()
        .collect::<String>()
        .to_ascii_uppercase();
    if !create_sql.contains("IDINTEGERPRIMARYKEYAUTOINCREMENT") {
        return Err(StorageError::IncompatibleSchema(
            "clipboard_items 缺少 AUTOINCREMENT 主键约束".to_owned(),
        ));
    }

    validate_index(
        connection,
        "idx_clipboard_items_content_hash",
        true,
        &["content_hash"],
        &[false],
    )?;
    validate_index(
        connection,
        "idx_clipboard_items_copied",
        false,
        &["copied_at", "id"],
        &[true, true],
    )?;

    Ok(())
}

/// 校验 v2 两张表、固定索引、图片 CHECK 和根引用外键。
fn validate_v2_schema(connection: &Connection) -> Result<(), StorageError> {
    let actual_columns = read_columns(connection, "clipboard_items")?;
    if actual_columns != expected_columns(&V2_COLUMNS) {
        return Err(StorageError::IncompatibleSchema(format!(
            "clipboard_items v2 字段不匹配，实际为 {actual_columns:?}"
        )));
    }
    let actual_root_columns = read_columns(connection, "image_asset_roots")?;
    if actual_root_columns != expected_columns(&V2_ROOT_COLUMNS) {
        return Err(StorageError::IncompatibleSchema(format!(
            "image_asset_roots 字段不匹配，实际为 {actual_root_columns:?}"
        )));
    }

    for table in ["clipboard_items", "image_asset_roots"] {
        let actual_sql = table_sql(connection, table)?;
        let expected_sql = canonical_v2_table_sql(table)?;
        if actual_sql != expected_sql {
            return Err(StorageError::IncompatibleSchema(format!(
                "{table} 建表约束与规范 v2 不匹配"
            )));
        }
    }

    validate_index(
        connection,
        "idx_clipboard_items_content_hash",
        true,
        &["content_hash"],
        &[false],
    )?;
    validate_index(
        connection,
        "idx_clipboard_items_copied",
        false,
        &["copied_at", "id"],
        &[true, true],
    )?;
    validate_global_explicit_index_set(connection)?;
    validate_image_root_foreign_key(connection)?;
    Ok(())
}

/// 在隔离内存库执行同一组受版本控制的 SQL，取得不可被现有数据库篡改的规范建表定义。
///
/// 完整比较可覆盖 all-or-none、storage class、路径哈希、尺寸、格式和大小约束，
/// 避免通过抽查子串把被削弱的 v2 schema 误判为兼容。
fn canonical_v2_table_sql(table: &str) -> Result<String, StorageError> {
    let canonical = Connection::open_in_memory()?;
    canonical.execute_batch(V1_SQL)?;
    canonical.execute_batch(V2_SQL)?;
    table_sql(&canonical, table)
}

/// 将静态字段契约转换为 PRAGMA 返回结构，避免 v1/v2 重复拼装。
fn expected_columns(
    definitions: &[(&str, &str, i64, i64, Option<&str>)],
) -> Vec<ColumnDefinition> {
    definitions
        .iter()
        .map(
            |(name, declared_type, not_null, primary_key, default_value)| {
                (
                    (*name).to_owned(),
                    (*declared_type).to_owned(),
                    *not_null,
                    *primary_key,
                    default_value.map(str::to_owned),
                )
            },
        )
        .collect()
}

/// 读取 SQLite 原样保存的建表 SQL；字符串字面量的大小写和空白也是 v2 契约的一部分。
fn table_sql(connection: &Connection, table: &str) -> Result<String, StorageError> {
    Ok(connection.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get::<_, String>(0),
    )?)
}

/// 校验图片根外键的表、列和删除动作，防止孤儿图片记录进入真相源。
fn validate_image_root_foreign_key(connection: &Connection) -> Result<(), StorageError> {
    let mut statement = connection.prepare("PRAGMA foreign_key_list('clipboard_items')")?;
    let foreign_keys = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(6)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if foreign_keys
        != vec![(
            "image_asset_roots".to_owned(),
            "image_root_id".to_owned(),
            "root_id".to_owned(),
            "RESTRICT".to_owned(),
        )]
    {
        return Err(StorageError::IncompatibleSchema(format!(
            "图片根外键不匹配，实际为 {foreign_keys:?}"
        )));
    }
    Ok(())
}

/// 迁移提交前必须确认当前数据库没有孤儿外键；返回任一行即拒绝提交。
fn validate_foreign_key_integrity(connection: &Connection) -> Result<(), StorageError> {
    let mut statement = connection.prepare("PRAGMA foreign_key_check")?;
    if statement.exists([])? {
        return Err(StorageError::IncompatibleSchema(
            "SQLite v2 存在孤儿图片根引用".to_owned(),
        ));
    }
    Ok(())
}

/// 读取 v1 已发放的 AUTOINCREMENT 高水位；没有历史行时返回零。
fn read_clipboard_sequence(connection: &Connection) -> Result<i64, StorageError> {
    Ok(connection
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'clipboard_items'",
            [],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

/// 表重建后恢复旧高水位，避免复用已经删除记录的稳定 ID。
fn restore_clipboard_sequence(
    connection: &Connection,
    old_sequence: i64,
) -> Result<(), StorageError> {
    let current_sequence = read_clipboard_sequence(connection)?;
    let target = old_sequence.max(current_sequence);
    if target > current_sequence {
        let affected = connection.execute(
            "UPDATE sqlite_sequence SET seq = ?1 WHERE name = 'clipboard_items'",
            [target],
        )?;
        if affected == 0 {
            connection.execute(
                "INSERT INTO sqlite_sequence (name, seq) VALUES ('clipboard_items', ?1)",
                [target],
            )?;
        }
    }
    Ok(())
}

/// 读取表字段名、声明类型、NOT NULL、主键和默认值，忽略 SQLite 自动生成的 cid。
fn read_columns(
    connection: &Connection,
    table: &str,
) -> Result<Vec<ColumnDefinition>, StorageError> {
    let pragma = format!("PRAGMA table_info('{table}')");
    let mut statement = connection.prepare(&pragma)?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(5)?,
            row.get(4)?,
        ))
    })?;

    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// 校验索引的唯一性和列顺序，确保后续游标查询可以依赖固定排序。
fn validate_index(
    connection: &Connection,
    index_name: &str,
    expected_unique: bool,
    expected_columns: &[&str],
    expected_descending: &[bool],
) -> Result<(), StorageError> {
    let mut statement = connection.prepare("PRAGMA index_list('clipboard_items')")?;
    let index = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .find(|(name, _, _, _)| name == index_name);

    let Some((_, unique, origin, partial)) = index else {
        return Err(StorageError::IncompatibleSchema(format!(
            "缺少索引 {index_name}"
        )));
    };

    if (unique != 0) != expected_unique {
        return Err(StorageError::IncompatibleSchema(format!(
            "索引 {index_name} 的唯一性不匹配"
        )));
    }

    if origin != "c" || partial != 0 {
        return Err(StorageError::IncompatibleSchema(format!(
            "索引 {index_name} 不是完整的显式索引"
        )));
    }

    let pragma = format!("PRAGMA index_info('{index_name}')");
    let mut statement = connection.prepare(&pragma)?;
    let actual_columns = statement
        .query_map([], |row| row.get::<_, Option<String>>(2))?
        .collect::<Result<Vec<_>, _>>()?;

    let expected_columns = expected_columns
        .iter()
        .map(|column| Some((*column).to_owned()))
        .collect::<Vec<_>>();
    if actual_columns != expected_columns {
        return Err(StorageError::IncompatibleSchema(format!(
            "索引 {index_name} 的列顺序不匹配，实际为 {actual_columns:?}"
        )));
    }

    let xinfo_pragma = format!("PRAGMA index_xinfo('{index_name}')");
    let mut xinfo_statement = connection.prepare(&xinfo_pragma)?;
    let actual_key_columns = xinfo_statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|(_, _, _, _, key)| *key != 0)
        .collect::<Vec<_>>();

    if actual_key_columns.len() != expected_columns.len()
        || actual_key_columns.iter().enumerate().any(
            |(index, (column_id, name, descending, collation, _))| {
                *column_id < 0
                    || name.as_deref() != expected_columns[index].as_deref()
                    || (*descending != 0) != expected_descending[index]
                    || collation != "BINARY"
            },
        )
    {
        return Err(StorageError::IncompatibleSchema(format!(
            "索引 {index_name} 的列、排序或 collation 不匹配"
        )));
    }

    Ok(())
}

/// 主 schema 的显式索引必须恰好是两个规范索引，不能把额外约束藏在其他表上。
fn validate_global_explicit_index_set(connection: &Connection) -> Result<(), StorageError> {
    let mut statement = connection.prepare(
        "SELECT name, tbl_name FROM sqlite_master \
         WHERE type = 'index' AND sql IS NOT NULL ORDER BY name",
    )?;
    let actual = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let expected = vec![
        (
            "idx_clipboard_items_content_hash".to_owned(),
            "clipboard_items".to_owned(),
        ),
        (
            "idx_clipboard_items_copied".to_owned(),
            "clipboard_items".to_owned(),
        ),
    ];
    if actual != expected {
        return Err(StorageError::IncompatibleSchema(format!(
            "主 schema 显式索引集合不匹配，实际为 {actual:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证 v2 迁移兼容、约束、回滚、自增高水位和版本上限。

    use rusqlite::{params, Connection};

    use super::{migrate, read_schema_version, CURRENT_SCHEMA_VERSION, V1_SQL, V2_SQL};

    /// 创建一份真实 v1 schema，供升级测试精确覆盖版本 1 分支。
    fn create_v1(connection: &Connection) {
        connection.execute_batch(V1_SQL).expect("创建 v1 schema 失败");
        connection
            .pragma_update(None, "user_version", 1)
            .expect("标记 v1 版本失败");
    }

    /// 生成合法图片路径，路径文件名与内容哈希保持完全一致。
    fn image_paths(value: u8) -> (Vec<u8>, String, String) {
        let hash = vec![value; 32];
        let hex = format!("{value:02x}").repeat(32);
        (
            hash,
            format!("{}/{hex}.png", &hex[..2]),
            format!("{}/{hex}.webp", &hex[..2]),
        )
    }

    /// 验证新库直接到达 v2，重复迁移不会清除已有短哈希文本。
    #[test]
    fn v2_migration_is_idempotent_and_preserves_rows() {
        let mut connection = Connection::open_in_memory().expect("创建内存数据库失败");
        assert_eq!(
            migrate(&mut connection).expect("首次迁移失败"),
            CURRENT_SCHEMA_VERSION
        );
        connection
            .execute(
                "INSERT INTO clipboard_items (item_type, preview_text, content_hash, created_at, copied_at) VALUES ('text', 'sentinel', X'01', 1, 1)",
                [],
            )
            .expect("插入哨兵记录失败");

        assert_eq!(
            migrate(&mut connection).expect("重复迁移失败"),
            CURRENT_SCHEMA_VERSION
        );
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM clipboard_items", [], |row| row.get(0))
            .expect("读取哨兵记录失败");
        assert_eq!(count, 1);
        assert_eq!(
            read_schema_version(&connection).expect("读取 schema 版本失败"),
            CURRENT_SCHEMA_VERSION
        );
    }

    /// 验证 v1 文本逐值保留，且已删除最高 ID 的 AUTOINCREMENT 高水位不会降低。
    #[test]
    fn v1_upgrade_preserves_text_and_deleted_id_high_watermark() {
        let mut connection = Connection::open_in_memory().expect("创建内存数据库失败");
        create_v1(&connection);
        connection
            .execute(
                "INSERT INTO clipboard_items \
                 (id, item_type, text_content, preview_text, content_hash, source_exe, source_app, \
                  copy_count, is_pinned, created_at, copied_at, last_used_at) \
                 VALUES (7, 'text', '完整正文', '短预览', X'01', 'old.exe', '旧应用', 4, 1, 10, 20, 30)",
                [],
            )
            .expect("插入 v1 文本失败");
        connection
            .execute(
                "INSERT INTO clipboard_items \
                 (id, item_type, preview_text, content_hash, created_at, copied_at) \
                 VALUES (100, 'text', '已删除高水位', X'02', 40, 40)",
                [],
            )
            .expect("插入高水位记录失败");
        connection
            .execute("DELETE FROM clipboard_items WHERE id = 100", [])
            .expect("删除高水位记录失败");

        assert_eq!(
            migrate(&mut connection).expect("v1 升级 v2 失败"),
            CURRENT_SCHEMA_VERSION
        );
        let text_row = connection
            .query_row(
                "SELECT id, item_type, text_content, preview_text, content_hash, source_exe, \
                        source_app, copy_count, is_pinned, created_at, copied_at, last_used_at \
                 FROM clipboard_items WHERE id = 7",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Vec<u8>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, i64>(8)?,
                        row.get::<_, i64>(9)?,
                        row.get::<_, i64>(10)?,
                        row.get::<_, Option<i64>>(11)?,
                    ))
                },
            )
            .expect("读取升级文本失败");
        assert_eq!(
            text_row,
            (
                7,
                "text".to_owned(),
                Some("完整正文".to_owned()),
                "短预览".to_owned(),
                vec![1],
                Some("old.exe".to_owned()),
                Some("旧应用".to_owned()),
                4,
                1,
                10,
                20,
                Some(30),
            )
        );
        let image_null_count: i64 = connection
            .query_row(
                "SELECT (image_root_id IS NULL) + (image_path IS NULL) + \
                        (thumbnail_path IS NULL) + (image_width IS NULL) + \
                        (image_height IS NULL) + (image_format IS NULL) + \
                        (content_size IS NULL) \
                 FROM clipboard_items WHERE id = 7",
                [],
                |row| row.get(0),
            )
            .expect("读取新增图片字段失败");
        assert_eq!(image_null_count, 7);
        connection
            .execute(
                "INSERT INTO clipboard_items \
                 (item_type, preview_text, content_hash, created_at, copied_at) \
                 VALUES ('text', '新记录', X'03', 50, 50)",
                [],
            )
            .expect("插入迁移后记录失败");
        assert_eq!(connection.last_insert_rowid(), 101);
    }

    /// 验证 v1 已发号但当前为空时仍恢复高水位，不复用已经删除的稳定 ID。
    #[test]
    fn v1_empty_table_preserves_deleted_id_high_watermark() {
        let mut connection = Connection::open_in_memory().expect("创建空表高水位数据库失败");
        create_v1(&connection);
        connection
            .execute(
                "INSERT INTO clipboard_items \
                 (id, item_type, preview_text, content_hash, created_at, copied_at) \
                 VALUES (100, 'text', '临时记录', X'01', 1, 1)",
                [],
            )
            .expect("插入空表高水位记录失败");
        connection
            .execute("DELETE FROM clipboard_items", [])
            .expect("清空 v1 高水位记录失败");

        migrate(&mut connection).expect("迁移空表高水位数据库失败");
        connection
            .execute(
                "INSERT INTO clipboard_items \
                 (item_type, preview_text, content_hash, created_at, copied_at) \
                 VALUES ('text', '迁移后记录', X'02', 2, 2)",
                [],
            )
            .expect("插入迁移后空表记录失败");
        assert_eq!(connection.last_insert_rowid(), 101);
    }

    /// 验证未来版本不会被当前程序悄悄降级或覆盖。
    #[test]
    fn future_schema_version_is_rejected() {
        let mut connection = Connection::open_in_memory().expect("创建内存数据库失败");
        connection
            .pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION + 1)
            .expect("写入未来 schema 版本失败");

        assert!(matches!(
            migrate(&mut connection),
            Err(crate::storage::StorageError::UnsupportedSchemaVersion(3))
        ));
    }

    /// 验证列声明不兼容时迁移失败且不会留下新建索引或错误版本。
    #[test]
    fn incompatible_schema_rolls_back_migration_changes() {
        let mut connection = Connection::open_in_memory().expect("创建内存数据库失败");
        connection
            .execute_batch(
                "CREATE TABLE clipboard_items (id INTEGER PRIMARY KEY, item_type TEXT NOT NULL, text_content TEXT, preview_text TEXT NOT NULL, content_hash TEXT NOT NULL, source_exe TEXT, source_app TEXT, copy_count INTEGER NOT NULL, is_pinned INTEGER NOT NULL, created_at INTEGER NOT NULL, copied_at INTEGER NOT NULL, last_used_at INTEGER);",
            )
            .expect("创建坏 schema 失败");

        assert!(matches!(
            migrate(&mut connection),
            Err(crate::storage::StorageError::IncompatibleSchema(_))
        ));
        assert_eq!(
            read_schema_version(&connection).expect("读取 schema 版本失败"),
            0
        );
        let index_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND tbl_name = 'clipboard_items'",
                [],
                |row| row.get(0),
            )
            .expect("读取索引数量失败");
        assert_eq!(index_count, 0);
    }

    /// 验证 partial 唯一索引不会被误判为全局 content_hash 唯一约束。
    #[test]
    fn partial_unique_index_is_rejected() {
        let mut connection = Connection::open_in_memory().expect("创建内存数据库失败");
        migrate(&mut connection).expect("创建基线 schema 失败");
        connection
            .execute_batch(
                "DROP INDEX idx_clipboard_items_content_hash; CREATE UNIQUE INDEX idx_clipboard_items_content_hash ON clipboard_items(content_hash) WHERE is_pinned = 1;",
            )
            .expect("创建 partial 索引失败");

        assert!(matches!(
            migrate(&mut connection),
            Err(crate::storage::StorageError::IncompatibleSchema(_))
        ));
    }

    /// 验证连接已启用外键，孤儿图片根和错误 SQLite storage class 均无法进入 v2。
    #[test]
    fn v2_rejects_orphan_roots_and_wrong_storage_classes() {
        let mut connection = Connection::open_in_memory().expect("创建内存数据库失败");
        migrate(&mut connection).expect("创建 v2 schema 失败");
        let foreign_keys: i64 = connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("读取外键状态失败");
        assert_eq!(foreign_keys, 1);

        let (hash, image_path, thumbnail_path) = image_paths(0x11);
        let orphan = connection.execute(
            "INSERT INTO clipboard_items \
             (item_type, preview_text, content_hash, created_at, copied_at, image_root_id, \
              image_path, thumbnail_path, image_width, image_height, image_format, content_size) \
             VALUES ('image', '图片', ?1, 1, 1, ?2, ?3, ?4, 10, 20, 'png', 30)",
            params![hash, vec![0x22_u8; 32], image_path, thumbnail_path],
        );
        assert!(orphan.is_err());

        let text_root = connection.execute(
            "INSERT INTO image_asset_roots (root_id, root_path, root_kind, created_at) \
             VALUES (?1, 'C:/images', 'custom', 1)",
            ["12345678901234567890123456789012"],
        );
        assert!(text_root.is_err());

        connection
            .execute(
                "INSERT INTO image_asset_roots (root_id, root_path, root_kind, created_at) \
                 VALUES (?1, 'C:/images', 'custom', 1)",
                params![vec![0x22_u8; 32]],
            )
            .expect("插入合法图片根失败");
        let (hash, image_path, thumbnail_path) = image_paths(0x33);
        let real_width = connection.execute(
            "INSERT INTO clipboard_items \
             (item_type, preview_text, content_hash, created_at, copied_at, image_root_id, \
              image_path, thumbnail_path, image_width, image_height, image_format, content_size) \
             VALUES ('image', '图片', ?1, 1, 1, ?2, ?3, ?4, 1.5, 20, 'png', 30)",
            params![hash, vec![0x22_u8; 32], image_path, thumbnail_path],
        );
        assert!(real_width.is_err());
    }

    /// 验证数据库内容哈希必须与原图和缩略图路径中的哈希完全一致。
    #[test]
    fn v2_rejects_image_path_hash_mismatch() {
        let mut connection = Connection::open_in_memory().expect("创建内存数据库失败");
        migrate(&mut connection).expect("创建 v2 schema 失败");
        connection
            .execute(
                "INSERT INTO image_asset_roots (root_id, root_path, root_kind, created_at) \
                 VALUES (?1, 'C:/images', 'custom', 1)",
                params![vec![0x44_u8; 32]],
            )
            .expect("插入合法图片根失败");
        let (wrong_hash, image_path, thumbnail_path) = image_paths(0x55);
        let result = connection.execute(
            "INSERT INTO clipboard_items \
             (item_type, preview_text, content_hash, created_at, copied_at, image_root_id, \
              image_path, thumbnail_path, image_width, image_height, image_format, content_size) \
             VALUES ('image', '错配图片', ?1, 1, 1, ?2, ?3, ?4, 10, 20, 'png', 30)",
            params![
                vec![0x66_u8; 32],
                vec![0x44_u8; 32],
                image_path,
                thumbnail_path
            ],
        );
        assert!(result.is_err());
        assert_eq!(wrong_hash, vec![0x55; 32]);
    }

    /// 验证 v1 伪图片无法升级，失败后版本、旧表和原记录全部保持不变。
    #[test]
    fn invalid_v1_image_row_rolls_back_v2_migration() {
        let mut connection = Connection::open_in_memory().expect("创建内存数据库失败");
        create_v1(&connection);
        connection
            .execute(
                "INSERT INTO clipboard_items \
                 (item_type, preview_text, content_hash, created_at, copied_at) \
                 VALUES ('image', '旧伪图片', X'01', 1, 1)",
                [],
            )
            .expect("插入 v1 伪图片失败");

        assert!(matches!(
            migrate(&mut connection),
            Err(crate::storage::StorageError::IncompatibleSchema(_))
        ));
        assert_eq!(
            read_schema_version(&connection).expect("读取回滚版本失败"),
            1
        );
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM clipboard_items", [], |row| row.get(0))
            .expect("读取回滚记录失败");
        assert_eq!(count, 1);
        let root_table_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'table' AND name = 'image_asset_roots'",
                [],
                |row| row.get(0),
            )
            .expect("检查回滚根表失败");
        assert_eq!(root_table_count, 0);
    }

    /// 验证任一关键 CHECK 被削弱后，v2 重开都会拒绝该 schema。
    #[test]
    fn weakened_v2_checks_are_rejected_on_reopen() {
        for removed_constraint in [
            "AND thumbnail_path IS NULL",
            "AND typeof(thumbnail_path) = 'text'",
            "AND typeof(image_height) = 'integer'",
            "AND image_format = 'png'",
            "AND typeof(content_size) = 'integer'",
            "CHECK (typeof(created_at) = 'integer')",
        ] {
            let weakened_sql = V2_SQL.replacen(removed_constraint, "", 1);
            assert_ne!(
                weakened_sql, V2_SQL,
                "测试约束片段不存在：{removed_constraint}"
            );
            let mut connection = Connection::open_in_memory().expect("创建削弱 schema 数据库失败");
            create_v1(&connection);
            connection
                .execute_batch(&weakened_sql)
                .expect("创建削弱 v2 schema 失败");
            connection
                .pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)
                .expect("标记削弱 v2 版本失败");

            assert!(
                matches!(
                    migrate(&mut connection),
                    Err(crate::storage::StorageError::IncompatibleSchema(_))
                ),
                "削弱约束未被拒绝：{removed_constraint}"
            );
        }

        for changed_sql in [
            V2_SQL.replacen("image_format = 'png'", "image_format = 'PNG'", 1),
            V2_SQL.replacen("image_format = 'png'", "image_format = 'p n g'", 1),
            V2_SQL.replacen(
                "root_kind IN ('default', 'custom')",
                "root_kind IN ('DEFAULT', 'CUSTOM')",
                1,
            ),
        ] {
            assert_ne!(changed_sql, V2_SQL, "字符串约束篡改测试没有改变 SQL");
            let mut connection = Connection::open_in_memory().expect("创建字符串篡改数据库失败");
            create_v1(&connection);
            connection
                .execute_batch(&changed_sql)
                .expect("创建字符串篡改 v2 schema 失败");
            connection
                .pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)
                .expect("标记字符串篡改 v2 版本失败");
            assert!(matches!(
                migrate(&mut connection),
                Err(crate::storage::StorageError::IncompatibleSchema(_))
            ));
        }
    }

    /// 验证额外显式唯一索引会改变合法写入，因此 v2 重开必须拒绝。
    #[test]
    fn extra_explicit_index_is_rejected_on_reopen() {
        let mut connection = Connection::open_in_memory().expect("创建额外索引数据库失败");
        migrate(&mut connection).expect("创建规范 v2 schema 失败");
        connection
            .execute_batch(
                "CREATE UNIQUE INDEX extra_preview_unique \
                 ON clipboard_items(preview_text);",
            )
            .expect("创建额外唯一索引失败");

        assert!(matches!(
            migrate(&mut connection),
            Err(crate::storage::StorageError::IncompatibleSchema(_))
        ));

        let mut root_connection = Connection::open_in_memory().expect("创建根表额外索引数据库失败");
        migrate(&mut root_connection).expect("创建根表规范 v2 schema 失败");
        root_connection
            .execute_batch(
                "CREATE UNIQUE INDEX extra_root_kind_unique \
                 ON image_asset_roots(root_kind);",
            )
            .expect("创建根表额外唯一索引失败");
        assert!(matches!(
            migrate(&mut root_connection),
            Err(crate::storage::StorageError::IncompatibleSchema(_))
        ));

        let mut extra_table_connection =
            Connection::open_in_memory().expect("创建第三表额外索引数据库失败");
        migrate(&mut extra_table_connection).expect("创建第三表规范 v2 schema 失败");
        extra_table_connection
            .execute_batch(
                "CREATE TABLE extra(value TEXT); \
                 CREATE INDEX extra_idx ON extra(value);",
            )
            .expect("创建第三表额外索引失败");
        assert!(matches!(
            migrate(&mut extra_table_connection),
            Err(crate::storage::StorageError::IncompatibleSchema(_))
        ));
    }
}
