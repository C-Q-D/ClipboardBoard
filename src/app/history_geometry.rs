//! 历史列表的显式混合高度几何计算。
//!
//! 该模块不依赖 Slint、窗口句柄或剪贴板正文，只保存每行的稳定身份、像素高度和
//! 前缀和。UI 层通过它得到合法视口和有界窗口，从而绕开 ListView 对混合 delegate
//! 高度使用平均值的 compiler-magic 推算。

/// 单条历史摘要参与几何计算所需的最小元数据。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryGeometryItem {
    /// 数据库稳定 ID，用于窗口本地索引回到绝对身份。
    pub id: u64,
    /// 与 ID 共同组成稳定身份的内容哈希。
    pub content_hash: [u8; 32],
    /// 该摘要外层卡片的整数像素高度，必须为正数。
    pub height: i64,
}

/// 二分查找得到的窗口行，索引区间采用半开区间 `[start, end)`。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryWindowItem {
    /// 在完整数据集中的绝对行号。
    pub absolute_index: usize,
    /// 数据库稳定 ID。
    pub id: u64,
    /// 与 ID 共同组成稳定身份的内容哈希。
    pub content_hash: [u8; 32],
    /// 该行相对于内容画布顶部的整数像素位置。
    pub top: i64,
    /// 该行的整数像素高度。
    pub height: i64,
}

/// 一次视口计算的不可变结果；空数据集只返回 `[0, 0)`。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryWindow {
    /// 完整数据集窗口起点。
    pub start: usize,
    /// 完整数据集窗口结束点（不包含）。
    pub end: usize,
    /// 完整数据集条目数量，而不是窗口数量。
    pub total_count: usize,
    /// 所有行高度的精确整数总和。
    pub total_height: i64,
    /// 经过非负化后的可视区域高度。
    pub visible_height: i64,
    /// 经过 `max_offset` clamp 后的负向视口坐标。
    pub viewport_y: i64,
    /// 当前窗口的稳定行元数据。
    pub items: Vec<HistoryWindowItem>,
}

impl HistoryWindow {
    /// 返回窗口行数。
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// 空窗口判断；非空数据集不允许生成占位行。
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// 计算合法的最大滚动偏移量。
    pub fn max_offset(&self) -> i64 {
        self.total_height.saturating_sub(self.visible_height).max(0)
    }
}

/// 以整数像素保存每一行前缀和的几何快照。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryGeometry {
    items: Vec<HistoryGeometryItem>,
    /// `prefix[i]` 是第 `i` 行顶部，长度为 `items.len() + 1`。
    prefix: Vec<i64>,
}

impl HistoryGeometry {
    /// 构造前缀和；任何非正高度或 checked 加法溢出都会拒绝快照。
    pub fn new(items: Vec<HistoryGeometryItem>) -> Result<Self, GeometryError> {
        let mut prefix = Vec::with_capacity(items.len().saturating_add(1));
        prefix.push(0_i64);
        for item in &items {
            if item.height <= 0 {
                return Err(GeometryError::InvalidHeight);
            }
            let next = prefix
                .last()
                .copied()
                .ok_or(GeometryError::Overflow)?
                .checked_add(item.height)
                .ok_or(GeometryError::Overflow)?;
            prefix.push(next);
        }
        Ok(Self { items, prefix })
    }

    /// 返回完整元数据的只读切片，供 UI 生成 bounded 卡片模型。
    pub fn items(&self) -> &[HistoryGeometryItem] {
        &self.items
    }

    /// 返回前缀和的只读切片，最后一项即精确内容总高。
    pub fn prefix(&self) -> &[i64] {
        &self.prefix
    }

    /// 返回数据集条目数量。
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// 返回是否为空。
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 返回精确总高度。
    pub fn total_height(&self) -> i64 {
        self.prefix.last().copied().unwrap_or(0)
    }

    /// 返回 `max(total_height - visible_height, 0)`。
    pub fn max_offset(&self, visible_height: i64) -> i64 {
        self.total_height()
            .saturating_sub(visible_height.max(0))
            .max(0)
    }

    /// 将任意输入视口 clamp 到合法的负向坐标。
    pub fn clamp_viewport(&self, viewport_y: i64, visible_height: i64) -> i64 {
        let max_offset = self.max_offset(visible_height);
        viewport_y
            .saturating_neg()
            .clamp(0, max_offset)
            .saturating_neg()
    }

