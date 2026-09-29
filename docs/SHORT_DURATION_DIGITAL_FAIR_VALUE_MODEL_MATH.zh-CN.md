# Short Duration Digital Fair Value：套利逻辑与数学算法

> 代码依据：`src/digital_fair_value.rs`、`src/digital_fair_value_calibration.rs`、`src/late_resolution_winner_model.rs`、`scripts/digital_fair_value_study.py`。
> 本文只解释现有实现的逻辑与算法，不评价参数优劣、不修改任何配置。

## 1. 标的：5 分钟二元「数字期权」

`btc/eth/sol-updown-5m-<window_start>` 每 5 分钟开一个新窗口，结算规则是**最终 TWAP 是否高于窗口开盘时的 price-to-beat**：

```
payoff(UP) = 1[ S_final > K ],  K = 窗口起点的 Chainlink 60s TWAP
payoff(DOWN) = 1 - payoff(UP)
```

这是标准的 **cash-or-nothing digital option**（数字/二元期权）：到期收益非 0 即 1，因此在 `[0,1]` 上的价格可直接解释为胜率，`p` 与价格同量纲——这是整个策略能够"用概率做套利"的前提。

平台层面还有两条平价约束：

- `YES + NO` 在结算时恰好赎回 1 USDC；
- 因此 `UP_ask + DOWN_ask < 1` 是**无风险**买双腿→Merge 的套利，`> 1` 是 Split 卖出套利。该策略模块**不做**这条腿（由 `binary-complement-arbitrage`、spread arbitrage 模块负责），它做的是下一节的方向性定价套利。

## 2. 套利逻辑：模型概率 vs 盘口报价

### 2.1 主逻辑

模块给每一侧算一个**公允概率** `p`，与盘口可执行卖价比较：

```
edge_side = p_side − ask_side − fee(ask_side)
```

- UP 腿：`p_up` 与 `up_ask`；DOWN 腿：`1 − p_up` 与 `down_ask`；
- 两侧同时评估，取 `edge` 较大者；只有 `edge ≥ MIN_NET_EDGE`（默认 0.03）才产生机会（`evaluate_live_tops` / `live_edge_evaluation`）。

含义：**当市场对某一侧的报价低于模型认为的公允价值（含费后仍有 ≥3 分钱空间）时买入该侧**。这是一次"押注"而不是无风险套利——风险敞口是模型概率错了。买入后到结算之间不再有对冲动作，最大亏损 = 买入成本，最大收益 = `1 − cost`。

### 2.2 与"真套利"的边界

| 类型 | 条件 | 是否有风险 | 本模块 |
|---|---|---|---|
| 平价套利 | `UP_ask + DOWN_ask + 2·fee < 1` | 无（Merge 赎回 1） | 不做 |
| 定价套利（本模块） | `p_side − ask − fee ≥ 0.03` | 有（模型风险） | 做 |
| 波动率错价 | 同一 margin 下 σ_model < σ_market | 有 | 隐含在上面 |

### 2.3 错价从哪来（经济直觉）

数字期权价格对 `margin/σ_rem` 极度非线性：临近到期时，`K` 附近的微小价差会让 `p` 在 0.5 附近快速移动，而深度实值侧的价格会像阶跃函数一样趋于 1。市场用**隐含波动率 + 离散 tick + 有限深度**定价；模块用**已实现波动率 + 解析正态公式**定价。两者在 `σ` 和漂移假设上的差异，就构成了被捕捉的"错价"：若市场隐含 σ 偏大（或方向判断保守），深度虚值侧会被卖得过便宜，模型就会给出正 edge。

## 3. 数学算法逐条

### 3.1 开盘参考价 `K`

`opening_reference()`：在窗口起点 ±5 s（`OPENING_MAX_SKEW_MS=5000`）内，取 `twap_source == "chainlink_twap"` 且值 > 0 的样本，选与窗口起点**时间距离最近**的一条。找不到则不评估（`no_opening_reference`）。

### 3.2 方向距离（margin，单位 bps）

```rust
margin_bps = ln(S_current / K) × 10_000
directional_margin_bps = buy_up ? margin_bps : −margin_bps
```

