#!/usr/bin/env python3
"""Probe Polymarket crypto up/down markets and rank them for arbitrage suitability.

Usage:
    python3 scripts/probe_markets.py                 # current window, all symbols
    python3 scripts/probe_markets.py --period 15     # 15-minute markets only
    python3 scripts/probe_markets.py --symbols btc,eth,sol,xrp
    python3 scripts/probe_markets.py --once          # single sweep (no follow-up)

What makes a market "worth watching" for the bot:
  1. Direction NOT yet decided  -> outcomePrices both in [0.30, 0.70]
     (a decided market is 0.95/0.05: no tradable spread)
  2. Real liquidity on both sides -> ask layers carry several USD at 0.3-0.7 prices
  3. Tight spread between best bid and best ask
"""
import argparse
import json
import sys
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed

GAMMA = "https://gamma-api.polymarket.com"
CLOB = "https://clob.polymarket.com"
UA = {"User-Agent": "curl/8"}


def get(url, timeout=8):
    req = urllib.request.Request(url, headers=UA)
    return json.loads(urllib.request.urlopen(req, timeout=timeout).read())


def parse_prices(raw):
    """outcomePrices arrives as a JSON-ish string like "['0.495', '0.505']"
    or an actual list; normalize to [float, float]."""
    if raw is None:
        return []
    if isinstance(raw, list):
        return [float(p) for p in raw]
    if isinstance(raw, str):
        import ast
        try:
            parsed = json.loads(raw)
        except json.JSONDecodeError:
            try:
                parsed = ast.literal_eval(raw)
            except (ValueError, SyntaxError):
                return []
        return [float(p) for p in parsed] if isinstance(parsed, list) else []
    return []


def parse_tokens(raw):
    """clobTokenIds arrives as a JSON string; normalize to a list of ids."""
    if raw is None:
        return []
    if isinstance(raw, list):
        return raw
    if isinstance(raw, str):
        try:
            parsed = json.loads(raw)
        except json.JSONDecodeError:
            return []
        return parsed if isinstance(parsed, list) else []
    return []


def probe(symbol, period_minutes, timestamp):
    secs = period_minutes * 60
    slug = f"{symbol}-updown-{period_minutes}m-{timestamp}"
    row = {"symbol": symbol, "period": f"{period_minutes}m", "slug": slug}
    try:
        market = get(f"{GAMMA}/markets?slug={slug}")[0]
    except Exception:
        row["status"] = "missing"
        return row

    prices = market.get("outcomePrices") or []
    row["prices"] = prices
    row["liquidity"] = round(float(market.get("liquidity") or 0), 0)
    row["volume"] = round(float(market.get("volume") or 0), 1)
    row["end"] = market.get("endDate", "")[11:16]

    # Both outcomes priced 0.30-0.70 -> direction not yet decided
    try:
        lo, hi = sorted(parse_prices(prices))
        row["decided"] = not (0.30 <= lo and hi <= 0.70)
    except (ValueError, TypeError):
        row["decided"] = True

    # Order book depth on both tokens (concurrent, bounded by 4s each).
    # Only asks below 0.85 count as "real" liquidity — 0.9x asks are guard orders
    # that sit at prices with no arbitrage meaning.
    row["ask_depth"] = []
    toks = parse_tokens(market.get("clobTokenIds"))[:2]

    def book_depth(tok):
        try:
            book = get(f"{CLOB}/book?token_id={tok}", timeout=4)
            asks = [a for a in book.get("asks", []) if float(a["price"]) < 0.85]
            depth1 = float(asks[0]["price"]) * float(asks[0]["size"]) if asks else 0.0
            depth5 = sum(float(a["price"]) * float(a["size"]) for a in asks[:5])
            best_ask = asks[0]["price"] if asks else None
            return (best_ask, round(depth1, 2), round(depth5, 2))
        except Exception:
            return (None, 0.0, 0.0)

    with ThreadPoolExecutor(max_workers=2) as ex:
        row["ask_depth"] = list(ex.map(book_depth, toks))
    return row


def fmt(row):
    if row.get("status") == "missing":
        return f"  {row['period']:<5} {row['symbol']:<6} ❌ 市场不存在"
    decided = "方向已定" if row["decided"] else "方向未定"
    flag = ""
    if "ask_depth" in row:
        depths = "  ".join(
            f"ask={a} 1层${d1:.1f}/5层${d5:.1f}" if a else "asks空"
            for a, d1, d5 in row["ask_depth"]
        )
        if not row["decided"] and min((d1 for _, d1, _ in row["ask_depth"]), default=0) >= 3:
            flag = " ⭐"
        return (
            f"  {row['period']:<5} {row['symbol']:<6} 价格={row['prices']} "
            f"流动性={row['liquidity']:.0f} 量={row['volume']} 到期={row['end']} [{decided}]"
            f"\n        {depths}{flag}"
        )
    return (
        f"  {row['period']:<5} {row['symbol']:<6} 价格={row['prices']} "
        f"流动性={row['liquidity']:.0f} 量={row['volume']} 到期={row['end']} [{decided}]"
    )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--period", type=int, default=None, help="5 or 15 (default: both)")
    ap.add_argument("--symbols", default="btc,eth")
    ap.add_argument("--once", action="store_true", help="single sweep, no auto-refresh")
    ap.add_argument("--gamma-only", action="store_true", help="skip CLOB book queries (fast)")
    args = ap.parse_args()

    symbols = [s.strip() for s in args.symbols.split(",") if s.strip()]
    periods = [args.period] if args.period else [5, 15]

    def probe_limited(sym, period, ts):
        if args.gamma_only:
            row = {"symbol": sym, "period": f"{period}m"}
            slug = f"{sym}-updown-{period}m-{ts}"
            try:
                m = get(f"{GAMMA}/markets?slug={slug}")[0]
                prices = parse_prices(m.get("outcomePrices"))
                try:
                    lo, hi = sorted(prices)
                    decided = not (0.30 <= lo and hi <= 0.70)
                except Exception:
                    decided = True
                row.update(
                    prices=prices, decided=decided,
                    liquidity=round(float(m.get("liquidity") or 0), 0),
                    volume=round(float(m.get("volume") or 0), 1),
                    end=m.get("endDate", "")[11:16], status="ok",
                )
            except Exception:
                row["status"] = "missing"
            return row
        return probe(sym, period, ts)

    while True:
        now = int(time.time())
        print(f"\n=== {time.strftime('%Y-%m-%d %H:%M:%S UTC', time.gmtime(now))} ===")
        for period in periods:
            ts = (now // (period * 60)) * (period * 60)
            print(f"\n[{period}m 窗口 {time.strftime('%H:%M', time.gmtime(ts))}-{time.strftime('%H:%M', time.gmtime(ts + period * 60))}]")
            with ThreadPoolExecutor(max_workers=len(symbols)) as ex:
                futures = [ex.submit(probe_limited, sym, period, ts) for sym in symbols]
                for fut in as_completed(futures):
                    try:
                        print(fmt(fut.result()))
                    except Exception as e:
                        print(f"  {period}m: 查询异常 {e}")
        if args.once:
            break
        print("\n--- 30s 后刷新（Ctrl+C 退出）---")
        time.sleep(30)


if __name__ == "__main__":
    main()
