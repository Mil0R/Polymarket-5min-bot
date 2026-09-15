# Cross-Venue Arbitrage 最新 Shadow 批次分析

## 结论

本批次不能证明跨平台套利不可行，也不能支持进入实盘。18.34 小时运行中没有候选事件或盘口探测，直接原因是全部 5,896 条重复记录的标签都是 `ALIGNED`，在检查新鲜度和配对映射之前就被跳过。当前本地 pair registry 为空，是后续盘口验证的障碍，但不能据此认定服务器整个运行期间 registry 为空。PMP 每轮返回 7–12 条记录，来源时间戳约每 15 分钟更新。

本报告已依据原始事件和代码二次审核修正。Shadow 没有订单功能；`actual_fills=0`、`realized_pnl=0` 是固定输出，不代表已完成交易盈利验证。

## 分析范围

- 运行目录：`results/cross-venue-arbitrage/shadow-run-001/`
- 主报告：`results/cross-venue-arbitrage/shadow-run-001/report.json`
- 配置中的系统日志：`logs/cross-venue-arbitrage/shadow-run-001/system.log`
- 本地非空运行日志：`logs/logs/cross-venue-arbitrage/shadow-run-001/system.log`（疑似同步目录嵌套，未核验具体同步命令）
- 运行批次：`40c3e0a2-2c37-42c0-a77b-adf2c32cc7e0`
- 时间：2026-09-14 14:20:35 UTC 至 2026-09-15 08:41:05 UTC
- 持续时间：66,029 秒，约 18.34 小时

## 运行统计

| 指标 | 结果 | 解释 |
|---|---:|---|
| PMP 原始响应 | 547 | 本地读取到了 Key；服务端 Pro 权限未核验 |
| feed summary | 542 | 大多数轮次完成扫描 |
| 旧日志周期错误 | 6 | 5 次供应商明确返回无数据，1 次 HTTP 504 |
| 周期未取得有效数据比例 | 6/548，约 1.1% | 不能等同网络失败率；其中 HTTP 504 占 1/548，约 0.18% |
| 返回行数 | 7–12 条/轮，重复计数 5,896 | 7条×73轮、9条×31轮、10条×60轮、11条×30轮、12条×348轮 |
| 信号 | 5,896 条全部 `ALIGNED` | 没有 `ARB`、`ARB_FLAGGED` 或 `WATCH` |
| 唯一 source 时间戳 | 74 | 相邻 73 个间隔中位数 900 秒，范围 883–918 秒；不等同独立机会 |
| source age 中位数/最大值 | 452/1,019 秒 | 供应商底层体育数据不是 120 秒级刷新 |
| 超过旧阈值的扫描 | 472 | 扫描年龄超限，不是 472 次套利候选被拦截 |
| pair opportunity | 0 | 全部标签被跳过，没有进入候选检查 |
| 盘口探测/稳健机会 | 0/0 | 没有进入两边 order book 校验 |
| 实际成交/PnL | 0/0 | 仅 shadow，且没有候选配对 |

## 关键问题判断

### 0. 日志路径需要统一确认

本地配置路径是 `logs/cross-venue-arbitrage/shadow-run-001/system.log`，非空历史日志位于 `logs/logs/cross-venue-arbitrage/shadow-run-001/system.log`。疑似同步目录嵌套，但尚未验证原因。事件和报告来自 `results/cross-venue-arbitrage/shadow-run-001/`；应核对同步源/目标目录，避免误读空文件。

### 1. “没有交易”不是价差被验证为不可行

代码先检查标签是否属于 ARB / ARB_FLAGGED / WATCH，再检查场地、新鲜度、配对映射。此次没有一条记录通过标签检查，即使补齐配对或改为 900 秒也不会产生盘口探测。当前本地 registry 为空；旧事件未记录 registry 条目数，不能反推历史服务器配置。新增 `probe` 可以对少量人工审核配对直接验证盘口，避免始终等不到标签触发。

### 2. 数据新鲜度存在明显错配

