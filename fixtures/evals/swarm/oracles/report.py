"""Independent persisted-report oracle, including byte-exact preservation."""
import json
import os
from pathlib import Path

root = Path(os.environ['BCODE_EVAL_WORKSPACE'])
expected = Path(__file__).resolve().parents[1] / 'workspace' / 'fixture.txt'
assert (root / 'fixture.txt').read_bytes() == expected.read_bytes(), 'fixture changed'
answer = json.loads((root / 'report.json').read_text())
assert answer == {
    'lines': expected.read_text().splitlines(),
    'sum': 4,
}, 'report does not match independently observed fixture and arithmetic'
print('persisted report and byte-exact preservation verified')
