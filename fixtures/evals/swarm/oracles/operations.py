import importlib.util
import os
from pathlib import Path

root = Path(os.environ['BCODE_EVAL_WORKSPACE'])
spec = importlib.util.spec_from_file_location('subject_operations', root / 'operations.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
assert module.total([]) == 0
assert module.total([1, 2, 3]) == 6
assert module.total([-1, 5, 9]) == 13
assert module.unique_sorted([3, 1, 3, 2, 1]) == [1, 2, 3]
assert module.unique_sorted([]) == []
print('hidden functional checks passed')