每 120 秒轮询一次，来源时间戳约每 900 秒变化。900 秒作为候选发现容忍窗口有依据，但不是可成交报价的新鲜度标准。旧批次年龄超过 120 秒有 472 次；同批次按 900 秒重算为 2 次。这个变化只来自门槛调整，不表示数据质量改善，也不表示找回了套利机会。盘口仍要求 REST 延迟不超过 1 秒、Polymarket 盘口年龄不超过 5 秒；Kalshi 缺少已验证的交易所时间戳。报告现在分别记录年龄超限、供应商 stale 和两者合并口径。旧数据供应商 stale 均为 false。

### 3. PMP 返回是否完整仍未被证明

542 次成功响应均满足 `returned=total_count`，这是完整返回该接口所声明结果集的积极证据。但 `truncated` 缺失，响应也没有已核验的 Pro 权益字段，不能据此证明全市场覆盖。`pro_key_present` 只表示本地读取了 Key；新报告明确输出 `pro_entitlement_verified=false`、`coverage_status=unverified`。需要独立核验服务端权限与订阅数据覆盖，继续轮询本身不能完成这项验证。

### 4. 错误需要区分业务无数据与请求失败

5 次 MCP 错误原文均为 `No arbitrage data available right now.`；无法由此确认是市场无结果还是供应商暂时缺数据，应计入供应商数据不可用。新代码保存原始响应和分类，等待下一个周期；HTTP 5xx、网络异常和明确的临时 RPC 错误最多尝试 3 次（2s/4s 两次退避），401/403、429、未知工具错误和 schema 错误不进行短间隔重试。旧报告中的 `rejections` 保留原始日志用语，新增 `feed_health` 给出重分类；两者不可重复相加。

## 已完成的调整

1. pair registry 缺失时自动创建空 registry 并继续采集，避免服务因 `open docs/cross-venue-arbitrage-pairs.json` 反复失败。
2. 默认 registry 路径统一为 `results/cross-venue-arbitrage/pairs.json`。
3. 区分 provider_no_data、HTTP、网络、鉴权、限流、RPC 与 schema；只对指定临时故障最多尝试 3 次，记录每次尝试。
4. 默认 source age 上限改为 900 秒。
5. 统一 source_stale 判断，记录标签跳过、映射拒绝、配对数量与数量上限延后；报告补充每轮行数分布、返回总量匹配与未验证权限说明。
6. 新增独立 `probe` 入口；`inspect` 仅做元数据和规则审核。两者使用独立子目录和日志，保留主 Shadow 最新运行报告。

验证：13 项 cross-venue 测试通过，release 二进制编译成功。重生成本批次报告得到 `label_skipped=5896`、`provider_no_data=5`、历史 HTTP 错误 1 次、每轮返回 7–12 条、年龄超限 472 次。临时目录 CLI 冒烟验证：空 registry 的 `probe` 返回非零状态并生成独立诊断报告，没有 PMP 请求，也未生成或覆盖主目录报告。尚未对真实已审核配对完成盘口验收，Pro 服务端权限仍待核验。

## 下一步建议

### 必须做

1. 部署最新二进制，核对 `.env` 中有效配置和唯一 registry 路径后重启。缺失 registry 会自动创建，无需同时复制两个模板，也不要覆盖已有人工审核配置。
2. 核验 Pro 权限和接口覆盖范围。已有约 18 小时数据足以说明来源时间戳刷新规律，无需强制继续空跑 24–48 小时。
3. 选择少量明确同一结算结果的配对，填好规则、费用和有效期，用 `inspect` 生成指纹并人工审核，再用 `probe` 验证两边盘口。即使 PMP 全部 ALIGNED，也可进行此诊断；它不计作 PMP 发现率证据。
4. 基础盘口链路通过后继续 Shadow，以独立配对、有效盘口、延迟复核覆盖决定观察时间。7–14 天只是观察预算，不是自动晋级条件。当前部分成交、失败腿实际退出、结算与真实费用仍未校准，必须取得相应证据后才考虑 canary。

### 暂不建议

- 不要因为本批次 0 opportunity 就降低 `CROSS_VENUE_ARBITRAGE_MIN_NET_PER_PAIR_USD`、跳过 pair review 或直接实盘。
- 不要把 `truncated=null` 当成完整市场数据。
- 不要把 5,896 条重复返回当作独立机会。

## 最终审核

调整重点是数据接入、筛选可观测性和候选配对验证。当前可以运行只读 Shadow 和独立盘口诊断；现有结果没有实际盘口或独立机会证据，尚不足以评估净盈利可行性。
