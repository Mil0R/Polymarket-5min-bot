"""Finite-capital SDDFV scenario replay. No network, credentials or real orders."""
import collections
import datetime as dt
import heapq
import math
import random

DEFAULTS = dict(initial_balance=1000.0, max_exposure=20.0, max_asset_exposure=10.0,
                settlement_delay_ms=120000, bootstrap_draws=2000)


def validate(policy):
    for key in ("initial_balance", "max_exposure", "max_asset_exposure"):
        if not math.isfinite(policy[key]) or policy[key] <= 0:
            raise ValueError("invalid_portfolio_budget/"+key)
    if policy["max_asset_exposure"] > policy["max_exposure"] or policy["max_exposure"] > policy["initial_balance"]:
        raise ValueError("invalid_portfolio_exposure_limits")
    if policy.get("budget",5.0)>policy["max_asset_exposure"]:
        raise ValueError("per_order_budget_exceeds_asset_limit")
    if policy["settlement_delay_ms"] < 0 or policy["bootstrap_draws"] < 100:
        raise ValueError("invalid_portfolio_release_or_bootstrap")


def replay(trades, policy=None, asof_ms=None):
    """Merge BTC/ETH at real delayed-book times; never add alternative candidates.

    Cash release is MODELLED at end+delay, not a claim of redeemable chain funds.
    Unresolved positions at an as-of boundary stay locked. No instant recycling.
    """
    policy = dict(DEFAULTS, **(policy or {}))
    validate(policy)
    cash, locked, peak, drawdown, max_locked = policy["initial_balance"], 0.0, policy["initial_balance"], 0.0, 0.0
    asset_locked = collections.defaultdict(float)
    pending, accepted, rejected, events, seen = [], [], collections.Counter(), [], set()

    def settle_until(now):
        nonlocal cash, locked, peak, drawdown
        while pending and pending[0][0] <= now:
            at = pending[0][0]
            # Same-time cross-asset settlements are one block, not arbitrary
            # BTC-then-ETH fluctuations in an invented NAV curve.
            while pending and pending[0][0] == at:
                _, _, t = heapq.heappop(pending)
                spent = t["cost"]+t["fees"]
                cash += spent+t["settlement_pnl"]
                locked -= spent
                asset_locked[t["asset"]] -= spent
            nav = cash+locked
            peak = max(peak, nav)
            drawdown = max(drawdown, peak-nav)
            events.append(dict(event="modeled_cash_release", at_ms=at, cash=cash, locked_cost=locked, nav_cost_basis=nav))

    ordered = sorted(trades, key=lambda t:(t["entry_ms"], t["asset"], t["slug"]))
    for t in ordered:
        if asof_ms is not None and t["entry_ms"] > asof_ms:
            continue
        settle_until(t["entry_ms"])
        key = (t["asset"], t["slug"])
        if key in seen:
            raise ValueError("duplicate_portfolio_market/"+t["slug"])
        seen.add(key)
        spent = t["cost"]+t["fees"]
        if not all(math.isfinite(t[k]) for k in ("cost", "fees", "settlement_pnl")) or spent <= 0:
            raise ValueError("invalid_trade_cashflow")
        if spent > policy.get("budget", 5.0)+1e-8:
            raise ValueError("trade_exceeds_per_order_budget")
        reason = ("cash_insufficient" if spent > cash+1e-9 else
                  "total_exposure_exceeded" if locked+spent > policy["max_exposure"]+1e-9 else
                  "asset_exposure_exceeded" if asset_locked[t["asset"]]+spent > policy["max_asset_exposure"]+1e-9 else None)
        if reason:
            rejected[reason] += 1
            events.append(dict(event="paper_order_rejected", at_ms=t["entry_ms"], slug=t["slug"], reason=reason))
            continue
        cash -= spent
        locked += spent
        asset_locked[t["asset"]] += spent
        max_locked = max(max_locked, locked)
        accepted.append(t)
        release = t["window_end_ms"]+policy["settlement_delay_ms"]
        if release < t["entry_ms"]:
            raise ValueError("cash_release_before_entry")
        heapq.heappush(pending, (release, len(accepted), t))
        events.append(dict(event="paper_cash_reserved", at_ms=t["entry_ms"], slug=t["slug"],
                           cost_with_fees=spent, cash=cash, locked_cost=locked, nav_cost_basis=cash+locked))
    settle_until(asof_ms if asof_ms is not None else math.inf)
    return dict(policy=policy, fills=len(accepted), independent_windows=len({t["window_end_ms"] for t in accepted}),
                cash=cash, locked_cost=max(0.0,locked), unresolved_positions=len(pending),
                nav_cost_basis=cash+locked, realized_pnl=cash+locked-policy["initial_balance"],
                final_settlement_pnl=sum(t["settlement_pnl"] for t in accepted),
                max_realized_drawdown=drawdown, peak_locked_cost=max_locked,
                rejected=dict(rejected), trades=accepted, events=events,
                settlement_release="modelled end+delay; not actual redeem/chain confirmation",
                valuation="pending inventory at entry cost; NOT executable mark-to-market",
                simulated_only=True)


