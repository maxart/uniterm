#!/usr/bin/env python3
"""Summarize intact ut-compare reports without pooling samples or editing evidence."""
import csv
import hashlib
import json
import math
import statistics
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
CORE = ['server_startup_ready', 'control_command_latency', 'daemon_idle_cohort_cpu',
        'daemon_idle_cohort_rss', 'foreground_idle_cohort_cpu',
        'foreground_idle_cohort_rss', 'terminal_input_to_visible', 'terminal_output_completion']
import argparse
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--baseline', type=Path)
parser.add_argument('reports', nargs='+', type=Path)
args = parser.parse_args()
paths = args.reports
assert paths, 'Supply completed raw report paths'
runs = [json.loads(p.read_text()) for p in paths]
first = runs[0]
ids = [c['id'] for c in first['contenders']]
maps = [{c['id']: {m['name']: m for m in c['benchmarks']} for c in r['contenders']} for r in runs]
for r in runs:
    assert r['schema_version'] == 7 and r['profile'] == first['profile']
    assert [c['id'] for c in r['contenders']] == ids
    for c, original in zip(r['contenders'], first['contenders']):
        assert not c['errors'], (c['id'], c['errors'])
        assert len(c['benchmarks']) == 36
        assert all(m['status'] == 'measured' for m in c['benchmarks'])
        assert c['source'] == original['source'] and c['binary'] == original['binary']

def summarize(values):
    return dict(median=statistics.median(values), min=min(values), max=max(values))

indices = {i: [] for i in ids}
for run, metrics in zip(runs, maps):
    scores = {i: [] for i in ids}
    for name in CORE:
        unit = metrics[ids[0]][name]['unit']
        floor = 0.1 if unit == '% core' else 0.0
        values = {i: max(floor, metrics[i][name]['summary']['median']) for i in ids}
        best = min(values.values())
        for i, value in values.items():
            ratio = 1 if abs(value - best) <= max(abs(best), 0.001) * 0.01 else max(0.01, min(1, best / value))
            scores[i].append(math.log(ratio))
    for i in ids:
        indices[i].append(100 * math.exp(sum(scores[i]) / len(CORE)))

result = dict(summary_schema=1, source_report_schema=7, trial_count=len(runs),
    aggregation='Median of run medians; range is min/max of run medians. Samples are not pooled.',
    profile=first['profile'], host=first['host'],
    tool_commit=json.loads((paths[0].parent/'provenance.json').read_text())['harness_commit'],
    tool_sha256=json.loads((paths[0].parent/'provenance.json').read_text())['harness_sha256'],
    warning_counts=[len(r['warnings']) for r in runs], contenders={}, metrics={})
for c in first['contenders']:
    i = c['id']
    result['contenders'][i] = dict(source=c['source'], binary=c['binary'],
        balanced_index={**summarize(indices[i]), 'per_trial': indices[i]})
names = CORE + sorted(set(maps[0][ids[0]]) - set(CORE))
for name in names:
    example = maps[0][ids[0]][name]
    entry = dict(unit=example['unit'], direction=example['direction'],
        classification='core' if name in CORE else 'context', contenders={})
    for i in ids:
        samples = [m[i][name]['summary'] for m in maps]
        medians = [s['median'] for s in samples]
        entry['contenders'][i] = {**summarize(medians), 'run_medians': medians,
            'run_p95': [s['p95'] for s in samples], 'run_sample_counts': [s['count'] for s in samples]}
    result['metrics'][name] = entry
out = paths[0].parent
(out/'statistics.json').write_text(json.dumps(result, indent=2)+'\n')
with (out/'statistics.csv').open('w') as f:
    w = csv.writer(f, lineterminator='\n')
    w.writerow(['metric', 'unit', 'classification', 'contender', 'median', 'min', 'max'])
    for name, entry in result['metrics'].items():
        for i, values in entry['contenders'].items():
            w.writerow([name, entry['unit'], entry['classification'], i, values['median'], values['min'], values['max']])

def fmt(value, unit):
    if unit == 'KiB': return f'{value/1024:.2f} MiB'
    if unit == 'bytes': return f'{value/1024:.2f} KiB'
    return f'{value:.3f} {unit}'


lines = ['# Comparative benchmark results', '',
    f"{len(runs)} {first['profile']['name']} trials on one host; median of run medians, with no pooled samples.",
    'Ranges describe the spread of run medians, not confidence intervals.', '',
    '| Contender | Balanced index | Run range |', '| --- | ---: | ---: |']
for i in sorted(ids, key=lambda i: statistics.median(indices[i]), reverse=True):
    s = summarize(indices[i])
    lines.append(f"| {i} | {s['median']:.2f} | {s['min']:.2f} to {s['max']:.2f} |")
lines += ['', 'Each cell shows the median and the minimum-to-maximum run-median range.', '', '| Metric | Uniterm | Herdr | tmux |', '| --- | ---: | ---: | ---: |']
for name, entry in result['metrics'].items():
    cells = []
    for i in ids:
        values = entry['contenders'][i]
        cells.append(f"{fmt(values['median'], entry['unit'])} ({fmt(values['min'], entry['unit'])} to {fmt(values['max'], entry['unit'])})")
    lines.append(f"| {name} | " + ' | '.join(cells) + ' |')
if args.baseline:
    baseline = json.loads(args.baseline.read_text())
    assert baseline['profile'] == result['profile'], 'Baseline must use the same profile'
    assert baseline['host'] == result['host'], 'Baseline must describe the same host'
    lines += ['', '## Before and after the Uniterm changes', '',
        'The earlier campaign used the same host, compiler, harness, profile and comparator binaries.',
        'The campaigns ran at different times on an active desktop; this is not an interleaved A/B experiment.',
        'Zero CPU readings and observer-floor input times should not be interpreted as precise speedups.', '',
        '| Core metric | Previous Uniterm | Current Uniterm | Previous Herdr/Uniterm | Current Herdr/Uniterm |',
        '| --- | ---: | ---: | ---: | ---: |']
    for name in CORE:
        old = baseline['metrics'][name]['contenders']; new = result['metrics'][name]['contenders']
        unit = result['metrics'][name]['unit']
        ratio = lambda x: f"{x['herdr']['median']/x['uniterm']['median']:.3f}" if x['uniterm']['median'] and not name.endswith('_cpu') else 'not interpreted'
        lines.append(f"| {name} | {fmt(old['uniterm']['median'], unit)} | {fmt(new['uniterm']['median'], unit)} | {ratio(old)} | {ratio(new)} |")
    lines += ['', 'For these lower-is-better metrics, Herdr/Uniterm ratios above 1 favor Uniterm.']
lines += ['', 'CPU scoring applies the harness 0.1%-of-one-core floor and one-percent tie rule.',
    'Observed zero CPU is not proof of zero CPU use; the terminal observer polls at about 2 ms.',
    'Desktop applications remained running. Claims apply to this host and workload.',
    'The generated report, raw JSON, statistics JSON/CSV and provenance accompany this summary.', '']
(out/'SUMMARY.md').write_text('\n'.join(lines))
print('\n'.join(lines[:10]))
