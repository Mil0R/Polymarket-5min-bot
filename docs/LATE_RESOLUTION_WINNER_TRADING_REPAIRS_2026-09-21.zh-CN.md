# Late-Resolution Winner Trading 数据安全修复（2026-09-21）

## 已实现

### 1. Journal 磁盘空间保护与写失败回滚

- 新增 `LATE_RESOLUTION_WINNER_TRADING_JOURNAL_MIN_FREE_BYTES`，默认保留 2 GiB。
- 状态 journal 与评估 journal 在启动和每次追加前检查剩余空间。
- 事件先完整序列化，再一次性写入；写入、flush 或 sync 失败后尝试截回写入前长度，并将 writer 永久置为 fail-closed。
- journal 文件使用进程级独占锁，避免两个进程同时使用相同路径。
- Winner Trading 启用时，服务磁盘健康门槛使用同一配置值。

### 2. 损坏评估 journal 恢复

- 启动时逐行验证 evaluation journal。
- 非法原始行完整保存到 `evaluations.ndjson.corrupt-<timestamp>`。
- 对“残缺前缀 + 完整后续事件”的已知 ENOSPC 形态，恢复可解析的完整后缀事件。
- 修复结果先写同目录临时文件、同步后原子替换。
- 报告工具不再因一个坏行完全失败；它会恢复可解析后缀、跳过无法恢复的行，并输出 `JOURNAL INTEGRITY: status=FAILED`，明确说明报告只能用于诊断。
- 新增 `scripts/repair_ndjson_journal.py`：默认只读检查；`--output` 生成修复副本；`--in-place` 会先创建完整备份再原子替换。运行中的 journal 禁止使用 `--in-place`。

### 3. Shadow 执行幂等与双轨共享

- 每个 `evaluation_id` 最多持久化一个 `shadow_executed`。
- rejected Shadow 与敏感性 Shadow 同时确认时，共享一次延迟盘口读取和模拟执行。
- 合并事件同时携带主研究 cohorts 与 sensitivity groups；报告工具把同一个执行结果归入两个研究区块，但不会产生第二笔执行事件。
- 指标只在结果成功持久化后更新，重复结果不会重复增加 fill/no-fill 计数。

## 损坏快照验证

对 `results-incoming/20260921T095406Z/.../evaluations.ndjson`：

- 检测到非法源行：2；
- 恢复完整后缀事件：2；
- 无法恢复并丢弃的完整行：0；
- 修复副本：2,487 条全部通过 JSON 解析；
- 新报告工具可直接读取原始损坏文件，并保持主结果 `98 markets / 69 fills / +3.80958730 USDC` 不变，同时标记完整性失败。

## 上线步骤

1. 停止远端 dry-run 进程，防止修复时继续追加。
2. 对原始 results/logs 做时间戳归档。
3. 用修复工具的 `--output` 生成新文件并复算报告；核对计数后再用 `--in-place`，保留自动备份。
4. 部署新 release binary，并确保远端 `.env` 的空间阈值符合磁盘容量。
5. 使用新批次目录启动，验证启动日志包含 journal ready，且连续候选中每个 `evaluation_id` 只有一个 `shadow_executed`。
6. 运行至少一个完整 5 分钟窗口后再确认旧批次归档完成。

远端当前根分区约 6.7 GiB，剩余约 2.2 GiB；默认 2 GiB 阈值余量很小。部署前应清理或扩容，否则新进程会按设计拒绝启动或很快停止 journal 写入。
