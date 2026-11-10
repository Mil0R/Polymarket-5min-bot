#!/usr/bin/env python3
"""Read-only winner-model diagnostics. No network, fitting, orders or plan changes.

Select the earliest model observation per market BEFORE looking at labels.
Multiple ticks/directions never become independent calibration observations.
Optional reference rows contain slug, opening_twap, closing_twap,
opening_observed_at_ms, closing_observed_at_ms, window_start_ms, end_ms and
reference_topic='crypto_prices_twap_sixty'. Only exact boundary samples verify
the reference; interpolation/nearest-neighbour samples are not ground truth.
"""
import argparse
import collections
import datetime as dt
import hashlib
import json
import math
from pathlib import Path


def load(paths):
    evaluations, labels, inputs = {}, {}, []
    for path in paths:
        raw = Path(path).read_bytes()
        inputs.append(dict(path=str(path), sha256=hashlib.sha256(raw).hexdigest()))
        for index, line in enumerate(raw.splitlines(), 1):
            if not line.strip():
                continue
            try:
                event = json.loads(line)
            except ValueError as error:
                raise ValueError(f"{path}:{index}: incomplete/corrupt journal") from error
            if event.get("action") in ("evaluated", "resolved") and event.get("version") != 1:
                raise ValueError("unsupported evaluation journal version")
            if event.get("action") == "evaluated":
                e = event["evaluation"]
                key = e["evaluation_id"]
                if key in evaluations and evaluations[key] != e:
                    raise ValueError("conflicting evaluation: " + key)
                evaluations[key] = e
            elif event.get("action") == "resolved":
                key = event["evaluation_id"]
                if key in labels and labels[key]["won"] != event["won"]:
                    raise ValueError("conflicting label: " + key)
                labels[key] = event
    return evaluations, labels, inputs


def number(value):
    try:
        value = float(value)
    except (ValueError, TypeError):
        return None
    return value if math.isfinite(value) else None


def instant(value):
    return dt.datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()


def select(evaluations):
    selected = {}
    for e in evaluations.values():
        p = number(e.get("model_probability"))
        if (p is None or not 0 <= p <= 1 or e.get("opening_twap") is None
                or e.get("current_twap") is None or e.get("seconds_remaining", 0) <= 0
                or instant(e["evaluated_at"]) >= instant(e["end_date"])):
            continue
        key = (e["asset"].lower(), e["slug"])
        if key not in selected or (instant(e["evaluated_at"]), e["evaluation_id"]) < (
                instant(selected[key]["evaluated_at"]), selected[key]["evaluation_id"]):
            selected[key] = e
    return list(selected.values())


def stats(rows):
    n = len(rows)
    if not n:
        return dict(n=0)
    wins = sum(row["y"] for row in rows)
    rate = wins / n
    z2 = 1.959963984540054 ** 2
    lower = (rate + z2 / (2*n) - math.sqrt(z2 * (rate*(1-rate)/n + z2/(4*n*n)))) / (1+z2/n)
    mean_p = sum(row["p"] for row in rows) / n
    return dict(n=n, wins=wins, losses=n-wins, mean_probability=mean_p,
                observed_win_rate=rate, overprediction=mean_p-rate,
                brier=sum((r["p"]-r["y"])**2 for r in rows)/n,
                wilson_95_lower_descriptive=lower,
                volatility_floor_fraction=sum(bool((r["e"].get("model_audit") or {}).get("volatility_floor_active")) for r in rows)/n)


