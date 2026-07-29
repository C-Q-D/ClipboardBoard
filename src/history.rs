//! 此模块提供内存历史协调器，集中处理文本记录的去重、置顶、计数和容量限制。
//!
//! 协调器只接收拥有型 UI 摘要，不读取剪贴板、不写 SQLite，也不触碰 Slint；捕获提交
//! 路径使用 `record_persisted` 直接接收数据库最终快照，旧 `record` 仅保留给非持久化
//! 测试或恢复前路径，避免 UI 在 SQLite 之外重新推导计数和身份。

use crate::command::UiClipboardItem;

/// UI 内存历史的默认容量；完整正文不在该模块内常驻。
pub const DEFAULT_MEMORY_HISTORY_CAPACITY: usize = 50;

/// 负责维护最近文本摘要顺序和重复记录合并规则的深模块。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryHistory {
    /// 只保存摘要、哈希和轻量元数据，按最近使用顺序排列。
    items: Vec<UiClipboardItem>,
    /// 达到上限后从尾部移除最旧记录；零表示显式不保留记录。
    capacity: usize,
}

impl MemoryHistory {
    /// 创建指定容量的内存历史；容量为零时所有记录都会被立即淘汰，不会产生隐式下限。
    pub fn new(capacity: usize) -> Self {
        Self {
            items: Vec::new(),
            capacity,
        }
    }

    /// 记录一次新的剪贴板摘要。
    ///
    /// 相同 `content_hash` 的记录只更新时间、增加饱和计数并移动到顶部；旧记录的
    /// 预览、来源、ID 和收藏状态保持不变，避免重复复制覆盖用户已有的整理信息。
    pub fn record(&mut self, mut incoming: UiClipboardItem) {
        if let Some(existing_index) = self
            .items
            .iter()
            .position(|item| item.content_hash == incoming.content_hash)
        {
            let mut existing = self.items.remove(existing_index);
            existing.relative_time = incoming.relative_time;
            existing.copy_count = existing.copy_count.saturating_add(1).max(1);
            self.items.insert(0, existing);
        } else {
            // 外部 DTO 可能来自恢复数据；零计数不符合“出现过一次”的历史语义。
            incoming.copy_count = incoming.copy_count.max(1);
            self.items.insert(0, incoming);
        }

        self.items.truncate(self.capacity);
    }

    /// 记录已经由 SQLite upsert 返回的最终快照，不重新推导重复计数或覆盖身份字段。
    ///
    /// 捕获提交路径必须使用此方法：数据库已经决定最终 ID、预览、来源、收藏和饱和
    /// 计数，UI 只负责把该快照置顶；DTO 转换层已经拒绝非法计数，这里不再改写它。
    pub fn record_persisted(&mut self, incoming: UiClipboardItem) {
        if let Some(existing_index) = self
            .items
            .iter()
            .position(|item| item.content_hash == incoming.content_hash)
        {
            self.items.remove(existing_index);
        }

        self.items.insert(0, incoming);
        self.items.truncate(self.capacity);
    }

    /// 用启动恢复或测试快照替换当前列表，并重新建立唯一哈希与非零计数不变量。
    pub fn replace(&mut self, items: Vec<UiClipboardItem>) {
        self.items.clear();
        if self.capacity == 0 {
            return;
        }

        for mut item in items {
            if self.items.len() >= self.capacity {
                break;
            }
            item.copy_count = item.copy_count.max(1);
            if self
                .items
                .iter()
                .any(|existing| existing.content_hash == item.content_hash)
            {
                continue;
            }
            self.items.push(item);
        }
    }

    /// 按稳定身份同步一条缓存记录的收藏状态；不在缓存内时保持无副作用。
    ///
    /// 缓存可能属于另一筛选集合，因此返回值只表示缓存是否命中，不能作为数据库
    /// 或当前可见快照是否成功更新的判据。
    pub fn set_pinned(&mut self, id: u64, content_hash: [u8; 32], is_pinned: bool) -> bool {
        let Some(item) = self
            .items
            .iter_mut()
            .find(|item| item.id == id && item.content_hash == content_hash)
        else {
            return false;
        };
        item.is_pinned = is_pinned;
        true
    }

    /// 按数据库 ID 和内容哈希删除一条缓存摘要；身份不匹配时保持无副作用。
    ///
    /// 返回值只表示当前缓存是否命中；数据库事务结果仍是删除成功的唯一判据。
    pub fn remove(&mut self, id: u64, content_hash: [u8; 32]) -> bool {
        let Some(index) = self
            .items
            .iter()
            .position(|item| item.id == id && item.content_hash == content_hash)
        else {
            return false;
        };
        self.items.remove(index);
        true
    }

    /// 以只读切片形式暴露当前顺序，调用方不能绕过协调器修改去重状态。
    pub fn items(&self) -> &[UiClipboardItem] {
        &self.items
    }
}

