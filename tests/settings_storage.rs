//! 此集成测试通过公开 settings 接口验证配置工作线程的持久化、恢复与并发契约。
//!
//! 所有文件都位于 ATOM-44 独占临时根；测试不读取默认 LOCALAPPDATA，也不启动真实应用。

use std::{
    fs,
    path::PathBuf,
    process,
    sync::{
        atomic::{AtomicU64, Ordering},
        OnceLock,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use clipboard_board::settings::{
    AppSettings, HistorySettings, SettingsError, SettingsLoadSource, SettingsWorker,
};

/// 临时根创建尝试上限，避免异常碰撞时无限循环。
const TEMPORARY_DIRECTORY_ATTEMPTS: usize = 64;

/// 同一进程内为临时根分配单调 token。
static NEXT_TEMPORARY_TOKEN: AtomicU64 = AtomicU64::new(0);

/// 返回当前测试进程固定 nonce，和 PID/token 一起隔离并发测试。
fn process_instance_nonce() -> u128 {
    static NONCE: OnceLock<u128> = OnceLock::new();
    *NONCE.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos())
            ^ u128::from(process::id())
    })
}

/// 使用 create_dir 独占创建 ATOM-44 测试根；碰撞时只在固定上限内重试。
fn temporary_directory(label: &str) -> PathBuf {
    temporary_directory_with(label, || {
        NEXT_TEMPORARY_TOKEN.fetch_add(1, Ordering::Relaxed)
    })
}

/// 使用可注入 token 源创建隔离根，供碰撞上限测试复用真实算法。
fn temporary_directory_with(label: &str, mut next_token: impl FnMut() -> u64) -> PathBuf {
    for _ in 0..TEMPORARY_DIRECTORY_ATTEMPTS {
        let token = next_token();
        let directory = std::env::temp_dir().join(format!(
            "clipboard-board-atom44-settings-core-{:032x}-{}-{token}-{label}",
            process_instance_nonce(),
            process::id()
        ));
        match fs::create_dir(&directory) {
            Ok(()) => return directory,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("创建 ATOM-44 隔离临时根失败：{error}"),
        }
    }
    panic!("ATOM-44 隔离临时根在有界重试内持续碰撞");
}

/// 显式关闭 worker 并清理测试根，避免后台线程跨测试存活。
fn shutdown_and_remove(mut worker: SettingsWorker, directory: PathBuf) {
    worker.begin_closing().expect("建立配置关闭线性化点失败");
    worker.finish_shutdown().expect("回收配置工作线程失败");
    fs::remove_dir_all(directory).expect("清理 ATOM-44 临时根失败");
}

/// 构造语义合法且容易辨识的非默认配置。
fn settings_with_max_items(max_items: u32) -> AppSettings {
    AppSettings {
        history: HistorySettings {
            max_items,
            ..HistorySettings::default()
        },
        ..AppSettings::default()
    }
}

/// 显式关闭 worker，但保留目录供同一用例重启。
fn shutdown_without_remove(mut worker: SettingsWorker) {
    worker.begin_closing().expect("建立配置关闭线性化点失败");
    worker.finish_shutdown().expect("回收配置工作线程失败");
}

/// 首次启动缺少主文件和备份时必须返回根计划中的默认配置。
#[test]
fn missing_configuration_returns_defaults_without_using_local_app_data() {
    let directory = temporary_directory("missing");
    let worker = SettingsWorker::start_at(&directory).expect("从显式临时根启动配置线程失败");
    let snapshot = worker.client().snapshot().expect("读取默认配置快照失败");

    assert_eq!(snapshot.source(), SettingsLoadSource::Defaults);
    assert_eq!(snapshot.revision(), 0);
    assert_eq!(snapshot.settings(), &AppSettings::default());
    assert!(!directory.join("settings.json").exists());
    assert!(!directory.join("settings.json.bak").exists());

    shutdown_and_remove(worker, directory);
}