    /// 用 prefix-sum + 二分查找计算可见行及有限 overscan 窗口。
    ///
    /// 返回值永远不会超过 100 行；空数据集返回 `[0, 0)`，不会伪造占位项。
    pub fn window_for(
        &self,
        viewport_y: i64,
        visible_height: i64,
        overscan: usize,
    ) -> Result<HistoryWindow, GeometryError> {
        let visible_height = visible_height.max(0);
        let clamped_viewport_y = self.clamp_viewport(viewport_y, visible_height);
        let total_height = self.total_height();
        if self.items.is_empty() {
            return Ok(HistoryWindow {
                start: 0,
                end: 0,
                total_count: 0,
                total_height,
                visible_height,
                viewport_y: clamped_viewport_y,
                items: Vec::new(),
            });
        }

        let offset = clamped_viewport_y.saturating_neg();
        let viewport_end = offset
            .checked_add(visible_height)
            .ok_or(GeometryError::Overflow)?
            .min(total_height);
        // 半开区间规则：恰好命中行边界时从右侧新行开始，底部 total_height 特判最后一行。
        let visible_start = self.find_row_at(offset);
        let visible_end = if viewport_end >= total_height {
            self.items.len()
        } else {
            self.find_row_at(viewport_end).saturating_add(1)
        };
        let mut start = visible_start.saturating_sub(overscan);
        let mut end = visible_end.saturating_add(overscan).min(self.items.len());
        // 高度很小的条目可能让可见窗口超过上限，逐步收缩 overscan 而不是放宽上限。
        while end.saturating_sub(start) > 100 && (start < visible_start || end > visible_end) {
            if end > visible_end {
                end -= 1;
            }
            if end.saturating_sub(start) > 100 && start < visible_start {
                start += 1;
            }
        }
        if end <= start || end.saturating_sub(start) > 100 {
            return Err(GeometryError::WindowTooLarge);
        }

        let items = (start..end)
            .map(|absolute_index| HistoryWindowItem {
                absolute_index,
                id: self.items[absolute_index].id,
                content_hash: self.items[absolute_index].content_hash,
                top: self.prefix[absolute_index],
                height: self.items[absolute_index].height,
            })
            .collect();
        Ok(HistoryWindow {
            start,
            end,
            total_count: self.items.len(),
            total_height,
            visible_height,
            viewport_y: clamped_viewport_y,
            items,
        })
    }

    /// 返回包含像素位置的行；`total_height` 命中时返回最后一行。
    fn find_row_at(&self, position: i64) -> usize {
        let position = position.clamp(0, self.total_height());
        if position >= self.total_height() {
            return self.items.len().saturating_sub(1);
        }
        let mut low = 0;
        let mut high = self.items.len();
        while low < high {
            let middle = low + (high - low) / 2;
            if self.prefix[middle + 1] <= position {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low.min(self.items.len().saturating_sub(1))
    }
}

/// 几何源数据不满足协议时使用的固定错误。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GeometryError {
    /// 行高不是正数。
    InvalidHeight,
    /// 前缀和或坐标加法溢出。
    Overflow,
    /// 可见窗口在收缩 overscan 后仍然超过协议上限。
    WindowTooLarge,
}

#[cfg(test)]
mod tests {
    //! 纯几何窄测试，覆盖混合高度、边界、clamp 和窗口上限。

    use super::{GeometryError, HistoryGeometry, HistoryGeometryItem};

    fn item(id: u64, height: i64) -> HistoryGeometryItem {
        HistoryGeometryItem {
            id,
            content_hash: [id as u8; 32],
            height,
        }
    }

    #[test]
    fn 空数据集不生成占位窗口() {
        let geometry = HistoryGeometry::new(Vec::new()).unwrap();
        let window = geometry.window_for(0, 200, 10).unwrap();
        assert_eq!((window.start, window.end, window.total_height), (0, 0, 0));
        assert!(window.items.is_empty());
    }

    #[test]
    fn 混合前缀和与边界命中稳定() {
        let geometry =
            HistoryGeometry::new(vec![item(1, 106), item(2, 186), item(3, 106)]).unwrap();
        assert_eq!(geometry.prefix(), &[0, 106, 292, 398]);
        assert_eq!(geometry.total_height(), 398);
        assert_eq!(geometry.window_for(-106, 1, 0).unwrap().start, 1);
        assert_eq!(geometry.window_for(-398, 1, 0).unwrap().start, 2);
        assert_eq!(geometry.window_for(-398, 1, 0).unwrap().viewport_y, -397);
    }

    #[test]
    fn 两万条交错高度精确求和且窗口有界() {
        let geometry = HistoryGeometry::new(
            (0..20_000)
                .map(|index| item(index, if index % 2 == 0 { 106 } else { 186 }))
                .collect(),
        )
        .unwrap();
        assert_eq!(geometry.total_height(), 2_920_000);
        for viewport in [0, -1_460_000, -2_920_000] {
            let window = geometry.window_for(viewport, 640, 20).unwrap();
            assert!(!window.is_empty());
            assert!(window.len() <= 100);
        }
    }

    #[test]
    fn 非法高度与窗口超限拒绝() {
        assert_eq!(
            HistoryGeometry::new(vec![item(1, 0)]),
            Err(GeometryError::InvalidHeight)
        );
        let geometry =
            HistoryGeometry::new((0..101).map(|index| item(index, 1)).collect()).unwrap();
        assert_eq!(
            geometry.window_for(0, 101, 0),
            Err(GeometryError::WindowTooLarge)
        );
    }
}