用**对数收益**衡量"离敲定价多远"，量纲为 bps。窗口内标的相对 K 的微小漂移即代表方向性优势。

> 注：`late_resolution_winner_model.rs`（Winner Trading 模块）用的是**简单收益** `(S/K − 1)·1e4`，与本文档描述的 fair value 模块略有差异，两者不可混用同一套阈值。

### 3.3 剩余期限波动（平方根时间）

```rust
remaining_sigma_bps = σ̂ × sqrt(remaining_secs)
```

布朗运动的方差随时间线性增长 → 标准差按 `√τ` 缩放。`σ̂` 的单位是 **bps per √second**（不年化，因为持有期只有 5 分钟）。

### 3.4 波动率估计量 σ̂（bps/√s 已实现波动）

```rust
// 相邻样本，按 Δt 归一化，再取均方根
σ̂ = sqrt( mean( (ln(S_i / S_{i−1}) × 1e4)² / Δt_i ) )
```

- 取 `VOLATILITY_LOOKBACK_SECS=120` 秒窗口、至少 `MIN_OBSERVATIONS=20` 个样本；
- 对同一 `observed_at_ms` 的重复样本去重（`last_observed_at`）；
- 大量逐秒增量被 TWAP 平滑，因此这是一个**被平滑过程支配的**短尺度估计量——这是该策略最脆弱的假设（见 §5）。

另有 `horizon_volatility(samples, seconds)`：只取 `Δt ∈ [h, h+3]` 的**非重叠**增量对（游标 `start` 前移跳过已用段），给出 10/20/40/60 秒四个期限的 σ 诊断值，用于检验 √t 假设是否成立。

### 3.5 公允概率：数字期权公式

```rust
p_up = Φ( margin_bps / max(σ̂_eff × sqrt(τ), 1e-9) )
p_down = 1 − p_up
```

即 `P(S_T > K) = Φ( ln(S/K) / (σ√τ) )`——零漂移、无贴现的 digital option（现金或无）公式。因为 5 分钟窗口内漂移相对于波动可忽略，且收益在结算时一次性支付，故不引入 `r`。

正态 CDF 用有理逼近（不是 `libm` 精确 erf）：

- `digital_fair_value.rs`：Abramowitz–Stegun 7.1.26 型逼近，`x = |z|/√2`，`t = 1/(1+0.3275911x)`，多项式系数 `1.061405429 / −1.453152027 / 1.421413741 / −0.284496736 / 0.254829592`，误差 ~1e-7 级别；
- `late_resolution_winner_model.rs`：另一种 A&S 26.2.17 型密度多项式形式；
- 两处都把结果 clamp 到 `[1e-6, 1−1e-6]`（winner 模型为 `[1e-4, 0.9999]`），避免 log loss / edge 出现 ±∞。

### 3.6 波动率下限（σ_floor）

```rust
σ̂_eff = max(σ̂_observed, σ_min(asset))
σ_min = BTC 0.8 / ETH 1.2 / SOL 1.5   (bps/√s)
```

- 作用：当 `margin → 0`（价格几乎贴着敲定价）时，`p` 会被推回 0.5，防止"绝对确定"的过度自信；`volatility_floor_active` 是每次评估都会记录的审计位（007 批次 828/828 全部命中）。
- 敏感性研究用 `floor_scale ∈ {0.25, 0.5, 1.0}` 重算概率（`sensitivity_decision`），用于判断"是下限还是模型本身导致亏损"。
- 无下限概率 `probability_without_volatility_floor` **只用于诊断**，不参与准入。

数值感受（`σ_min`）：

| 资产 | τ=10 s | τ=60 s | τ=300 s |
|---|---:|---:|---:|
| BTC (0.8) | 2.53 bps | 6.20 bps | 13.86 bps |
| ETH (1.2) | 3.79 bps | 9.29 bps | 20.78 bps |
| SOL (1.5) | 4.74 bps | 11.62 bps | 25.98 bps |

### 3.7 手续费模型

```rust
fee(price) = rate × (price × (1 − price))^exponent        // rate=0.07, exponent=1
```