/// version 1 主文件允许缺少已知字段，并在保存后保留顶层和 history 未知值。
#[test]
fn version_one_preserves_unknown_fields_across_save() {
    let directory = temporary_directory("unknown");
    fs::write(
        directory.join("settings.json"),
        r#"{
            "schema_version": 1,
            "future_top": {"nested": [1, true, "保留"]},
            "history": {
                "max_items": 3210,
                "future_history": {"mode": "future"}
            }
        }"#,
    )
    .expect("写入未知字段夹具失败");
    let worker = SettingsWorker::start_at(&directory).expect("加载 version 1 配置失败");
    let client = worker.client();
    let loaded = client.snapshot().expect("读取已加载快照失败");
    assert_eq!(loaded.source(), SettingsLoadSource::Primary);
    assert_eq!(loaded.settings().history.max_items, 3_210);
    assert_eq!(loaded.settings().history.retention_days, 30);

    let saved = client
        .save(loaded.revision(), settings_with_max_items(4_000))
        .expect("保存已知字段失败");
    assert_eq!(saved.revision(), 1);
    assert_eq!(saved.source(), SettingsLoadSource::Primary);
    let document: serde_json::Value = serde_json::from_slice(
        &fs::read(directory.join("settings.json")).expect("读取保存后的主文件失败"),
    )
    .expect("解析保存后的主文件失败");
    assert_eq!(document["future_top"]["nested"][2], "保留");
    assert_eq!(document["history"]["future_history"]["mode"], "future");
    assert_eq!(document["history"]["max_items"], 4_000);

    shutdown_and_remove(worker, directory);
}

/// 未来 schema 主文件必须阻止旧版本回退到可写备份。
#[test]
fn future_primary_schema_does_not_fall_back_to_version_one_backup() {
    let directory = temporary_directory("future-schema");
    let primary = br#"{"schema_version":2,"future":"must-preserve"}"#;
    let backup = br#"{"schema_version":1,"history":{"max_items":1234}}"#;
    fs::write(directory.join("settings.json"), primary).expect("写入未来主文件失败");
    fs::write(directory.join("settings.json.bak"), backup).expect("写入旧备份失败");

    let result = SettingsWorker::start_at(&directory);
    assert!(matches!(result, Err(SettingsError::UnsupportedSchema(2))));
    assert_eq!(fs::read(directory.join("settings.json")).unwrap(), primary);
    assert_eq!(
        fs::read(directory.join("settings.json.bak")).unwrap(),
        backup
    );
    fs::remove_dir_all(directory).expect("清理未来 schema 临时根失败");
}

/// 两个客户端使用同一旧 revision 时，只有首个保存可以提交。
#[test]
fn stale_client_cannot_overwrite_newer_settings() {
    let directory = temporary_directory("stale-write");
    let worker = SettingsWorker::start_at(&directory).expect("启动配置线程失败");
    let first = worker.client();
    let second = first.clone();
    let initial = first.snapshot().expect("读取初始快照失败");

    let committed = first
        .save(initial.revision(), settings_with_max_items(2_222))
        .expect("首个客户端保存失败");
    assert_eq!(committed.revision(), 1);
    assert!(matches!(
        second.save(initial.revision(), settings_with_max_items(3_333)),
        Err(SettingsError::RevisionConflict {
            expected: 0,
            actual: 1
        })
    ));
    assert_eq!(
        second.snapshot().unwrap().settings().history.max_items,
        2_222
    );

    shutdown_and_remove(worker, directory);
}

/// 保存会把上一份已验证主文件轮换为备份。
#[test]
fn successive_saves_rotate_last_valid_primary_to_backup() {
    let directory = temporary_directory("backup-rotation");
    let worker = SettingsWorker::start_at(&directory).expect("启动配置线程失败");
    let client = worker.client();
    let first = client
        .save(0, settings_with_max_items(1_111))
        .expect("保存 A 失败");
    client
        .save(first.revision(), settings_with_max_items(2_222))
        .expect("保存 B 失败");

    let primary: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join("settings.json")).unwrap()).unwrap();
    let backup: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join("settings.json.bak")).unwrap()).unwrap();
    assert_eq!(primary["history"]["max_items"], 2_222);
    assert_eq!(backup["history"]["max_items"], 1_111);

    shutdown_and_remove(worker, directory);
}

