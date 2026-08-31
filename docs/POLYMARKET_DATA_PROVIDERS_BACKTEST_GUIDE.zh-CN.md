# Polymarket 历史数据供应商与量化回测适配指南

> 调研日期：2026-08-31  
> 调研对象：PMData、PolyBackTest、PolyOrderbooks  
> 用途：选择回测数据源，并为当前及后续 Polymarket 量化策略规划数据适配器。

## 1. 核心结论

三个供应商并不是简单的替代关系，而是分别适合不同层级的研究：

| 数据供应商 | 核心优势 | 最适合的研究 | 主要不足 |
|---|---|---|---|
| PMData | 增量 L2、逐笔成交、链上成交、钱包地址、Chainlink 原始流与 TWAP | 事件驱动套利、订单流、跟单、钱包分析、预言机边界、做市 Shadow | 历史覆盖起点较新；仍没有真实排队位置和完整订单 ID 生命周期 |
| PolyBackTest | 标准化 Crypto Up/Down 市场、较高频快照、标的价格、结算结果 | Binary Complement、Late-Resolution、数字期权模型、标准化本地回测 | 主要面向加密 Up/Down；默认历史窗口有限；部分 Binance 数据接口需按当前版本和套餐复核 |
| PolyOrderbooks | 使用简单、覆盖多个加密市场、统一频率的 L2 快照与市场指标 | 广泛筛选、流动性统计、静态吃单回测、概率曲面和市场状态研究 | 1 秒采样可能漏掉短暂机会；稀疏数据会前值填充；缺少逐笔成交和钱包身份 |

对当前项目，建议继续把 **PolyBackTest 作为第一阶段标准化本地回测数据源**；新增 **PMData 事件数据适配器**，用于订单流、钱包、预言机和更真实的 Shadow 执行研究；PolyOrderbooks 可作为后续广市场扫描和独立交叉验证数据源。

需要特别强调：三者都不能单独完成交易所级别的精确 Maker 撮合回放。没有历史订单的真实队列位置、撤单优先级和自身订单到达时点，就只能构建 queue-aware 概率模型，不能把“触价”直接当作“成交”。

## 2. 数据能力对比

| 数据能力 | PMData | PolyBackTest | PolyOrderbooks |
|---|---:|---:|---:|
| 市场元数据与结算结果 | 是 | 是 | 是 |
| 完整 L2 订单簿 | 增量事件/快照 | 高频快照 | 固定周期快照 |
| 逐笔成交方向与数量 | 是 | 未作为核心数据提供 | 未作为核心数据提供 |
| 链上 Maker/Taker 钱包 | 是 | 否 | 否 |
| 数据源时间与本地接收时间 | 是 | 需按接口字段验证 | 固定采样时间为主 |
| Chainlink 原始价格 | 是 | 对齐的标的参考价格 | 否，需外部数据源 |
| Chainlink 30/60 秒 TWAP | 是 | 未明确提供独立 TWAP 流 | 否 |
| Binance 现货/期货数据 | 否，需自行接入 | 文档列出，使用前应核实 v4 接口和套餐 | 否 |
| 钱包画像/跟单研究 | 强 | 不适合 | 不适合 |
| 非加密市场 | 完整市场数据自 2026-06 起 | 不适合 | 当前不支持 |
| 可直接重建精确 Maker 队列 | 否 | 否 | 否 |

## 3. PMData

### 3.1 提供的数据

PMData 以 Parquet 文件提供数据，重点不是定时快照，而是保留 WebSocket 事件及不同时间语义：

- L2 数据包含完整 `book` 快照、`price_change` 档位变动、最优价和 tick-size 等事件。
- `timestamp` 表示数据源事件时间，`local_timestamp` 表示采集端接收时间，可用于估计传输延迟和信息陈旧程度。
- 逐笔成交包含 outcome、价格、数量和主动方 BUY/SELL。
- 链上成交包含 Maker/Taker 钱包、方向、outcome、美元金额、token 数量和交易哈希。
- Chainlink 数据包括 BTC、ETH、SOL、XRP、DOGE、BNB、HYPE 等资产的直接价格流，以及 30 秒、60 秒 TWAP。
- Up/Down 专用数据支持 5 分钟、15 分钟、1 小时市场。

