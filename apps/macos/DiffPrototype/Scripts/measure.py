#!/usr/bin/env python3
"""Measure an already-built release spike without activating a GUI window."""
import hashlib
import json
import re
import statistics
import subprocess
from pathlib import Path

base = Path(__file__).resolve().parents[1]
evidence = base / 'evidence'
evidence.mkdir(exist_ok=True)
exe = base / '.build/release/diff-prototype'
for run in range(1, 4):
    with (evidence / f'model-benchmark-{run}.json').open('w') as out, (evidence / f'model-benchmark-{run}-time.txt').open('w') as err:
        subprocess.run(['/usr/bin/time', '-l', str(exe), '--benchmark'], stdout=out, stderr=err, check=True)
with (evidence / 'release-render-time.txt').open('w') as err, (evidence / 'release-render.log').open('w') as out:
    subprocess.run(['/usr/bin/time', '-l', str(exe), '--render-check', str(evidence / 'release-render')], stdout=out, stderr=err, check=True)
runs = [json.loads((evidence / f'model-benchmark-{i}.json').read_text()) for i in range(1, 4)]
reports = []
for index, count in enumerate([20_000, 100_000]):
    metrics = {}
    for key in ['fixture_ms', 'projection_ms', 'expand_ms', 'new_side_copy_ms']:
        values = [r['measurements'][index][key] for r in runs]
        metrics[key] = {'median': statistics.median(values), 'min': min(values), 'max': max(values)}
    reports.append({'rows': count, 'metrics': metrics})

def peak_rss(file):
    return int(re.search(r'(\d+)\s+maximum resident set size', file.read_text()).group(1))

summary = {
    'binary_sha256': hashlib.sha256(exe.read_bytes()).hexdigest(),
    'source_sha256': {str(p.relative_to(base)): hashlib.sha256(p.read_bytes()).hexdigest()
                      for p in sorted((base / 'Sources').rglob('*.swift'))},
    'model_runs': reports,
    'whole_model_benchmark_peak_rss_bytes': [peak_rss(evidence / f'model-benchmark-{i}-time.txt') for i in range(1, 4)],
    'whole_native_render_process_peak_rss_bytes': peak_rss(evidence / 'release-render-time.txt'),
    'rss_warning': 'Whole-process peaks, not per-document memory delta. Concurrent workloads active.',
    'performance_verdict': 'Initial virtual-row feasibility established; no Instruments scroll hitch baseline or final app launch/memory gate established.'
}
(evidence / 'performance-summary.json').write_text(json.dumps(summary, indent=2) + '\n')
print(json.dumps(summary, indent=2))