/// 保存、显式关闭并从同一目录重启后，全部已知值必须一致。
#[test]
fn saved_settings_survive_explicit_shutdown_and_restart() {
    let directory = temporary_directory("save-restart");
    let worker = SettingsWorker::start_at(&directory).unwrap();
    let expected = AppSettings {
        history: HistorySettings {
            max_items: 12_345,
            retention_days: 365,
            image_quota_mib: 2_048,
            capture_images: false,
            capture_source_app: false,
            image_storage_root: None,
        },
        ..AppSettings::default()
    };
    let saved = worker.client().save(0, expected.clone()).unwrap();
    assert_eq!(saved.revision(), 1);
    assert_eq!(saved.source(), SettingsLoadSource::Primary);
    shutdown_without_remove(worker);

    let restarted = SettingsWorker::start_at(&directory).unwrap();
    let snapshot = restarted.client().snapshot().unwrap();
    assert_eq!(snapshot.settings(), &expected);
    assert_eq!(snapshot.source(), SettingsLoadSource::Primary);
    assert_eq!(snapshot.revision(), 0);
    shutdown_and_remove(restarted, directory);
}

/// 自定义图片根属于已知字段：保存后 JSON 与重启快照必须保留同一字符串值。
#[test]
fn image_storage_root_survives_save_and_restart() {
    let directory = temporary_directory("image-root-restart");
    let custom_root = directory.join("images");
    let custom_root = custom_root
        .to_str()
        .expect("Windows 临时路径必须可转换为 UTF-8")
        .to_owned();
    let worker = SettingsWorker::start_at(&directory).expect("启动配置线程失败");
    let mut expected = worker.client().snapshot().unwrap().settings().clone();
    expected.history.image_storage_root = Some(custom_root.clone());

    let saved = worker
        .client()
        .save(0, expected.clone())
        .expect("保存图片根失败");
    assert_eq!(
        saved.settings().history.image_storage_root.as_deref(),
        Some(custom_root.as_str())
    );
    let document: serde_json::Value = serde_json::from_slice(
        &fs::read(directory.join("settings.json")).expect("读取图片根配置失败"),
    )
    .expect("解析图片根配置失败");
    assert_eq!(document["history"]["image_storage_root"], custom_root);
    shutdown_without_remove(worker);

    let restarted = SettingsWorker::start_at(&directory).expect("重启配置线程失败");
    assert_eq!(
        restarted
            .client()
            .snapshot()
            .unwrap()
            .settings()
            .history
            .image_storage_root
            .as_deref(),
        Some(custom_root.as_str())
    );
    shutdown_and_remove(restarted, directory);
}

/// 图片根非法值必须在 staging 创建前被统一字段错误拒绝，不能先落盘再启动回退。
#[test]
fn invalid_image_storage_root_is_rejected_before_disk_write() {
    for (label, value) in [
        ("relative", "relative\\images"),
        ("control", "D:\\Images\n"),
    ] {
        let directory = temporary_directory(&format!("image-root-invalid-{label}"));
        let worker = SettingsWorker::start_at(&directory).expect("启动配置线程失败");
        let mut invalid = worker.client().snapshot().unwrap().settings().clone();
        invalid.history.image_storage_root = Some(value.to_owned());
        assert!(matches!(
            worker.client().save(0, invalid),
            Err(SettingsError::InvalidSettings("history.image_storage_root"))
        ));
        assert!(!directory.join("settings.json").exists());
        shutdown_and_remove(worker, directory);
    }
}

/// 从备份恢复后保存不得用损坏主文件覆盖唯一有效恢复点。
#[test]
fn saving_after_backup_recovery_preserves_backup_recovery_point() {
    let directory = temporary_directory("backup-preservation");
    fs::write(directory.join("settings.json"), b"{broken").expect("写入损坏主文件失败");
    fs::write(
        directory.join("settings.json.bak"),
        br#"{"schema_version":1,"history":{"max_items":1111}}"#,
    )
    .expect("写入备份 A 失败");
    let mut worker = SettingsWorker::start_at(&directory).expect("从备份 A 恢复失败");
    let client = worker.client();
    let recovered = client.snapshot().expect("读取备份快照失败");
    assert_eq!(recovered.source(), SettingsLoadSource::Backup);
    client
        .save(recovered.revision(), settings_with_max_items(2_222))
        .expect("保存 B 失败");
    worker.begin_closing().expect("关闭首个配置 worker 失败");
    worker.finish_shutdown().expect("回收首个配置 worker 失败");
    fs::write(directory.join("settings.json"), b"{broken-b").expect("损坏新主文件 B 失败");

    let recovered_again =
        SettingsWorker::start_at(&directory).expect("损坏 B 后未能再次从备份 A 恢复");
    let snapshot = recovered_again.client().snapshot().unwrap();
    assert_eq!(snapshot.source(), SettingsLoadSource::Backup);
    assert_eq!(snapshot.settings().history.max_items, 1_111);
    shutdown_and_remove(recovered_again, directory);
}