文档公布的主要历史覆盖起点：

- Crypto Up/Down：2026-02-01 起。
- 完整 Polymarket 市场：2026-06-01 起。
- Chainlink direct stream：2026-06-07 起。
- Chainlink TWAP：2026-08-01 起。

### 3.2 最适合的策略

1. **Binary Complement Arbitrage**
   - 用增量 L2 重建 YES/NO 两腿的可执行深度。
   - 模拟两腿不同到达时间、部分成交和短暂价格机会。

2. **Late-Resolution Winner Trading**
   - 把 Chainlink direct/TWAP 与 Polymarket 盘口按事件时间对齐。
   - 研究市场对标的价格、预言机窗口和临近结算信息的反应延迟。

3. **Market Making / Shadow Maker**
   - 用逐笔成交、盘口变化、价差和短期 markout 估计 adverse selection。
   - 可实现 queue-aware 成交概率模型，但不能宣称精确复现历史成交。

4. **Copy Trading 与钱包质量分析**
   - 利用链上 Maker/Taker 钱包构建收益、活跃度、市场类别、持仓时间和共同交易图谱。
   - 检查地址间同步交易、循环资金、对敲和疑似虚假盈利。

5. **订单流动量/反转与毒性研究**
   - 使用主动买卖方向、成交数量、盘口撤补速度、microprice 和 markout。

6. **预言机/TWAP 边界策略**
   - 分析 direct stream、30/60 秒 TWAP 和市场隐含概率的偏离与收敛。

7. **政治、天气、体育等跨市场概率一致性**
   - 完整市场数据可与 Gamma 的父子市场关系结合，用于条件概率和互斥事件约束研究。

### 3.3 局限性

- 历史起点较新，不足以覆盖多个年度、选举周期或极端行情周期。
- 增量档位变化不是完整的订单 ID 生命周期，无法知道每张单在队列中的确切优先级。
- 链上时间不能替代 CLOB 内部撮合时间；跟单回测必须加入可观察延迟。
- Liquidity Rewards 策略还需要历史奖励规则、实际账户奖励和最终库存损益，单靠行情数据不足以证明盈利。

## 4. PolyBackTest

### 4.1 提供的数据

PolyBackTest 面向标准化 Crypto Up/Down 市场，当前文档列出：

- BTC：5m、15m、1h、4h、24h。
- ETH、SOL：5m、15m、1h。
- 市场元数据：market ID、slug、周期、开始/结束时间、开盘标的价格、token、winner、最终成交量/流动性、结束标的价格和 resolved_at。
- 市场快照：标的价格、UP/DOWN 价格，以及可选的两侧完整订单簿。
- 当前数据保留说明约为每个市场每秒 8 个快照，实际回测应检查相邻时间戳确认采样分布。
- Chainlink 对齐的参考价格和最终 winner。

文档还列出 Binance 现货/期货 20 档订单簿和 1 秒聚合 OHLCV、主动买卖量、成交笔数等数据。由于服务正在从 v3 迁移到 v4，正式开发 CEX lead-lag 前应先用当前 API Key 验证端点、字段、覆盖时间及套餐权限。

历史窗口和限制以套餐为准：

- 默认历史窗口约 31 天，更长的 60/120 天通常需要附加套餐或 Enterprise。
- Free 主要为 BTC，限速约 1 请求/秒。
- Pro 支持 BTC、ETH、SOL，文档列出的限速约为 10 请求/秒、300 请求/分钟。

### 4.2 最适合的策略

1. **Binary Complement Arbitrage**
   - 直接对 UP/DOWN 两侧订单簿执行深度消耗和两腿成本回测。
   - 很适合当前项目第一阶段快速验证阈值、手续费、滑点和最小订单约束。

2. **Late-Resolution Winner Trading**
   - 使用标的价格、盘口价格、结束时间和 winner 评估临近结算的胜率与收益。
   - 可按 BTC/ETH/SOL 分别校准波动率、延迟和阈值。

3. **数字期权公允价值/波动率策略**
   - Up/Down 合约可以视为短期限数字期权，研究剩余期限、距执行价距离和隐含胜率的关系。

4. **CEX–Polymarket Lead-Lag**
   - 若 Binance 数据端点可用，可对齐 CEX 订单流、短期收益和 Polymarket 概率变化。

