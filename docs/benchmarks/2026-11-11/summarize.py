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
paths = [Path(p) for p in sys.argv[1:]]
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
    tool_commit='76026d6233515d1e568ba60f5837bbb529e626cb',
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
    return f'{value:.3f}'

lines = ['# Local benchmark results', '',
    'Publication held: Uniterm retains memory and idle-CPU advantages over Herdr, but its relative standing is weaker and the same-host follow-up confirms a startup regression.', '',
    f"{len(runs)} {first['profile']['name']} trials; medians of run medians, with no pooled samples.", '',
    '| Contender | Balanced index | Run range |', '| --- | ---: | ---: |']
for i in sorted(ids, key=lambda i: statistics.median(indices[i]), reverse=True):
    s = summarize(indices[i])
    lines.append(f"| {i} | {s['median']:.2f} | {s['min']:.2f} to {s['max']:.2f} |")
lines += ['', '| Metric | Unit | Uniterm | Herdr | tmux |', '| --- | --- | ---: | ---: | ---: |']
for name, entry in result['metrics'].items():
    cells = [fmt(entry['contenders'][i]['median'], entry['unit']) for i in ids]
    lines.append(f"| {name} | {entry['unit']} | " + ' | '.join(cells) + ' |')
baseline = json.loads((ROOT/'baseline-statistics.json').read_text())
lines += ['', '## Change in standing against Herdr', '',
    'Historical results used different hardware, kernel, compiler, and application versions.',
    'These ratios describe relative standing in two campaigns; they do not isolate a Uniterm code regression.', '',
    'Herdr/Uniterm ratios above 1 favor Uniterm for these lower-is-better metrics.', '',
    '| Metric | Historical Herdr/Uniterm | Current Herdr/Uniterm |', '| --- | ---: | ---: |']
for name in CORE:
    if name.endswith('_cpu'): continue
    old = baseline['metrics'][name]['contenders']; new = result['metrics'][name]['contenders']
    lines.append(f"| {name} | {old['herdr']['median']/old['uniterm']['median']:.3f} | {new['herdr']['median']/new['uniterm']['median']:.3f} |")
lines += ['', 'CPU ranking applies the harness 0.1%-of-one-core floor; observed zero is not proof of no CPU use.',
    'Desktop applications remained running. Claims apply to this host and workload.',
    'See raw JSON, generated report, per-trial statistics, and provenance.json for evidence.', '']
followup = out/'startup-followup.json'
if followup.exists():
    probe = json.loads(followup.read_text())
    old = [s['startup_ms'] for s in probe['samples'] if s['contender'] == 'uniterm-old']
    current = [s['startup_ms'] for s in probe['samples'] if s['contender'] == 'uniterm-current']
    assert len(old) == len(current) == 20
    lines += ['## Same-host startup regression check', '',
        'Twenty paired trials, alternating version order, using the unchanged harness startup adapter, clean release builds and Rust 1.96.0 on this host.', '',
        '| Version | Median startup | Sample range |', '| --- | ---: | ---: |',
        f'| Uniterm 1.0.4 | {statistics.median(old):.3f} ms | {min(old):.3f} to {max(old):.3f} ms |',
        f'| Uniterm 1.2.20 | {statistics.median(current):.3f} ms | {min(current):.3f} to {max(current):.3f} ms |', '',
        f'Current startup is {statistics.median(current)/statistics.median(old):.2f} times slower in this controlled follow-up.',
        'This confirms a startup regression; it does not isolate the responsible code change or establish same-host regressions for the other metrics.',
        'Raw evidence: startup-followup.json; build identity: provenance.json; reproduction source: startup-check.rs and its Cargo files; see NOTES.md.', '']
(out/'SUMMARY.md').write_text('\n'.join(lines))
print('\n'.join(lines[:10]))