/// 语义非法配置必须在创建 staging 之前被拒绝。
#[test]
fn invalid_settings_are_rejected_before_disk_write() {
    let directory = temporary_directory("invalid-save");
    let worker = SettingsWorker::start_at(&directory).expect("启动配置线程失败");
    let client = worker.client();
    let invalid = settings_with_max_items(0);
    assert!(matches!(
        client.save(0, invalid),
        Err(SettingsError::InvalidSettings("history.max_items"))
    ));
    assert_eq!(client.snapshot().unwrap().revision(), 0);
    assert!(!directory.join("settings.json").exists());
    assert_eq!(
        fs::read_dir(&directory).unwrap().count(),
        0,
        "非法保存不得遗留 staging"
    );

    shutdown_and_remove(worker, directory);
}

/// 主文件和备份都损坏时必须保留证据并拒绝启动。
#[test]
fn corrupt_primary_and_backup_are_not_overwritten() {
    let directory = temporary_directory("double-corrupt");
    let primary = b"{broken-primary";
    let backup = b"{broken-backup";
    fs::write(directory.join("settings.json"), primary).unwrap();
    fs::write(directory.join("settings.json.bak"), backup).unwrap();
    assert!(matches!(
        SettingsWorker::start_at(&directory),
        Err(SettingsError::UnrecoverableConfiguration)
    ));
    assert_eq!(fs::read(directory.join("settings.json")).unwrap(), primary);
    assert_eq!(
        fs::read(directory.join("settings.json.bak")).unwrap(),
        backup
    );
    fs::remove_dir_all(directory).expect("清理双损坏临时根失败");
}