impl Default for MemoryHistory {
    /// 默认使用 UI 当前约定的 50 条摘要容量。
    fn default() -> Self {
        Self::new(DEFAULT_MEMORY_HISTORY_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    //! 此测试模块只通过 MemoryHistory 的公开接口验证可观察的排序和合并规则。

    use super::MemoryHistory;
    use crate::command::UiClipboardItem;

    /// 构造测试记录；哈希是显式输入，确保测试验证的是协调器而不是隐式文本解析。
    fn item(hash: u8, preview: &str, time: &str, pinned: bool) -> UiClipboardItem {
        UiClipboardItem {
            id: hash as u64,
            preview: preview.to_owned(),
            source: "测试来源".to_owned(),
            relative_time: time.to_owned(),
            content_hash: [hash; 32],
            copy_count: 1,
            is_pinned: pinned,
        }
    }

    /// 相同哈希只保留一条，并且更新时间、计数和顺序符合最新复制行为。
    #[test]
    fn 重复文本合并并置顶() {
        let mut history = MemoryHistory::new(10);
        history.record(item(1, "旧文本", "较早", false));
        history.record(item(2, "另一条", "中间", false));
        history.record(item(1, "重复文本的新摘要", "刚刚", false));

        let items = history.items();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].content_hash, [1; 32]);
        assert_eq!(items[0].preview, "旧文本");
        assert_eq!(items[0].relative_time, "刚刚");
        assert_eq!(items[0].copy_count, 2);
        assert_eq!(items[1].content_hash, [2; 32]);
    }

    /// 合并重复文本时，旧记录的收藏状态必须保留，不能被新 DTO 的默认值清掉。
    #[test]
    fn 重复文本保留收藏状态() {
        let mut history = MemoryHistory::new(10);
        history.record(item(7, "收藏内容", "之前", true));
        history.record(item(7, "来自新复制事件", "现在", false));

        let merged = &history.items()[0];
        assert!(merged.is_pinned);
        assert_eq!(merged.copy_count, 2);
        assert_eq!(merged.preview, "收藏内容");
    }

    /// 容量限制从尾部淘汰最旧记录，同时不影响顶部的去重结果。
    #[test]
    fn 超过容量淘汰最旧记录() {
        let mut history = MemoryHistory::new(2);
        history.record(item(1, "一", "一", false));
        history.record(item(2, "二", "二", false));
        history.record(item(3, "三", "三", false));

        assert_eq!(
            history
                .items()
                .iter()
                .map(|item| item.content_hash[0])
                .collect::<Vec<_>>(),
            vec![3, 2]
        );
    }

    /// 恢复快照也必须遵守容量上限，但不在恢复边界重复执行复制计数。
    #[test]
    fn 替换快照遵守容量上限() {
        let mut history = MemoryHistory::new(1);
        history.replace(vec![item(1, "旧", "一", false), item(2, "新", "二", false)]);

        assert_eq!(history.items().len(), 1);
        assert_eq!(history.items()[0].content_hash, [1; 32]);
    }

    /// 恢复快照中的重复哈希只保留第一条，并把异常的零计数修正为一次。
    #[test]
    fn 替换快照修正重复和零计数() {
        let mut history = MemoryHistory::new(3);
        let mut zero_count = item(4, "第一条", "较早", false);
        zero_count.copy_count = 0;
        history.replace(vec![zero_count, item(4, "重复条目", "后来", false)]);

        assert_eq!(history.items().len(), 1);
        assert_eq!(history.items()[0].preview, "第一条");
        assert_eq!(history.items()[0].copy_count, 1);
    }

    /// 零容量配置必须稳定地产生空历史，不能因为插入路径而保留一条隐藏记录。
    #[test]
    fn 零容量不保留记录() {
        let mut history = MemoryHistory::new(0);
        history.record(item(3, "不会保留", "刚刚", false));
        assert!(history.items().is_empty());
    }

    /// 计数到达上限后继续复制必须饱和，不能回绕为零或较小数值。
    #[test]
    fn 复制计数饱和不回绕() {
        let mut history = MemoryHistory::new(2);
        let mut max_count = item(8, "极限", "之前", false);
        max_count.copy_count = u64::MAX;
        history.record(max_count);
        history.record(item(8, "新摘要", "刚刚", false));

        assert_eq!(history.items()[0].copy_count, u64::MAX);
    }

    /// 持久化捕获重复项必须完整采用数据库最终快照，不能把旧卡片本地加一或保留旧身份。
    #[test]
    fn 持久化重复项采用数据库最终快照() {
        let mut history = MemoryHistory::new(10);
        history.record(item(12, "旧预览", "之前", false));

        let mut persisted = item(12, "数据库最终预览", "刚刚", true);
        persisted.id = 88;
        persisted.source = "数据库来源".to_owned();
        persisted.copy_count = u64::MAX;
        history.record_persisted(persisted);

        let actual = &history.items()[0];
        assert_eq!(actual.id, 88);
        assert_eq!(actual.preview, "数据库最终预览");
        assert_eq!(actual.source, "数据库来源");
        assert_eq!(actual.relative_time, "刚刚");
        assert_eq!(actual.copy_count, u64::MAX);
        assert!(actual.is_pinned);
    }

    /// 收藏同步必须同时校验 ID 和哈希，且缓存未命中时不能误改其他记录。
    #[test]
    fn 按稳定身份同步收藏状态() {
        let mut history = MemoryHistory::new(10);
        history.record(item(21, "目标", "刚刚", false));
        assert!(!history.set_pinned(99, [21; 32], true));
        assert!(!history.set_pinned(21, [99; 32], true));
        assert!(history.set_pinned(21, [21; 32], true));
        assert!(history.items()[0].is_pinned);
    }

    /// 删除缓存摘要必须同时校验 ID 和哈希，错误身份不能移除任何其他记录。
    #[test]
    fn 按稳定身份删除缓存摘要() {
        let mut history = MemoryHistory::new(10);
        history.record(item(31, "目标", "刚刚", false));
        history.record(item(32, "保留", "刚刚", false));

        assert!(!history.remove(99, [31; 32]));
        assert!(!history.remove(31, [99; 32]));
        assert!(history.remove(31, [31; 32]));
        assert_eq!(history.items().len(), 1);
        assert_eq!(history.items()[0].id, 32);
    }
}