def daily_interval(trades, draws=2000):
    """UTC-day block bootstrap, preserving within-day and cross-asset correlation.

    Exploratory interval only: few days, repeated reviews and fat tails are not
    repaired by increasing Monte Carlo draws. Includes zero-fill covered days
    when caller supplies them through summarize().
    """
    daily = collections.defaultdict(float)
    for t in trades:
        day = dt.datetime.fromtimestamp((t["window_end_ms"]-300000)/1000, dt.timezone.utc).date().isoformat()
        daily[day] += t["settlement_pnl"]
    return interval_values(list(daily.values()), draws), dict(sorted(daily.items()))


def interval_values(values, draws=2000):
    if len(values) < 5:
        return dict(available=False, blocks=len(values), reason="fewer_than_five_daily_blocks")
    rng = random.Random(9)
    samples = sorted(sum(rng.choice(values) for _ in values)/len(values) for _ in range(draws))
    return dict(available=True, blocks=len(values), mean_daily_pnl=sum(values)/len(values),
                lower=samples[int(draws*.025)], upper=samples[min(draws-1,int(draws*.975))],
                method="exploratory UTC-day block bootstrap; not proof of positive expectation")


def summarize(trades, rows, policy=None):
    account = replay(trades, policy)
    funded = account.pop("trades")
    ledger_events = account.pop("events")
    interval, daily = daily_interval(funded, (policy or DEFAULTS).get("bootstrap_draws",2000))
    coverage = collections.defaultdict(set)
    for r in rows:
        day = dt.datetime.fromtimestamp(r["time"], dt.timezone.utc).date().isoformat()
        coverage[day].add(r["time"]%86400)
    full_days = [day for day,times in coverage.items() if 0 in times and 86100 in times and len(times)>=274]
    # Main interval uses only approximately complete days, including zero-fill
    # days. Partial first/last days stay visible but do not pad the gate.
    full_interval = interval_values([daily.get(day,0.0) for day in sorted(full_days)],
                                   (policy or DEFAULTS).get("bootstrap_draws",2000))
    total = sum(t["cost"]+t["fees"] for t in funded)
    pnl = sum(t["settlement_pnl"] for t in funded)
    winners = sorted((t["settlement_pnl"] for t in funded if t["settlement_pnl"]>0),reverse=True)
    calibration = collections.defaultdict(list)
    for t in funded:
        q=t["predicted_probability"]
        calibration[str(min(9,int(q*10)))].append((q,int(t["settlement_pnl"]>0)))
    result = dict(account=account, fills=len(funded), cost=total, pnl=pnl, roi=pnl/total if total else None,
                  nav_return=pnl/account["policy"]["initial_balance"],
                  fees=sum(t["fees"] for t in funded), daily_pnl=daily,
                  full_covered_days=sorted(full_days), full_day_pnl_interval=full_interval,
                  all_observed_daily_interval=interval,
                  concentration=dict(largest_win=winners[0] if winners else 0.0,
                                     pnl_excluding_largest_win=pnl-(winners[0] if winners else 0.0),
                                     note="stress diagnostic, NOT an unbiased profit estimate"),
                  trade_calibration={k:dict(samples=len(v),mean_probability=sum(q for q,y in v)/len(v),
                                          win_rate=sum(y for q,y in v)/len(v)) for k,v in sorted(calibration.items())})
    return result, funded, ledger_events