- 在 `p=0.5` 处最大（0.0175/份），在 `p→0/1` 处趋于 0；
- `p=0.3` → 0.0147；`p=0.9` → 0.0063；`p=0.97` → 0.0020；
- 同一公式在 `src/trading/executor.rs::fee_per_share` 中用于真实/纸面执行，费率与指数来自交易所返回的 market rules（`winner_fee_policy`），本模块的 0.07/1 是假设值，记录在报告的 `fee_rate_assumption` 里。

### 3.8 准入闸门（`live_edge_evaluation`）

按顺序（任一失败即拒，原因写入 journal）：

1. 盘口完整（bid/ask/size 都存在）→ `incomplete_book`
2. `MIN_PRICE=0.05 ≤ ask ≤ MAX_PRICE=0.95` → `price_out_of_range`
3. 未交叉（`ask ≥ bid`）→ `crossed_book`
4. `ask − bid ≤ MAX_SPREAD=0.03` → `spread_too_wide`
5. `ask × size ≥ MIN_DEPTH_USDC=10` → `insufficient_depth`
6. `edge = p − ask − fee(ask)`，需 `≥ min_edge`

注意价格上限 0.95：与 Late-Resolution Winner Trading（买 0.96–0.98 的深实值）相反，本模块偏好在**中低价格侧**寻找被低估的一侧。

### 3.9 盘口走单与预算求解

- `decode_asks()`：zstd 解压盘口快照 → 解析 `asks[].price/size` → 过滤非法值 → 按价格升序；
- `walk_ask_levels_partial(asks, target, haircut)`：从最优卖价开始逐层吃，每层可用量打 `LIQUIDITY_HAIRCUT=0.75` 折扣，返回 `(shares, cost)`；
- 单笔预算 `ORDER_USDC=5.0`：`budgeted_live_fill()` 对目标股数做 **64 轮二分**，找满足

```
shares × ( avg_price × (1 + SLIPPAGE_BPS/10000) + fee(avg_price) ) ≤ ORDER_USDC
```

的最大可行 `shares`，再按该规模真实走单；`MIN_PARTIAL_FILL_RATIO=0.25` 决定部分成交是否采纳。

### 3.10 延迟与可复现执行

- 信号产生后固定等待 `EXECUTION_DELAY_MS`（默认 1000 ms），**重新拉 REST 盘口**并重算模型概率（`fresh_execution_state` + `live_probability`）——即"信号时刻的 edge"与"成交时刻的 edge"是两个不同的量；
- 重新校验：REST 延迟 ≤ `MAX_REST_LATENCY_MS=2000`、盘口年龄 ≤ `MAX_BOOK_AGE_MS=1000`、token/condition identity 匹配、`validate_exchange_age` 校验交易所时间戳；
- `deterministic_failure(key, rate)`：对 `key` 求 `DefaultHasher` 哈希再映射到 `[0,1)`，与 `ORDER_FAILURE_RATE=0.02` 比较——**确定性**地模拟 2% 下单失败，保证同一份历史数据重复回放得到完全相同的结果（可复现的蒙特卡洛替代方案）。

### 3.11 时点与参数网格

- `CHECKPOINTS_SECS=240,180,120,60,30,10`：每个市场最后 N 秒内持续扫描，每市场最多一次尝试（不是同时下多单）；
- 回测网格：`BACKTEST_DELAYS_MS=100,250,500,1000` × `BACKTEST_LIQUIDITY_HAIRCUTS=0.50,0.75,1.0`，用于给出"延迟/深度敏感性"区间而不是单点估计。

### 3.12 校准与验证算法

评估指标（`MetricAccumulator::record`、`digital_fair_value_calibration.rs::Scores`）：

```
Brier = mean( (p − y)² )
LogLoss = −mean( y·ln p + (1−y)·ln(1−p) )
校准桶 = 按 floor(p×10) 分成 10 桶，比较桶内平均 p 与实际 Up 频率
市场基线 = 同一样本用市场 mid 计算的 Brier（比模型强才有意义）
```

冻结研究 `scripts/digital_fair_value_study.py` 的算法：

