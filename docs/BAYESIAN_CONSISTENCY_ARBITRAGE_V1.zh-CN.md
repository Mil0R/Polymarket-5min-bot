# Bayesian Consistency Arbitrage v1

## 范围与安全边界

第一版只研究可由支付矩阵严格证明的父子分区：多个子事件必须互斥，且完整覆盖父事件。
它不把标题相似、统计相关或一般条件概率关系当作套利关系，也不实现签名、下单、撤单、
Split、Merge 或 Convert。

支持两个固定支付篮子：

```text
Parent NO + every Child YES  -> 每个有效状态固定支付 1
Parent YES + every Child NO  -> N 个子事件时固定支付 N
```

## 阶段门

1. `discover`：读取当前 Gamma 活跃市场，根据共同主题词与区间/幅度提示生成 `pending` 候选；
   同时解析显式 condition-ID 种子，保存规则正文、resolution source 和 SHA-256，但永不修改注册表
   或自动批准。
2. `discover-history`：独立读取已关闭事件，验证这种市场结构历史上是否出现；输出 scope 固定为
   `historical_closed_research_only`，代码禁止晋级。
3. `analyze-boundaries`：只读分析数值区间的缺口、重叠、开放尾部和父阈值切割；报告中所有条目
   `promotable=false`，不能替代规则审核。
4. `monitor`：按小时增量执行活动发现和边界诊断，追加快照、差异和告警；不运行 CLOB scan，
   不修改注册表、不下单。
5. `prepare-review`：把当前发现快照转换为审核清单；候选正文摘要变化时自动清空旧审核决定。
6. `promote`：只有审核证据、审核人、RFC3339 时间和手续费假设完整，且候选摘要仍匹配时，
   才原子追加正式注册表；永不覆盖已有 relationship。
7. `validate`：注册表必须为 version 1；只有 `manually_approved`、`disjoint=true`、
   `exhaustive=true` 且合约、token、规则哈希完整唯一的关系会被接纳。
8. `audit`：按 condition ID 重新读取 Gamma；问题、规则哈希、YES/NO Token 或可交易状态漂移
   时失败关闭。`scan` 和 `shadow` 启动前会强制执行同样检查。
9. `scan`：至少存在一条通过验证和实时审计的关系后，才读取一次真实 CLOB 盘口。
10. `shadow`：连续检测独立机会 episode；对新机会执行 100/500/1000ms 延迟复核，以及
   100%/75%/50% 可见深度折扣。延迟复核逐腿读取 REST，暴露多腿非原子的时间偏差。
11. `report`：统计独立机会、延迟后完整篮子、拒绝原因和最佳延迟后净边际。

任何阶段失败都不会自动放宽关系、边际、时间或深度约束。

## 发现阶段基线

2026-09-02 使用默认严格条件重新读取 Gamma 的前 1,000 个活跃事件，共解析 3,405 个可交易二元市场，
最终生成 0 个候选。早期宽松条件会把年份、累计阈值以及仅有共同主题词的市场误判为分区；当前实现
要求子市场覆盖其所在事件的全部可交易结果、父市场来自另一事件、父标题不含范围/阈值提示，且主题
词覆盖率至少为 75%。因此宁可漏掉候选，也不会把语义相关性当成可锁定支付关系。

发现输出位于：

```text
results/bayesian-consistency-arbitrage/discovery/candidates.json
```

该文件只是可重复生成的审核输入；其中所有候选都固定为 `pending`，不能直接用于 `scan` 或 `shadow`。

同日对 1,000 个已关闭事件进行独立历史扫描，解析 729 个有完整 Token 元数据的二元市场，严格候选
同样为 0。历史输出为 `results/bayesian-consistency-arbitrage/discovery/historical-candidates.json`，
只用于评估结构稀缺性，不能进入 `promote`。

## 显式 condition-ID 种子

自动词法发现覆盖不了跨事件或需要人工理解规则的关系。可在
`config/bayesian-consistency-candidate-seeds.json` 中填写父合约和 2–12 个子合约 condition ID，再运行
`discover`。种子必须是唯一、合法且当前开放的二元市场；解析失败会写入 `seed_errors`，不会影响其他
候选。种子只绕过词法召回，不绕过互斥、完备、摘要绑定、人工审核或实时审计。

示例结构：

```json
{
  "version": 1,
  "seeds": [
    {
      "seed_id": "human-readable-id",
      "title": "Why these markets may form a partition",
      "parent_condition_id": "0x...",
      "child_condition_ids": ["0x...", "0x..."],
      "notes": ["State the hypothesis, not the conclusion."]
    }
  ]
}
```

开发验证曾用三个 BTC 截止日期市场测试此链路：系统成功生成 `pending` 候选，但这些 YES 事件彼此
重叠、不互斥，因此没有保留为正式种子，也没有进入注册表。

## 区间边界诊断结果

运行：

```bash
./target/release/bayesian_consistency_arbitrage analyze-boundaries
```

2026-09-02 的真实扫描覆盖 941 个包含开放二元市场的事件，识别 7 个可解析的数值区间事件，生成
8 条有明确父阈值或条件决胜规则的诊断，数值上可完整表示的关系为 0。报告位于：

```text
results/bayesian-consistency-arbitrage/discovery/boundary-diagnostics.json
```

主要阻断案例：

