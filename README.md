# Polymarket-5min-bot

**English** | [中文](./README.zh-CN.md)

**Official Website**: [polypulse.wiki](https://polypulse.wiki/)

> Rust arbitrage bot for [Polymarket](https://polymarket.com) crypto “Up or Down” 5-minute prediction markets.

![account](image/account.png)

![Dashboard](image/main.png)

## How It Works

Polymarket Up/Down markets open a new 5-minute window (UTC) every cycle. Each market has YES and NO outcome tokens.

In theory, holding equal amounts of YES + NO redeems for 1 USDC at settlement, so:

```
YES best ask + NO best ask < 1  →  arbitrage opportunity
```

The bot roughly follows these steps:

1. **Market discovery** — Finds current 5-minute Up/Down markets for configured symbols (e.g. btc, eth).
2. **Order book monitoring** — Subscribes to CLOB order books and tracks YES + NO combined price in real time.
3. **Arbitrage execution** — When the combined price is below the threshold, submits the thinner best-ask leg first, freezes and reconciles its fill, then sizes the opposite leg from that confirmed fill. Slippage, size limits, and execution spread are configurable.
4. **Merge** — When holding both YES and NO, merges on-chain into USDC/pUSD to reduce position risk.
5. **Wind-down** — Near window end, can auto-cancel orders, merge, and market-sell remaining single-leg positions.

> This bot connects to live markets and real funds. Understand the risks before use.

## Quick Start

### Pre-built binary

If you don't want to compile from source, use the pre-built executable from **[Releases](../../releases/latest)**:

1. Download the package for your OS (Linux / Windows)
2. Copy `.env.example` to `.env` and fill in required fields
3. Run:
   - Linux / macOS: `./polypulse`
   - Windows: `polypulse.exe`

### Build from source

Requires [Rust](https://rustup.rs).

```bash
cp .env.example .env   # fill required fields in .env, then run
cargo run
```

Read-only online soak test (30 minutes by default; accepts seconds):

```bash
./scripts/dry_run_soak.sh 1800
```

See `.env.example` for full options, grouped as `[1]`–`[9]`: earlier sections are more important.

## Configuration

### Required

| Variable | Description |
|----------|-------------|
| `POLYMARKET_PRIVATE_KEY` | Signer private key. Email/Magic: [reveal.magic.link/polymarket](https://reveal.magic.link/polymarket); browser wallet: export your EOA key |
| `POLYMARKET_PROXY_ADDRESS` | Funder address from Settings (not your EOA) — [polymarket.com/settings](https://polymarket.com/settings) |

### Signature type `SIGNATURE_TYPE`

Choose based on **funder wallet type in Settings**, not whether you registered with email or a browser wallet:

| Value | When to use |
|-------|-------------|
| `Poly1271` (**default**) | V2 deposit wallet — email/Magic and browser-wallet accounts |
| `Proxy` | Legacy V1 Magic proxy only (Settings address must match ProxyFactory CREATE2 derive from your EOA) |
| `GnosisSafe` | Gnosis Safe multisig |
| `Eoa` | Direct EOA trading — no `POLYMARKET_PROXY_ADDRESS` needed |

**Rule of thumb:** keep the default `Poly1271`. If `Proxy` orders fail with `please use the deposit wallet flow`, your account uses the V2 deposit wallet — switch back to `Poly1271` (no need to change private key or proxy address).

### Required for Merge (when scheduled Merge or wind-down is enabled)

| Variable | Description |
|----------|-------------|
| `POLY_BUILDER_API_KEY` | Builder API key |
| `POLY_BUILDER_SECRET` | Builder API secret |
| `POLY_BUILDER_PASSPHRASE` | Builder API passphrase |

Get these from Polymarket → Settings → Builder.

### Common options

| Variable | Default | Description |
|----------|---------|-------------|
| `CRYPTO_SYMBOLS` | `btc,eth,sol,xrp` | Symbols to monitor, comma-separated |
| `ARBITRAGE_EXECUTION_SPREAD` | `0.01` | Execute when `yes + no <= 1 - spread` |
| `MAX_ORDER_SIZE_USDC` | `100.0` | Maximum notional per leg, converted to shares at the final limit price |
| `RISK_MAX_EXPOSURE_USDC` | `1000.0` | Maximum account-wide position and pending-order exposure |
| `ARBITRAGE_FEE_BUFFER` | `0.002` | Fee/rounding buffer required after slippage |
| `DRY_RUN` | `false` | Use live read-only data and simulated fills without account writes |
| `CANARY_MODE` | `false` | Tiny live-money hard gate; live canary requires FOK/FAK |
| `CANARY_MAX_PAIR_NOTIONAL_USDC` / `CANARY_MAX_TOTAL_NOTIONAL_USDC` | `3.0` / `3.0` | Per-pair and process-total planned notional limits |
| `CANARY_MAX_LOSS_USDC` | `2.0` | Conservative downside budget; permanently pauses entries for the process when reached |
| `CANARY_MAX_PAIRS_PER_WINDOW` / `CANARY_MAX_PAIRS_TOTAL` | `1` / `1` | Hard pair-count limits per window and process |
| `WINNER_BUY_MAX_STRATEGY_LOSS_USDC` / `WINNER_BUY_MAX_ASSET_LOSS_USDC` | `20.0` / `10.0` | Persisted Winner Buy realized-loss circuit breakers, globally and per asset |
| `WINNER_BUY_MAX_CONSECUTIVE_LOSSES` | `3` | Persisted consecutive-loss circuit breaker; `0` disables it |
| `WINNER_BUY_AUTO_REDEEM` | `false` | Automatically redeem confirmed winning tokens in live mode; requires proxy/builder credentials |
| `WINNER_BUY_RTDS_URL` / `WINNER_BUY_FEED_MAX_AGE_MS` | official RTDS / `5000` | Chainlink 60-second TWAP source and fail-closed freshness limit |
| `WINNER_BUY_CONFIRMATION_OBSERVATIONS` / `WINNER_BUY_MAX_ATTEMPTS_PER_MARKET` | `3` / `1` | Distinct Chainlink updates required and finite per-market execution retries |
| `WINNER_BUY_MIN_MARGIN_BPS` / `WINNER_BUY_REVERSAL_SIGMA` | `3.0` / `2.0` | Directional TWAP margin and remaining-time reversal boundary |
| `WINNER_BUY_MAX_SPREAD` / `WINNER_BUY_MIN_DEPTH_USDC` | `0.03` / `10.0` | Selected outcome book-quality filters |
| `WINNER_BUY_MIN_NET_EV` | `0.002` | Minimum `p_model - effective_price - model_risk_buffer` |
| `WINNER_BUY_HYBRID_PAPER_ENABLED` | `false` | PAPER-only research path for price `0.96–0.98`, positive winner payoff, Chainlink-direction agreement, and normal spread/depth/confirmation gates; live mode rejects this setting |
| `ARBITRAGE_ENABLED` / `WINNER_BUY_ENABLED` | `true` / `false` | Initial strategy states; the TUI can pause/resume entries for the current process |
| `PAPER_INITIAL_BALANCE_USDC` | `50.0` | Independent virtual cash budget for Dry-run |
| `PAPER_LIQUIDITY_HAIRCUT` | `0.75` | Fraction of displayed depth treated as executable in paper fills |
| `PAPER_SECOND_LEG_DELAY_MS` | `250` | Delay before fetching a fresh live book for the opposite leg |
| `PAPER_WINNER_BUY_DELAY_MS` | `250` | Dry-run latency before Winner Buy re-reads the live book and simulates the fill |
| `MAX_BOOK_AGE_MS` / `MAX_BOOK_SKEW_MS` | `2000` / `1000` | Reject stale or mutually skewed leg snapshots |
| `SIGNAL_COOLDOWN_MS` | `3000` | Cooldown for identical market/price signals |
| `ARBITRAGE_STATE_JOURNAL_FILE` | `results/spread-arbitrage/{paper|live}/orders.ndjson` | Arbitrage order-pair/recovery journal; legacy `STATE_JOURNAL_FILE` remains supported |
| `WINNER_BUY_STATE_JOURNAL_FILE` | `results/winner-buy/{paper|live}/orders.ndjson` | Winner Buy order, fill, and settlement journal |
| `WINNER_BUY_EVALUATION_JOURNAL_FILE` | `results/winner-buy/{paper|live}/evaluations.ndjson` | Winner Buy candidate and counterfactual evaluations |
| `ARBITRAGE_SHADOW_LOG_FILE` | unset | Optional NDJSON research log for price-positive candidates rejected by depth; never changes admission |
| `LOG_FILE` | `logs/system.log` in TUI mode | System log; strategy events move to their configured strategy file without duplication |
| `ARBITRAGE_STRATEGY_LOG_FILE` / `WINNER_BUY_STRATEGY_LOG_FILE` | unset | Optional mutually exclusive per-strategy text logs |
| `MERGE_INTERVAL_MINUTES` | `0` | Scheduled Merge interval (minutes); `0` = disabled |
| `WIND_DOWN_BEFORE_WINDOW_END_MINUTES` | `0` | Wind-down before window end (minutes); `0` = disabled |
| `RUST_LOG` | `info` | Log level |
| `LOG_ROTATE_MAX_BYTES` | `10485760` | Runtime size-rotation threshold per log file |
| `LOG_ROTATE_FILES` | `5` | Number of numbered log backups to retain |
| `OBSERVABILITY_BIND` | unset | Optional `/livez`, `/readyz`, compatible `/healthz`, and Prometheus `/metrics` bind address |

Other settings (CLOB URL, signature type, slippage, order type, position sync, etc.) have sensible defaults. See `.env.example` for the full list with bilingual comments.

Startup reconciles cumulative CLOB fills and Data API positions, then restores manageable recovery tasks. New journal records carry a version and SHA-256 checksum. A crash-damaged final line is isolated to a `.corrupt-*` file while the valid prefix is retained; corruption in the middle still blocks startup. Legacy records without enough context block live startup by default. `ALLOW_UNRESOLVED_STATE=true` only permits the reconciliation/recovery process to run; unresolved state still blocks new live entries.

For the first tiny live validation, use `DRY_RUN=false`, `CANARY_MODE=true`, and `ARBITRAGE_ORDER_TYPE=FAK`. `/readyz` returns 200 only when initialization, market/user-stream connectivity, journal writability, disk capacity, unresolved-order state, and the process-wide live safety gate are all healthy. Any ambiguous CLOB write pauses new entries across windows and requires a restart plus account/order reconciliation; it cannot be cleared in-process.

With `DRY_RUN=true`, paper execution walks the real multi-level CLOB book for the harder leg, waits `PAPER_SECOND_LEG_DELAY_MS`, fetches a fresh live book for the opposite leg, and applies FOK/FAK/immediate-fill semantics to the confirmed virtual first-leg amount. It never restores real recovery tasks, syncs real positions, places/cancels orders, merges, or sells. Prometheus exposes paper pair/one-sided/PnL metrics, CLOB connect-failure/reconnect/recovery counters, and the Winner Buy scan → signal → model rejection/confirmation → attempt → fill → pending → resolved → redeemed funnel. The TUI keeps estimated winner payout separate from realized settlement PnL.

### Strategy development stages

Strategies now follow a strict local backtest → live-data dry-run → interactive
TUI live-promotion workflow. Spread Arbitrage and Winner Buy support both
`backtest` and forced-safe standalone `dry-run` commands. Copy Trading starts at
live-data PAPER because wallet history is absent from PolyBacktest; Maker and
Bayesian research likewise start with read-only `shadow` mode. Standalone
executables cannot enter real execution, and live/canary `polypulse` requires an
interactive TUI terminal.

See [the Chinese stage guide](docs/STRATEGY_DEVELOPMENT_STAGES.zh-CN.md) for the
strategy/data compatibility matrix, commands, result paths, and promotion gates.

### PolyBacktest historical downloader

The independent downloader persists resolved crypto Up/Down markets for offline
Spread Arbitrage and Winner Buy replay. It stores an auditable Zstandard copy of
each API page plus a WAL-enabled SQLite catalog with scalar signals, top of book,
and compressed full books. Completed pages are transactional and idempotent, so
rerunning after Ctrl-C or a crash resumes at the first missing offset.

```bash
cargo run --release --bin polybacktest_downloader -- sync
cargo run --release --bin polybacktest_downloader -- watch
cargo run --release --bin polybacktest_downloader -- status
```

Free Plan defaults enforce at least 1100ms between request starts. See
[`docs/POLYBACKTEST_DOWNLOADER.zh-CN.md`](docs/POLYBACKTEST_DOWNLOADER.zh-CN.md)
for storage schema, recovery behavior, and operational guidance.

## Disclaimer

This software is for learning and research only — not investment advice. Crypto and prediction markets carry significant risk, including loss of funds. Assess your own risk and comply with Polymarket’s terms and applicable laws.
