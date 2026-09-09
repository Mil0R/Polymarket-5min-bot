# NegRisk Gamma 发现失败修复

根因：旧 /events offset=2100 实测返回 HTTP 422，响应要求改用 /events/keyset。10 页×100 条×3 轮的第三轮必然进入超限范围；旧代码失败后每 10 秒重试同页并跳过整个 Shadow 扫描。

修复内容：

- 共用 Gamma 发现器改为 /events/keyset，解析 events 与 next_cursor，通过 after_cursor 顺序翻页。保留分窗口覆盖语义；从首游标遍历至目标窗口，前置页不计入该窗口发现结果。检测重复游标并报错，避免循环。
- 非成功响应包含 HTTP 状态、页码、目标窗口、Retry-After 头和最多 512 字符响应摘要；NegRisk 保存完整错误链。Retry-After 当前仅记录，重试使用下述本地退避。
- NegRisk 失败退避为 30、60、120、240、300 秒，之后保持 300 秒；成功后恢复配置的正常刷新周期。
- 发现失败保留已有路线，继续获取新盘口并执行原有新鲜度检查。路线元数据超过两个刷新周期后暂停采样，记录 sampling_unavailable；未完成首次发现时也暂停。刷新请求本身仍在扫描循环内等待，故请求耗时仍可能造成短暂停顿。
- 报告 event_counts 可区分 scan_summary、error、sampling_unavailable。采样暂停事件的 interval_secs 是计划等待时长，不是精确的总停机时间；不能把报告首尾跨度全部当作有效观察时长。

验证：19 项 cross_market_probability_arbitrage 相关测试通过；NegRisk 与 Cross-Market release 编译成功。实际游标请求连续通过 22 页（每页 100 条），第 20、21 页均为 HTTP 200，已覆盖旧 offset 限制边界。

新版 NegRisk 只读 scan 冒烟发现 1 条路线，获得 5 个盘口并正常生成报告，无 discovery 错误；测试产物位于 results/neg-risk-conversion-arbitrage/smoke-keyset-20260909/。这只证明发现/采样链路正常，不是套利盈利验证。

部署：替换服务器 neg_risk_conversion_arbitrage 后重启 `./neg_risk_conversion_arbitrage shadow`。现有 .env 的 DISCOVERY_WINDOWS=3 可以保留，不需要临时降至 2。日志和结果仍追加原配置目录，按 run_id 区分修复前后。共用客户端的 cross_market_probability_arbitrage 也已编译新版。