- “共和党控制众议院”的推导边界为 `>=218`，但席位子市场的 `215–219` 桶跨越 218；不能拆分
  一份二元合约，所以无法用完整子腿复制父合约。
- “当年进入 Top 100 的新币数量”相关市场缺少完整上下尾部且区间有缺口。
- SpaceX 年度发射数的低位桶已经关闭，当前开放腿不再构成完备分区；另外 Starship 成功入轨与
  SpaceX 全部发射次数的语义也不同。
- 解析器只读取比较短语相邻的数字，绝不把问题中的年份当成阈值。

`numerically_representable=true` 也只代表整数区间可以由完整合约组合；规则来源、截止时间、平局、
取消和条件决胜仍需人工审核。当前报告的全部条目都固定为 `promotable=false`。

## 低频监控器

启动：

```bash
./target/release/bayesian_consistency_arbitrage monitor
```

默认每 3,600 秒执行一轮，使用以下文件：

```text
results/bayesian-consistency-arbitrage/monitor/events.ndjson
results/bayesian-consistency-arbitrage/monitor/state.json
```

`events.ndjson` 追加 `monitor_snapshot`、`monitor_error` 和停止事件；`state.json` 保存最近一次候选
及边界摘要，重启后会继续比较新增、消失和变化。可用
`BAYESIAN_CONSISTENCY_ARBITRAGE_MONITOR_DURATION_SECONDS` 做一次或短时验证；即使设置了 duration，
也至少完成一轮扫描。监控告警只打印到控制台，所有发现仍是 `pending`，不会自动执行
`prepare-review`、`promote`、`scan` 或 `shadow`。

## 人工审核与晋级

先生成或合并审核文件：

```bash
./target/release/bayesian_consistency_arbitrage prepare-review
```

默认审核文件为 `config/bayesian-consistency-candidate-reviews.json`。只允许编辑每个 review 的
`decision`、`reviewer`、`reviewed_at`、四个 proof 字段和显式手续费假设；不要编辑 `candidate_id`
或 `candidate_digest`。`decision=approved` 不足以完成晋级，任何必填证据为空都会被拒绝。

完成逐条规则审核后执行：

```bash
./target/release/bayesian_consistency_arbitrage promote
./target/release/bayesian_consistency_arbitrage validate
./target/release/bayesian_consistency_arbitrage audit
```

如果重新运行 `discover` 后候选正文或规则发生变化，必须再次运行 `prepare-review`；摘要不匹配的旧
审核决定不会被保留。`promote` 是显式修改注册表的命令，但只做追加，遇到同名 relationship 会退出，
不会覆盖人工维护的数据。

## 关系注册表

默认文件：

```text
config/bayesian-consistency-relationships.json
```

示例条目：

```json
{
  "version": 1,
  "relationships": [
    {
      "relationship_id": "reviewed-partition-id",
      "title": "Human-readable parent/child partition",
      "status": "manually_approved",
      "disjoint": true,
      "exhaustive": true,
      "reviewed_at": "2026-09-02T00:00:00Z",
      "notes": ["Record why no outcome is missing."],
      "parent": {
        "condition_id": "0x...",
        "question": "Parent event",
        "yes_token_id": "...",
        "no_token_id": "...",
        "rules_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        "fee_rate": "0.07",
        "fee_exponent": 1
      },
      "children": [
        {
          "condition_id": "0x...",
          "question": "Child A",
          "yes_token_id": "...",
          "no_token_id": "...",
          "rules_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
          "fee_rate": "0.07",
          "fee_exponent": 1
        },
        {
          "condition_id": "0x...",
          "question": "Child B",
          "yes_token_id": "...",
          "no_token_id": "...",
          "rules_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
          "fee_rate": "0.07",
          "fee_exponent": 1
        }
      ]
    }
  ]
}
```

规则哈希应由审核时保存的完整规则正文计算，不能把示例零哈希用于真实研究。

## 命令

```bash
cargo build --release --bin bayesian_consistency_arbitrage

./target/release/bayesian_consistency_arbitrage discover
./target/release/bayesian_consistency_arbitrage discover-history
./target/release/bayesian_consistency_arbitrage analyze-boundaries
./target/release/bayesian_consistency_arbitrage prepare-review
./target/release/bayesian_consistency_arbitrage promote
./target/release/bayesian_consistency_arbitrage validate
./target/release/bayesian_consistency_arbitrage audit
./target/release/bayesian_consistency_arbitrage scan
./target/release/bayesian_consistency_arbitrage shadow
./target/release/bayesian_consistency_arbitrage report \
  results/bayesian-consistency-arbitrage/shadow-run-001/events.ndjson
```

默认注册表为空，所以 `validate` 成功但 `scan` 和 `shadow` 会失败关闭。这表示代码阶段门工作正常，
不表示已经找到真实可交易关系。填入真实关系前必须逐条审核父子规则、取消条款、Other/平局/遗漏状态，
并更新每个合约的规则哈希。

## 当前 Shadow 模型边界

延迟执行使用逐腿 REST 盘口复核，是非原子执行的保守代理，但还不是订单级模拟：没有推断 FOK
提交确认、部分成交顺序和残腿实际卖出价格。因此 `delayed_shadow_evaluation.qualifies=true` 仍只是
下一阶段候选证据，不是成交或已实现收益。