def report(evaluations, labels, references=(), gamma_references=None):
    selected = select(evaluations)
    rows, missing = [], 0
    # Check label consistency across opposite directions within a market.
    market_labels = {}
    for key, label in labels.items():
        if key not in evaluations:
            continue
        e = evaluations[key]
        if not isinstance(label.get("won"), bool) or instant(label["resolved_at"]) < instant(e["end_date"]):
            raise ValueError("invalid/premature settlement label")
        up = label["won"] if e["buy_yes"] else not label["won"]
        previous = market_labels.setdefault(e["slug"], up)
        if previous != up:
            raise ValueError("conflicting market directions: " + e["slug"])
    for e in selected:
        label = labels.get(e["evaluation_id"])
        if label is None:
            missing += 1
            continue  # Do not substitute a later labelled tick.
        rows.append(dict(e=e, p=float(e["model_probability"]), y=int(label["won"])))
    groups = collections.defaultdict(list)
    for row in rows:
        e = row["e"]
        pbin = "<0.90" if row["p"] < .9 else "0.90-0.97" if row["p"] < .97 else "0.97-0.99" if row["p"] < .99 else ">=0.99"
        skew = (e.get("model_audit") or {}).get("opening_skew_ms")
        skew_bin = "unknown" if skew is None else "<=1s" if abs(skew) <= 1000 else ">1s"
        horizon = "<=10s" if e["seconds_remaining"] <= 10 else "11-20s" if e["seconds_remaining"] <= 20 else ">20s"
        for group in ("asset/"+e["asset"], "probability/"+pbin, "opening_skew/"+skew_bin,
                      "asset_horizon/"+e["asset"]+"/"+horizon):
            groups[group].append(row)
    verified, mismatch, residuals = 0, [], collections.defaultdict(list)
    for ref in references:
        if (ref.get("reference_topic") != "crypto_prices_twap_sixty"
                or ref["opening_observed_at_ms"] != ref["window_start_ms"]
                or ref["closing_observed_at_ms"] != ref["end_ms"]):
            continue
        opening, closing = number(ref["opening_twap"]), number(ref["closing_twap"])
        if opening is None or closing is None or opening <= 0 or closing <= 0 or ref["slug"] not in market_labels:
            continue
        # Ties need the market's explicit equality rule; do not invent a label.
        if closing == opening:
            continue
        verified += 1
        if (closing > opening) != market_labels[ref["slug"]]:
            mismatch.append(ref["slug"])
        for row in rows:
            e = row["e"]
            if e["slug"] == ref["slug"]:
                residuals[e["asset"]+"/"+e.get("seconds_remaining_bucket", "legacy")].append(
                    (closing-float(e["current_twap"]))/opening*10000)
    def quantiles(values):
        values = sorted(values)
        return dict(n=len(values), **{str(q): values[round(q*(len(values)-1))] for q in (.01,.05,.5,.95,.99)})
    gamma_errors, gamma_local_sign_mismatches, gamma_label_mismatches = [], [], []
    gamma_residuals = collections.defaultdict(list)
    for row in rows:
        e = row["e"]
        ref = (gamma_references or {}).get(e["slug"], {})
        metadata = ref.get("eventMetadata") or {}
        opening, closing = number(metadata.get("priceToBeat")), number(metadata.get("finalPrice"))
        description = ref.get("description", "")
        if (ref.get("closed") is not True or "Chainlink" not in description or "TWAP" not in description
                or opening is None or closing is None or min(opening, closing) <= 0
                or not ref.get("fetched_at") or instant(ref["fetched_at"]) < instant(e["end_date"])):
            continue
        local_opening = float(e["opening_twap"])
        if local_opening <= 0:
            continue
        gamma_errors.append((local_opening/opening-1)*10000)
        official_up = closing >= opening  # Explicit Up >= rule checked below.
        if "greater than or equal" not in description:
            gamma_errors.pop()
            continue
        if official_up != market_labels[e["slug"]]:
            gamma_label_mismatches.append(e["slug"])
        if (closing >= local_opening) != market_labels[e["slug"]]:
            gamma_local_sign_mismatches.append(e["slug"])
        gamma_residuals[e["asset"]+"/"+e.get("seconds_remaining_bucket", "legacy")].append(
            (closing-float(e["current_twap"]))/opening*10000)
    return dict(schema_version=1, purpose="development_diagnostics_only",
                selection="earliest eligible observation per asset/market; includes rejected/unfilled observations",
                selected=len(selected), missing_selected_labels=missing, overall=stats(rows),
                groups={key:stats(value) for key,value in sorted(groups.items())},
                opening_reference=dict(exact_boundary_verified=verified, mismatches=mismatch,
                    status="requires_exact_boundary_TWAP_capture" if not verified else "diagnostic_only"),
                empirical_terminal_residual_bps={key:quantiles(value) for key,value in sorted(residuals.items())},
                gamma_metadata_reference=dict(n=len(gamma_errors),
                    opening_error_bps=quantiles(gamma_errors) if gamma_errors else None,
                    max_abs_opening_error_bps=max(map(abs,gamma_errors), default=None),
                    official_sign_vs_label_mismatches=gamma_label_mismatches,
                    logged_opening_final_sign_vs_label_mismatches=gamma_local_sign_mismatches,
                    terminal_residual_bps={key:quantiles(value) for key,value in sorted(gamma_residuals.items())},
                    evidence="cached Gamma priceToBeat/finalPrice with explicit Chainlink TWAP and Up >= rule; not a raw boundary-feed capture"),
                limitations=["No model or risk parameter is changed.",
                    "Wilson intervals are descriptive; correlated market/day regimes require grouped forward validation.",
                    "Outcome-only journals cannot verify exact opening TWAP or estimate terminal residuals.",
                    "A new fitting/calibration study needs chronological splits and settlement-label embargoes."])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("journals", nargs="+", type=Path)
    parser.add_argument("--references", type=Path)
    parser.add_argument("--gamma-reference-audit", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    evaluations, labels, inputs = load(args.journals)
    refs = [] if args.references is None else [json.loads(line) for line in args.references.read_text().splitlines() if line.strip()]
    gamma = {} if args.gamma_reference_audit is None else json.loads(args.gamma_reference_audit.read_text())
    result = report(evaluations, labels, refs, gamma)
    result["inputs"] = inputs
    if args.references:
        result["reference_sha256"] = hashlib.sha256(args.references.read_bytes()).hexdigest()
    if args.gamma_reference_audit:
        result["gamma_reference_input"] = dict(path=str(args.gamma_reference_audit),
            sha256=hashlib.sha256(args.gamma_reference_audit.read_bytes()).hexdigest())
    # Refuse to overwrite a study/report silently; never modify input journals.
    with args.output.open("x") as output:
        json.dump(result, output, indent=2, ensure_ascii=False)
        output.write("\n")


if __name__ == "__main__":
    main()
