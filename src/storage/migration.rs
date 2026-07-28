//! 此模块封装 SQLite v1 schema 的事务迁移、结构校验和版本读取。
//!
//! 迁移只接受明确的 v1 表结构；发现坏 schema 或未来版本时必须在提交前返回错误。

use rusqlite::Connection;

use super::StorageError;

/// 当前代码能够读取和验证的最高 schema 版本。
pub(crate) const CURRENT_SCHEMA_VERSION: i64 = 1;

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

/// SQLite `PRAGMA table_info` 返回的字段契约，集中表达名称、类型、约束和默认值。
type ColumnDefinition = (String, String, i64, i64, Option<String>);

/// 在一个事务内完成版本 0 到 v1 的创建，或对 v1 数据库做只读结构复核。
pub(crate) fn migrate(connection: &mut Connection) -> Result<i64, StorageError> {
    let version = read_schema_version(connection)?;

    if version > CURRENT_SCHEMA_VERSION {
        return Err(StorageError::UnsupportedSchemaVersion(version));
    }

    let transaction = connection.transaction()?;

    if version == 0 {
        transaction.execute_batch(V1_SQL)?;
    }

    validate_v1_schema(&transaction)?;

    if version == 0 {
        transaction.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)?;
    }

    transaction.commit()?;
    Ok(CURRENT_SCHEMA_VERSION)
}

/// 读取 SQLite 内置的 schema 版本载体；该函数只在存储线程内被调用。
pub(crate) fn read_schema_version(connection: &Connection) -> Result<i64, StorageError> {
    Ok(connection.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

/// 校验 v1 的表、字段约束和两个稳定查询索引，避免把不兼容库误判为幂等。
fn validate_v1_schema(connection: &Connection) -> Result<(), StorageError> {
    let actual_columns = read_columns(connection)?;
    let expected_columns = V1_COLUMNS
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
        .collect::<Vec<_>>();

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

/// 读取表字段名、声明类型、NOT NULL、主键和默认值，忽略 SQLite 自动生成的 cid。
fn read_columns(connection: &Connection) -> Result<Vec<ColumnDefinition>, StorageError> {
    let mut statement = connection.prepare("PRAGMA table_info(clipboard_items)")?;
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

#[cfg(test)]
mod tests {
    //! 此测试模块验证 v1 迁移的幂等、坏 schema 拒绝和版本上限行为。

    use rusqlite::Connection;

    use super::{migrate, read_schema_version, CURRENT_SCHEMA_VERSION};

    /// 验证重复迁移不会清除已有哨兵记录，并始终保持 v1 版本。
    #[test]
    fn v1_migration_is_idempotent_and_preserves_rows() {
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
            1
        );
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
            Err(crate::storage::StorageError::UnsupportedSchemaVersion(2))
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
}