5. **结算延迟与价格陈旧策略**
   - 比较 end time、resolved_at、结束标的价格和盘口的剩余报价。

6. **跨资产参数迁移**
   - 比较 BTC、ETH、SOL 的信号稳定性，避免只在单一资产上过拟合。

### 4.3 局限性

- 聚焦 Crypto Up/Down，不适合政治、天气、体育和钱包级研究。
- 快照无法完整呈现快照间的成交、撤单和短暂套利机会。
- 没有钱包身份和完整逐笔主动成交带，无法独立进行 Copy Trading 或交易者网络分析。
- 31 天样本适合工程验证和短周期参数研究，不足以证明跨市场状态的长期稳健盈利。

## 5. PolyOrderbooks

### 5.1 提供的数据

PolyOrderbooks 当前主要覆盖加密市场，提供：

- series、events、markets 的发现接口。
- 市场价格历史和 volume、liquidity、spread 等指标。
- 每个 outcome 的完整 L2 订单簿，档位格式为 `[price, size]`。
- resolved winner 和市场关闭信息。
- 原生 1 秒采样，并可返回 1 分钟、5 分钟等更低频率。

需要注意其时间序列构造方式：若某个采样桶内没有新盘口，会使用最近一次观察值填充。因此连续相同记录既可能代表盘口稳定，也可能代表该时间桶没有捕获到新事件。

文档公布的主要套餐示例：

- Starter：免费，7 天历史，约 60 请求/分钟、1000 请求/日。
- Pro：31 天历史，约 300 请求/分钟、50000 请求/日。
- Scale：90 天历史，约 1000 请求/分钟、200000 请求/日。
- Enterprise：自定义。

### 5.2 最适合的策略

1. **广泛加密市场机会扫描**
   - 快速比较不同资产、期限和市场的价差、深度、成交量与流动性状态。

2. **静态 Taker 滑点回测**
   - 对某一快照逐档消耗盘口，计算目标下单金额的 VWAP、滑点和容量。

3. **Binary Complement 的低频筛选**
   - 筛选持续存在而非亚秒级闪现的 UP/DOWN 定价偏差。

4. **流动性状态与策略开关**
   - 按 spread、depth、volume、剩余期限划分市场状态，判断策略应何时开启或暂停。

5. **跨资产概率曲面与相对价值**
   - 比较 BTC、ETH、SOL 等市场在相同期限下的隐含概率和风险溢价。

6. **概率校准与 Brier Score 研究**
   - 使用历史价格路径和最终 winner 检查不同概率区间的实际命中率。

### 5.3 局限性

- 1 秒快照会漏掉亚秒级机会，并且无法知道机会在采样桶内维持了多久。
- 前值填充数据不能被误解为每秒都收到了一次新的真实盘口。
- 缺少逐笔成交、主动方和钱包身份，不适合高保真 Maker、订单流或跟单研究。
- Late-Resolution 需要另接 Chainlink 或其他与结算规则一致的标的价格源。

## 6. 策略与数据源适配矩阵

| 策略 | 首选数据源 | 次选数据源 | 关键说明 |
|---|---|---|---|
| Binary Complement Arbitrage | PMData | PolyBackTest | PMData 更适合事件驱动和腿风险；PolyBackTest 更易做标准化本地回测 |
| Late-Resolution Winner Trading | PMData | PolyBackTest | 必须严格对齐预言机、截止时间、观测延迟和结算规则 |
| Market Making / Shadow Maker | PMData | 无完全等价替代 | 只能做 queue-aware 估计，不能把触价视为成交 |
| Copy Trading | PMData | 官方 Data API + Polygon | 需要钱包、链上成交、可观察延迟和退出规则 |
| 钱包造假/对敲识别 | PMData | Polygon 自建索引 | 需要交易图谱、资金关系和同步行为 |
| CEX–Polymarket Lead-Lag | PolyBackTest（先验证 Binance 接口） | PMData + 外部 CEX | 必须防止不同时间源造成前视偏差 |
| 订单流动量/反转 | PMData | 无完全等价替代 | 需要逐笔方向、盘口事件和短期 markout |
| Taker 滑点/容量 | PMData | PolyOrderbooks、PolyBackTest | 三者均可；PMData 能更细致地处理盘口变化 |
| 概率校准 | PolyOrderbooks | PolyBackTest、PMData | 低频快照和最终结果已足够 |
| 跨加密市场相对价值 | PolyOrderbooks | PMData | 适合广市场和多期限横截面研究 |
| 政治/天气/体育贝叶斯套利 | PMData | 官方 Gamma/CLOB 自建数据 | 另需可靠的父子市场和互斥关系建模 |
| Chainlink/TWAP 边界套利 | PMData | PolyBackTest | PMData 提供 direct 与 TWAP 流，最贴近问题本身 |
| Liquidity Rewards | PMData + 账户奖励审计 | 无 | 行情只是一部分，还要有历史奖励规则和实际奖励到账 |
| Spoofing/撤单异常 | PMData | 无完全等价替代 | 可检测档位行为，但没有订单主体时不能确认操纵者身份 |