/// load 与 save 共用的数值规则必须拒绝错误 JSON 类型和越界值。
#[test]
fn load_rejects_invalid_known_numeric_values() {
    let fields = [
        ("max_items", "1", "100000", "100001"),
        ("retention_days", "1", "3650", "3651"),
        ("image_quota_mib", "16", "10240", "10241"),
    ];
    for (field, minimum, maximum, above_maximum) in fields {
        for (kind, value) in [
            ("zero", "0"),
            ("above", above_maximum),
            ("negative", "-1"),
            ("fraction", "1.5"),
            ("string", r#""1""#),
            ("null", "null"),
        ] {
            let label = format!("{field}-{kind}");
            let directory = temporary_directory(&label);
            fs::write(
                directory.join("settings.json"),
                format!(r#"{{"schema_version":1,"history":{{"{field}":{value}}}}}"#),
            )
            .expect("写入非法数值夹具失败");
            assert!(
                matches!(
                    SettingsWorker::start_at(&directory),
                    Err(SettingsError::UnrecoverableConfiguration)
                ),
                "非法夹具未被拒绝：{label}"
            );
            fs::remove_dir_all(directory).expect("清理非法数值临时根失败");
        }

        for (kind, value) in [("minimum", minimum), ("maximum", maximum)] {
            let label = format!("{field}-{kind}");
            let directory = temporary_directory(&label);
            fs::write(
                directory.join("settings.json"),
                format!(r#"{{"schema_version":1,"history":{{"{field}":{value}}}}}"#),
            )
            .unwrap();
            let worker = SettingsWorker::start_at(&directory).expect("合法数值边界必须可以加载");
            shutdown_and_remove(worker, directory);
        }
    }
}

/// save 对三个字段的无符号可表达越界值都在文件 IO 前拒绝。
#[test]
fn save_rejects_all_numeric_out_of_range_values() {
    for (label, settings, field) in [
        (
            "save-max-zero",
            AppSettings {
                history: HistorySettings {
                    max_items: 0,
                    ..HistorySettings::default()
                },
                ..AppSettings::default()
            },
            "history.max_items",
        ),
        (
            "save-max-high",
            AppSettings {
                history: HistorySettings {
                    max_items: 100_001,
                    ..HistorySettings::default()
                },
                ..AppSettings::default()
            },
            "history.max_items",
        ),
        (
            "save-days-zero",
            AppSettings {
                history: HistorySettings {
                    retention_days: 0,
                    ..HistorySettings::default()
                },
                ..AppSettings::default()
            },
            "history.retention_days",
        ),
        (
            "save-days-high",
            AppSettings {
                history: HistorySettings {
                    retention_days: 3_651,
                    ..HistorySettings::default()
                },
                ..AppSettings::default()
            },
            "history.retention_days",
        ),
        (
            "save-quota-low",
            AppSettings {
                history: HistorySettings {
                    image_quota_mib: 15,
                    ..HistorySettings::default()
                },
                ..AppSettings::default()
            },
            "history.image_quota_mib",
        ),
        (
            "save-quota-high",
            AppSettings {
                history: HistorySettings {
                    image_quota_mib: 10_241,
                    ..HistorySettings::default()
                },
                ..AppSettings::default()
            },
            "history.image_quota_mib",
        ),
    ] {
        let directory = temporary_directory(label);
        let worker = SettingsWorker::start_at(&directory).unwrap();
        assert!(matches!(
            worker.client().save(0, settings),
            Err(SettingsError::InvalidSettings(actual)) if actual == field
        ));
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
        shutdown_and_remove(worker, directory);
    }
}

/// 缺失 schema_version 的旧配置按 version 1 读取。
#[test]
fn missing_schema_version_is_compatible_with_version_one() {
    let directory = temporary_directory("missing-schema");
    fs::write(
        directory.join("settings.json"),
        br#"{"history":{"max_items":4321}}"#,
    )
    .expect("写入无 schema 夹具失败");
    let worker = SettingsWorker::start_at(&directory).expect("无 schema 配置未按 version 1 加载");
    let snapshot = worker.client().snapshot().unwrap();
    assert_eq!(snapshot.source(), SettingsLoadSource::Primary);
    assert_eq!(snapshot.settings().history.max_items, 4_321);
    shutdown_and_remove(worker, directory);
}

/// schema_version 的非当前整数类型均按损坏处理，不回退为默认配置。
#[test]
fn invalid_schema_values_are_rejected() {
    for (label, value) in [
        ("schema-zero", "0"),
        ("schema-negative", "-1"),
        ("schema-fraction", "1.5"),
        ("schema-string", r#""1""#),
        ("schema-null", "null"),
    ] {
        let directory = temporary_directory(label);
        fs::write(
            directory.join("settings.json"),
            format!(r#"{{"schema_version":{value},"history":{{}}}}"#),
        )
        .unwrap();
        assert!(matches!(
            SettingsWorker::start_at(&directory),
            Err(SettingsError::UnrecoverableConfiguration)
        ));
        fs::remove_dir_all(directory).unwrap();
    }
}

/// 临时根生成器必须用 create_dir 独占产生不同路径。
#[test]
fn temporary_roots_are_unique_and_atom_scoped() {
    let first = temporary_directory("unique");
    let second = temporary_directory("unique");
    assert_ne!(first, second);
    for directory in [&first, &second] {
        assert!(directory
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("clipboard-board-atom44-settings-core-"));
    }
    fs::remove_dir_all(first).expect("清理首个唯一临时根失败");
    fs::remove_dir_all(second).expect("清理第二个唯一临时根失败");
}

/// 临时根持续碰撞时必须在固定次数后停止，且不删除既有目录。
#[test]
fn temporary_root_collision_has_bounded_retries() {
    let label = "bounded-collision";
    let token = u64::MAX;
    let collision = std::env::temp_dir().join(format!(
        "clipboard-board-atom44-settings-core-{:032x}-{}-{token}-{label}",
        process_instance_nonce(),
        process::id()
    ));
    if !collision.exists() {
        fs::create_dir(&collision).unwrap();
    }
    let mut attempts = 0;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = temporary_directory_with(label, || {
            attempts += 1;
            token
        });
    }));
    assert!(result.is_err());
    assert_eq!(attempts, TEMPORARY_DIRECTORY_ATTEMPTS);
    assert!(collision.exists());
    fs::remove_dir(&collision).unwrap();
}
