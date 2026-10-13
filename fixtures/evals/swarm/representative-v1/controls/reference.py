"""Independent known-good producer. Never import grading expectations.

Apply only to a temporary fixture copy; this is a calibration control, not model
execution, workflow completion, cancellation, or an acceptance override.
"""
import argparse
import json
from pathlib import Path
import subprocess
import sys

BASE = Path(__file__).resolve().parents[1]
FINDINGS = {
    'train-review': [
        {'file':'cache.py','function':'lookup','category':'false-cache-miss','severity':'medium','explanation':'An empty cached string causes an unnecessary fetch.'},
        {'file':'billing.py','function':'charge','category':'duplicate-cache-charge','severity':'medium','explanation':'Cached tokens are included in input and must not also receive ordinary pricing.'},
    ],
    'holdout-review': [
        {'file':'catalog.py','function':'visible','category':'unsafe-default','severity':'medium','explanation':'A missing enabled field must mean false, not include the entry.'},
        {'file':'transfer.py','function':'transfer','category':'negative-amount','severity':'medium','explanation':'A negative transfer increases balance instead of being rejected.'},
    ],
}
RUN = r'''
import importlib.util
import json
from pathlib import Path
import sys
root = Path.cwd()
sys.path.insert(0, str(root))
spec = importlib.util.spec_from_file_location('reference_app', root/'app.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
print(json.dumps({'result':module.evaluate(json.loads((root/'input.json').read_text()))}, allow_nan=False))
'''


def produce(workspace, case):
    workspace = Path(workspace)
    sources = json.loads((BASE/'controls'/'reference_sources.json').read_text())
    if case not in sources:
        raise ValueError('unknown reference')
    for name, content in sources[case].items():
        (workspace/name).write_text(content)
    if case in FINDINGS:
        answer = {'findings':FINDINGS[case]}
    else:
        result = subprocess.run([sys.executable,'-I','-B','-c',RUN], cwd=workspace, capture_output=True, text=True, timeout=2, check=True)
        answer = json.loads(result.stdout)
    (workspace/'answer.json').write_text(json.dumps(answer, sort_keys=True)+'\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('case')
    parser.add_argument('workspace',type=Path)
    args = parser.parse_args()
    produce(args.workspace,args.case)