## 7. 值得扩展的新策略

### 7.1 CEX–Polymarket 信息领先策略

使用 Binance 现货/永续的短周期收益、订单簿失衡、主动成交量预测 Up/Down 概率的下一次变化。适合 PolyBackTest（确认相应接口可用后），或 PMData 加独立 CEX 数据。

回测必须纳入：

- 两个数据源的时钟偏移与接收时间。
- Polymarket 的真实可执行 ask/bid，而不是中间价。
- REST/WebSocket 决策到下单延迟。
- 信号有效期、手续费、滑点和未成交概率。

### 7.2 短期限数字期权公允价值

用距开盘参考价距离、剩余期限和实时波动率估计 UP 的理论概率，交易市场价格与模型值的偏差。PolyBackTest 的标准化 Up/Down 数据最方便，PMData 可用于提升事件精度。

### 7.3 Oracle/TWAP 窗口套利

根据结算使用的预言机规则，比较即时价格、滚动 TWAP 和市场概率。PMData 的 direct/TWAP 数据最合适，但必须逐个核对市场的正式结算条款，不能假设所有市场使用相同窗口。

### 7.4 订单流毒性与做市动态退出

预测报价后 5 秒、30 秒、60 秒的 adverse markout，在毒性升高时扩大价差、降低库存或暂停报价。PMData 的逐笔成交和增量 L2 是必要条件。

### 7.5 跨资产和跨期限概率曲面

研究 BTC、ETH、SOL 在相同到期周期以及 5m/15m/1h 不同期限之间的概率形状，寻找异常偏离。PolyOrderbooks 适合广范围横截面扫描，PMData 适合确认机会是否可执行。

### 7.6 钱包行为图谱与可跟单性评分

除历史盈利率外，还应考察：成交后的可复制时间窗口、价格冲击、容量、市场类别一致性、资金关联、共同交易钱包、是否依赖 Maker 奖励，以及盈利是否集中在少数事件。PMData 链上成交最适合此类研究。

### 7.7 流动性状态切换

根据 spread、近端深度、撤单率、成交强度、剩余期限和波动率识别市场状态，让其他策略只在适合的状态运行。PolyOrderbooks 适合低频状态建模，PMData 适合事件级状态切换。

## 8. 回测可信度分级

建议给回测报告标注数据和执行模型等级，避免不同精度的结果被直接比较：

| 等级 | 数据与执行模型 | 可以回答的问题 | 不能声称的结论 |
|---|---|---|---|
| A：价格/结果级 | 市场价格、时间、winner | 信号方向、概率校准、理论上限 | 真实可成交收益 |
| B：静态 L2 Taker | 决策时点完整盘口，逐档消耗 | 下单容量、VWAP、滑点后的 Taker 收益 | 快照间机会持续时间、Maker 成交 |
| C：事件驱动 Taker | 增量盘口、成交、源/接收时间、延迟模型 | 腿风险、机会寿命、较真实的执行收益 | 交易所内部精确撮合顺序 |
| D：Queue-aware Maker | C 级数据加排队概率、撤单和 markout 模型 | Shadow Maker 的条件期望和风险 | 确切历史成交、确切 Liquidity Rewards |

任何等级都应使用 walk-forward 或滚动样本外验证，并明确：