1. **按时间分区**（train / validation / 复核），同窗口资产×期限不跨区泄漏；输入文件与官方标签缓存都用 SHA-256 记录（`provenance`）；
2. **固定小模型族**：
   - `current`：现有 `p`；
   - `floor`：`σ = max(σ̂, σ_min·scale)`、`scale ∈ {0.25, 0.5, 1}`（**只用训练集选 scale**）；
   - `empirical`：直接对训练集 `ln(final/current)·1e4` 残差分布建模，概率为

     ```
     p = (1 + #{r ≥ ln(K/S_t)·1e4}) / (n + 2)      // 加一平滑，限制极端自信
     ```

3. **选择规则**：仅看验证集，候选必须同时不劣于 `current` 的 Brier 与 log loss，再取 log loss 最小者；选定后不使用复核标签重拟合；
4. `model.json` 保存模型与源数据哈希，可供**只预测不训练**的冻结前瞻验证（008 批次用 SHA-256 固定模型）。

## 4. 准入门禁（两道独立的闸）

**A. 回测报告门（`run_backtest` 末尾计算 `readiness`）**

```
direction_ready = test 方向样本 ≥ MIN_DIRECTION_SAMPLES(500)
               && 结算参考样本 ≥ 500 / checkpoints 个数
               && 参考价匹配率 ≥ 0.99
               && test Brier < 0.25
               && (无市场报价样本 || test Brier ≤ 市场 Brier × 1.05)

conservative_cost_ready = 最保守场景（最大延迟 1000 ms + 最小流动性折扣 0.50）
                          在 validation 与 test 上均满足 fills ≥ MIN_COST_FILLS(100) 且净 PnL > 0
```

三态结论：`NOT_READY_DIRECTION_MODEL` / `NOT_READY_COST_SENSITIVITY` / `READY_FOR_DRY_RUN_REVIEW`。注意第二个条件要求**含费净 PnL 为正**，而不是"Brier 变好"。

**B. 金丝雀门（dry-run 报告 `canary_gate`）**：只统计通过 v6 数据质量筛选（execution_version 匹配、交易所时间戳 ≤1 s、参考观测年龄 ≤5 s、RTDS 发布到本机接收 ≤2 s、本机接收到决策 ≤2 s、REST ≤2 s）的成交。上游正常发布时间延迟与本机行情陈旧已分开，不能再用单一 `feed_age_ms` 混合判断。门禁要求整体净 PnL > 0、Brier ≤ `CANARY_MAX_BRIER_SCORE(0.10)`，且每个启用资产样本 ≥ `CANARY_MIN_ASSET_SAMPLES(50)`；不满足则自动实盘提升保持关闭。

冻结研究（008）另有一道明确的**执行收益关卡为 `NOT_EVALUABLE`**，独立于概率关卡。

## 5. 这套逻辑为什么会失效（数学上的三个死穴）

1. **σ 估计量被 TWAP 平滑**：`σ̂` 由平滑价格的逐秒增量估计，低估真实终值残差；而分母 σ 越小，`p` 越极端、越自信。经验残差模型（`empirical`）正是为绕开这个假设而写。
2. **波动率下限把概率压向 0.5**：`σ_floor` 生效时，任何 `|margin| < σ_floor·√τ` 的市场都被估成接近 0.5。若市场对便宜一侧报价 0.30，模型就会认为它被低估 20 分——007 批次 20 笔成交中 18 笔均价 < 0.35、最终 1 胜 19 负，正是这一机制的直接后果。
3. **可交易子集远差于市场**：007 批次有报价子集上模型 Brier 0.2192（30 s）vs 市场中点 0.0548，说明"模型比市场聪明"的前提在有报价处并不成立；全窗口 Brier 好看只是被大量接近确定的窗口掩盖。

因此正确的判据不是"全窗口 Brier"，而是**在有报价、可成交的子集上，模型必须同时优于市场 mid 的 Brier 与 log loss**，然后才谈 `edge ≥ min_edge`。

## 6. 一句话总结

> 用 Chainlink 60s TWAP 构造数字期权的解析公允概率 `Φ(ln(S/K)/(σ√τ))`，与 Polymarket 盘口卖价比较，在含费净 edge ≥ 阈值时买入被低估的一侧——这是**建立在概率校准正确之上的统计套利**；真正的无风险套利（`UP+DOWN<1` 的平价）由其他模块负责。当前所有失败都可归因到 `σ` 低估与波动率下限导致的概率高估，而不是执行链路。
