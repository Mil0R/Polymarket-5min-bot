//! Viewer-local language selection. It never changes journal or engine state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Language {
    #[default]
    English,
    Chinese,
}

impl Language {
    pub fn toggle(&mut self) {
        *self = match self {
            Self::English => Self::Chinese,
            Self::Chinese => Self::English,
        };
    }

    pub fn text<'a>(self, english: &'a str, chinese: &'a str) -> &'a str {
        match self {
            Self::English => english,
            Self::Chinese => chinese,
        }
    }

    // Older engines and the read-only ledger return Chinese diagnostics. Translate
    // their known messages at the display boundary, preserving raw audit records.
    pub fn diagnostic(self, value: &str) -> String {
        if self == Self::Chinese {
            return value.into();
        }
        let mut result = value.to_owned();
        for (zh, en) in [
            (
                "请输入有效日期 YYYY-MM-DD，不能晚于 UTC 今天",
                "Enter a valid YYYY-MM-DD date, no later than today in UTC",
            ),
            (
                "只读账本快照；无实时连接",
                "Read-only ledger snapshot; no live connection",
            ),
            (
                "账本验证失败（停止累计）",
                "Ledger validation failed (updates stopped)",
            ),
            (
                "只支持price-cap v1研究账本",
                "Only price-cap v1 study journals are supported",
            ),
            (
                "不是只读PAPER研究账本",
                "Not a read-only PAPER study journal",
            ),
            (
                "注册重复或不在文件首行",
                "Duplicate registration or registration is not first",
            ),
            ("单条记录超过2MiB", "Record exceeds 2 MiB"),
            (
                "未连接策略快照服务",
                "Cannot connect to engine snapshot service",
            ),
            (
                "快照批次与账本不匹配",
                "Snapshot batch does not match journal",
            ),
            ("实时TUI需要Unix socket", "Live TUI requires a Unix socket"),
            ("快照不完整", "Incomplete snapshot"),
            ("不支持的研究轨道", "Unsupported study arm"),
            ("不支持的资产", "Unsupported asset"),
            ("无法读取", "Cannot read"),
            ("不支持的资金参数", "Unsupported capital settings"),
            ("轨道定义不匹配", "Arm definition mismatch"),
            ("缺少注册记录", "Missing registration"),
            ("重复run ID", "Duplicate run ID"),
            (
                "未注册run的观察",
                "Observation references an unregistered run",
            ),
            ("观察轨道不完整", "Incomplete observation arms"),
            ("无效研究尝试范围", "Invalid study attempt scope"),
            ("重复尝试", "Duplicate attempt"),
            ("无效资金预留", "Invalid capital reservation"),
            ("尝试超出报价范围", "Attempt outside quote range"),
            ("模拟现金不足", "Insufficient PAPER cash"),
            ("执行缺少尝试", "Execution has no attempt"),
            ("重复执行", "Duplicate execution"),
            ("执行关联错误", "Execution link mismatch"),
            ("执行金额或时序错误", "Invalid execution amounts or timing"),
            ("无效成交", "Invalid fill"),
            ("未成交存在敞口", "Unfilled execution has exposure"),
            ("未知执行状态", "Unknown execution outcome"),
            ("结算缺少尝试", "Settlement has no attempt"),
            (
                "结算重复或缺少执行",
                "Duplicate settlement or missing execution",
            ),
            ("无效结算金额或时序", "Invalid settlement amounts or timing"),
            ("历史索引失效", "History index invalidated"),
            ("执行索引失效", "Execution index invalidated"),
            ("结算索引失效", "Settlement index invalidated"),
            ("发现市场", "Market discovery"),
            ("监控窗口", "Monitoring window"),
            ("初始化", "Initializing"),
        ] {
            result = result.replace(zh, en);
        }
        result
    }
}
