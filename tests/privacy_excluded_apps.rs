//! ATOM-46 的排除程序边界测试；只使用拥有型来源和 RecordingGate，不访问真实剪贴板。

use clipboard_board::clipboard::ClipboardReadError;
use clipboard_board::platform::windows::{ProcessSource, ProcessSourceSnapshot};
use clipboard_board::privacy::{ExcludedAppsSnapshot, GateMode, RecordingGate};

/// 构造带可选完整映像路径的请求级来源快照。
fn source(executable: &str, image_path: Option<&str>) -> ProcessSourceSnapshot {
    ProcessSourceSnapshot {
        source: ProcessSource {
            executable: executable.to_owned(),
            display_name: "测试来源".to_owned(),
            process_id: 7,
        },
        image_path: image_path.map(str::to_owned),
    }
}

/// exe 规则必须大小写不敏感且精确匹配，不能把相似文件名当作命中。
#[test]
fn basename_rule_is_exact_and_case_insensitive() {
    let snapshot = ExcludedAppsSnapshot::from_rules(&["KeePass.exe".to_owned()]).unwrap();
    assert!(snapshot.matches(Some(&source("keepass.EXE", None))));
    assert!(!snapshot.matches(Some(&source("KeePassPortable.exe", None))));
    assert!(!snapshot.matches(None));
}

/// 完整 DOS/UNC 路径只在规范化后精确命中，不做目录前缀或文件系统解析。
#[test]
fn absolute_path_rule_normalizes_separators_but_not_prefixes() {
    let snapshot =
        ExcludedAppsSnapshot::from_rules(&[r"C:/Tools/KeePass/KeePass.exe".to_owned()]).unwrap();
    assert!(snapshot.matches(Some(&source(
        "KeePass.exe",
        Some(r"c:\Tools\KeePass\KeePass.exe"),
    ))));
    assert!(!snapshot.matches(Some(&source(
        "KeePass.exe",
        Some(r"C:\Tools\KeePass\KeePassPortable.exe"),
    ))));
    assert!(!snapshot.matches(Some(&source(
        "KeePass.exe",
        Some(r"C:\Tools\KeePass2\KeePass.exe"),
    ))));
}

/// 规则验证拒绝相对、根相对、越根、重复分隔符、扩展前缀和不完整 UNC。
#[test]
fn invalid_path_boundaries_are_rejected() {
    for rule in [
        "C:KeePass.exe",
        r".\KeePass.exe",
        r"..\KeePass.exe",
        r"\KeePass.exe",
        r"C:\..\KeePass.exe",
        r"C:\Tools\\KeePass.exe",
        r"\\?\C:\KeePass.exe",
        r"\\server\share",
    ] {
        assert!(
            ExcludedAppsSnapshot::from_rules(&[rule.to_owned()]).is_err(),
            "{rule}"
        );
    }
}

/// 快照自身也执行 64 条上限，不能绕过设置层校验构造无界规则集合。
#[test]
fn snapshot_rejects_more_than_sixty_four_rules() {
    let rules = (0..65)
        .map(|index| format!("app-{index}.exe"))
        .collect::<Vec<_>>();
    assert!(ExcludedAppsSnapshot::from_rules(&rules).is_err());
}

/// 暂停优先于排除；活动状态命中规则时才返回 ExcludedApp，未知来源保持放行。
#[test]
fn pause_has_priority_and_unknown_source_fails_open() {
    let excluded = ExcludedAppsSnapshot::from_rules(&["secret.exe".to_owned()]).unwrap();
    let paused = RecordingGate::new_with_excluded_apps(GateMode::Paused, excluded.clone());
    assert!(matches!(
        paused.try_read_for_snapshot(Some(&source("secret.exe", None))),
        Err(ClipboardReadError::Paused)
    ));

    let active = RecordingGate::new_with_excluded_apps(GateMode::Active, excluded);
    assert!(matches!(
        active.try_read_for_snapshot(Some(&source("SECRET.EXE", None))),
        Err(ClipboardReadError::ExcludedApp)
    ));
    let permit = active
        .try_read_for_snapshot(None)
        .expect("未知来源不得误伤普通捕获");
    drop(permit);
}

/// 规则替换与 reader 共用更新屏障，新请求只看替换后的不可变快照。
#[test]
fn replacing_rules_is_linearized_with_readers() {
    let old = ExcludedAppsSnapshot::from_rules(&["old.exe".to_owned()]).unwrap();
    let gate = RecordingGate::new_with_excluded_apps(GateMode::Active, old);
    let permit = gate
        .try_read_for_snapshot(Some(&source("other.exe", None)))
        .unwrap();
    let next = ExcludedAppsSnapshot::from_rules(&["new.exe".to_owned()]).unwrap();
    let clone = gate.clone();
    let handle = std::thread::spawn(move || clone.replace_excluded_apps(next));
    std::thread::sleep(std::time::Duration::from_millis(1));
    drop(permit);
    handle.join().unwrap();
    assert!(matches!(
        gate.try_read_for_snapshot(Some(&source("new.exe", None))),
        Err(ClipboardReadError::ExcludedApp)
    ));
    let old_permit = gate
        .try_read_for_snapshot(Some(&source("old.exe", None)))
        .unwrap();
    drop(old_permit);
}