- 手续费和历史费率生效时间。
- tick size、最小订单和市场规则变化。
- 决策、网络、REST 和确认延迟。
- 部分成交、两腿失败和退出成本。
- 未结算、异常结算和数据缺口的处理。
- 参数选择是否使用了未来 winner 或结束价格。

## 9. 对当前项目的接入顺序

### 优先级 1：保留并完善 PolyBackTest

- 继续服务 `binary_complement_arbitrage` 和 `late_resolution_winner_trading` 的第一阶段本地回测。
- 按 BTC、ETH、SOL 分开存储资产参数和结果。
- 验证 Binance 相关 v4 端点后，再加入 CEX lead-lag 回测。
- 在报告中明确实际市场数量、时间跨度、缺失率和快照频率。

### 优先级 2：新增 PMData 事件适配器

首批建议接入：

- `l2`
- `trades`
- `onchain_fills`
- `streams_direct`
- `streams_twap60s`

内部统一事件至少应保留：provider、market/token、source timestamp、local timestamp、outcome、side、price、size、event type 和原始文件定位信息。适配器不要过早丢弃供应商原始字段。

### 优先级 3：接入 PolyOrderbooks 做广市场研究

- 用于加密市场发现、流动性排名、概率校准和低频横截面研究。
- 数据记录必须标记原生观察与前值填充，避免把填充桶当成新行情。
- 发现候选后，优先用 PMData 或真实 dry run 再确认执行可行性。

### 优先级 4：重叠样本校验

正式购买或依赖某个数据源前，选择一组相同市场和相同时间窗口进行交叉验证：

- market/token 映射是否一致。
- 最优 bid/ask 和档位数量是否一致。
- 时间戳偏移和缺失事件比例。
- winner、开始/结束时间和标的参考价。
- 数据重复、前值填充、乱序和断线恢复行为。

## 10. 数据源选择建议

- 若目标是**尽快完成当前两项加密策略的第一阶段回测**：继续使用 PolyBackTest。
- 若目标是**提高套利执行真实性或研究 Maker**：优先增加 PMData，构建事件驱动回放。
- 若目标是**低成本扫描大量加密市场和研究概率/流动性横截面**：使用 PolyOrderbooks。
- 若目标是**跟单、钱包质量、对敲或地址关联分析**：三者中只有 PMData 的链上成交数据直接适合。
- 若目标是**精确证明做市历史盈利**：三者都不充分，还需要真实 canary 成交、账户奖励记录和库存最终损益审计。

## 11. 资料来源

### PMData

- [Quick Start](https://test.pmdata.dev/docs)
- [Historical Data Overview](https://test.pmdata.dev/docs/historical-data-details/overview)
- [Full Polymarket Coverage](https://test.pmdata.dev/docs/historical-data-details/polymarket)
- [Polymarket Up/Down Coverage](https://test.pmdata.dev/docs/historical-data-details/polymarket-updown)
- [L2 Schema](https://test.pmdata.dev/docs/schemas/l2)
- [Trades Schema](https://test.pmdata.dev/docs/schemas/trades)
- [On-chain Trades Schema](https://test.pmdata.dev/docs/schemas/onchain-trades)
- [Chainlink Coverage](https://test.pmdata.dev/docs/historical-data-details/chainlink)
- [Chainlink Schema](https://test.pmdata.dev/docs/schemas/chainlink)

### PolyBackTest

- [Available Data](https://docs.polybacktest.com/available-data)
- [Snapshots API](https://docs.polybacktest.com/api-reference/endpoint/get-snapshots)
- [Data Retention](https://docs.polybacktest.com/data-retention)
- [Rate Limits](https://docs.polybacktest.com/rate-limits)

### PolyOrderbooks

- [Documentation](https://docs.polyorderbooks.com/)
- [Historical Data Overview](https://docs.polyorderbooks.com/historical/overview)
- [Historical Order Books](https://docs.polyorderbooks.com/historical/order-books)
- [Pricing and Limits](https://docs.polyorderbooks.com/pricing)
- [Polymarket Crypto Data](https://polyorderbooks.com/polymarket-crypto-data)

> 供应商的覆盖范围、保留周期、限速、价格和 API 版本会变化。开始大规模下载或付费前，应重新核对官方文档，并用少量重叠市场验证实际响应。
