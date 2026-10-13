"""Versioned external artifact/functional grading, not workflow completion grading.

Only the parent holds expectations and adjudicates results. Worker values and
exception names are untrusted observations, never pass markers. Python process
isolation is NOT a sandbox or proof that an implementation produced its response.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import time

BASE = Path(__file__).resolve().parents[1]
MAX_BYTES = 16384
WORKER = r'''
import copy
import importlib.util
import json
import pathlib
import sys
request = json.loads(sys.stdin.read())
root = pathlib.Path(sys.argv[1])
sys.path.insert(0, str(root))
spec = importlib.util.spec_from_file_location('subject', root / 'app.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
argument = request['argument']
original = copy.deepcopy(argument)
try:
    value = module.evaluate(argument)
except Exception as error:
    response = {'schema_version': 1, 'kind': 'exception', 'name': type(error).__name__, 'input_preserved': argument == original}
else:
    response = {'schema_version': 1, 'kind': 'value', 'value': value, 'input_preserved': argument == original}
print(json.dumps(response, allow_nan=False))
'''

# Hidden test inputs/expected values live outside copied subject workspaces.
TESTS = {
    'train-local': [([{'amount':' 7 ','status':'posted'},{'amount':'9'},{'amount':'-2','status':'posted'}], 5), ([], 0), ([{'amount':'-11','status':'posted'},{'amount':'50','status':'void'}], -11)],
    'holdout-debug': [({'start':'2026-01-01T00:00:00Z','end':'2026-01-01T01:00:00Z','events':[{'id':'a','at':'2026-01-01T00:30:00Z'},{'id':'b','at':'2026-01-01T01:00:00Z'},{'id':'c','at':'2025-12-31T20:15:00-04:00'}]}, ['a','c']), ({'start':'2026-01-01T00:00:00Z','end':'2026-01-01T01:00:00Z','events':[]}, []), ({'start':'2026-01-01T00:00:00Z','end':'2026-01-01T00:00:00Z','events':[{'id':'x','at':'2026-01-01T00:00:00Z'}]}, [])],
    'train-parallel': [({'ledger':[10,-3,2],'inventory':[{'sku':'b','qty':2},{'sku':'b','qty':3},{'sku':'a','qty':9,'void':True}], 'paths':['/a/b','a/c/','/'], 'words':['Red blue','RED']}, {'ledger':9,'inventory':{'b':5},'paths':3,'words':{'blue':1,'red':2}}), ({'ledger':[],'inventory':[],'paths':[],'words':[]}, {'ledger':0,'inventory':{},'paths':0,'words':{}}), ({'ledger':[-10,4],'inventory':[{'sku':'x','qty':-2},{'sku':'x','qty':5}],'paths':['x/x','/x/y/'],'words':['A a',' b  A ']}, {'ledger':-6,'inventory':{'x':3},'paths':2,'words':{'a':3,'b':1}})],
    'holdout-parallel': [({'intervals':[[5,8],[1,3],[3,6]],'routes':[{'destination':'x','cost':4,'hop':'b'},{'destination':'x','cost':4,'hop':'a'}],'metrics':[9,2,5,1],'tags':[['a','b'],['b','c']]}, {'intervals':[[1,8]],'routes':{'x':[4,'a']},'metrics':2,'tags':['b']}), ({'intervals':[],'routes':[],'metrics':[],'tags':[]}, {'intervals':[],'routes':{},'metrics':None,'tags':[]}), ({'intervals':[[8,9],[1,2],[1,4]],'routes':[{'destination':'y','cost':8,'hop':'a'},{'destination':'y','cost':2,'hop':'z'}],'metrics':[-2,7,3],'tags':[['z','a','a'],['a'],['b','a']]}, {'intervals':[[1,4],[8,9]],'routes':{'y':[2,'z']},'metrics':3,'tags':['a']})],
    'train-handoff': [([{'id':'z','value':' 2 '},{'id':'a','value':'9'},{'id':'b','value':'2'}], {'normalized':{'z':2,'a':9,'b':2},'ranking':{'ordered_ids':['b','z','a'],'checksum':33}}), ([], {'normalized':{},'ranking':{'ordered_ids':[],'checksum':0}}), ([{'id':'a','value':'-4'},{'id':'b','value':' 10 '}], {'normalized':{'a':-4,'b':10},'ranking':{'ordered_ids':['a','b'],'checksum':16}})],
    'holdout-recovery': [({'rows':[{'id':'a','amount':'0.29'},{'id':'b','amount':'-1.05'}],'failures':1}, {'published':{'a':29,'b':-105},'attempts':2,'commits':[{'a':29,'b':-105}]}), ({'rows':[],'failures':0}, {'published':{},'attempts':1,'commits':[{}]}), ({'rows':[{'id':'x','amount':'1.00'}],'failures':2}, {'published':{'x':100},'attempts':3,'commits':[{'x':100}]})],
}
ERRORS = {
    'train-handoff': [([{'id':'x','value':'1'},{'id':'x','value':'2'}], 'ValueError')],
    'holdout-recovery': [({'rows':[],'failures':3}, 'TransientError'), ({'rows':[{'id':'x','amount':'0.001'}],'failures':0}, 'ValueError')],
}
REVIEW = {
    'train-review': {('cache.py','lookup'): ('empty','fetch'), ('billing.py','charge'): ('cached','input')},
    'holdout-review': {('catalog.py','visible'): ('missing','enabled'), ('transfer.py','transfer'): ('negative','transfer')},
}


def unique_object(items):
    result = {}
    for key, value in items:
        if key in result:
            raise ValueError('duplicate key')
        result[key] = value
    return result


def strict_equal(value, expected):
    if type(value) is not type(expected):
        return False
    if type(expected) is dict:
        return value.keys() == expected.keys() and all(strict_equal(value[k], v) for k, v in expected.items())
    if type(expected) is list:
        return len(value) == len(expected) and all(strict_equal(v, e) for v, e in zip(value, expected))
    return value == expected


def observe(workspace, argument, timeout=1.0):
    with tempfile.TemporaryFile() as stream:
        child = subprocess.Popen([sys.executable,'-I','-B','-c',WORKER,str(workspace)], stdin=subprocess.PIPE, stdout=stream, stderr=subprocess.DEVNULL, cwd=workspace, start_new_session=True)
        deadline = time.monotonic() + timeout
        try:
            child.stdin.write(json.dumps({'argument':argument}).encode())
            child.stdin.close()
            while child.poll() is None:
                if time.monotonic() >= deadline or os.fstat(stream.fileno()).st_size > MAX_BYTES:
                    return None
                time.sleep(0.01)
            if child.returncode != 0:
                return None
        except OSError:
            return None
        finally:
            try:
                child.stdin.close()
            except OSError:
                pass
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()
        stream.seek(0)
        data = stream.read(MAX_BYTES+1)
        if len(data) > MAX_BYTES:
            return None
        try:
            return json.loads(data, object_pairs_hook=unique_object)
        except (ValueError, UnicodeError, RecursionError):
            return None


def valid_observation(response, expected, error=False):
    if type(response) is not dict or type(response.get('schema_version')) is not int or response['schema_version'] != 1 or response.get('input_preserved') is not True:
        return False
    keys = {'schema_version','kind','input_preserved','name' if error else 'value'}
    return (set(response) == keys and response['kind'] == ('exception' if error else 'value')
            and strict_equal(response['name' if error else 'value'], expected))


def read_json(path):
    if path.is_symlink() or path.stat().st_size > MAX_BYTES:
        raise ValueError('invalid artifact')
    return json.loads(path.read_bytes(), object_pairs_hook=unique_object)


def preserved(workspace, case):
    source = BASE / 'workspaces' / case
    names = ['input.json','TASK.md']
    if case in REVIEW:
        names.extend(p.name for p in source.glob('*.py'))
    return all(not (workspace/name).is_symlink() and (workspace/name).stat().st_size <= MAX_BYTES and (workspace/name).read_bytes() == (source/name).read_bytes() for name in names)


def review_valid(answer, case):
    if type(answer) is not dict or set(answer) != {'findings'} or type(answer['findings']) is not list:
        return False
    expected = REVIEW[case]
    seen = set()
    for finding in answer['findings']:
        if type(finding) is not dict or set(finding) != {'file','function','category','severity','explanation'} or not all(type(v) is str for v in finding.values()):
            return False
        key = (finding['file'],finding['function'])
        if key in seen or key not in expected or finding['severity'] != 'medium' or re.fullmatch(r'[a-z]+(?:-[a-z]+)*', finding['category']) is None:
            return False
        explanation = finding['explanation'].lower()
        if not all(term in explanation for term in expected[key]):
            return False
        seen.add(key)
    return seen == expected.keys()


def grade(workspace, case, timeout=1.0):
    workspace = Path(workspace).resolve()
    try:
        if case not in TESTS and case not in REVIEW or not preserved(workspace,case):
            return False
        answer = read_json(workspace/'answer.json')
        if case in REVIEW:
            return review_valid(answer,case)
        if not strict_equal(answer, {'result':TESTS[case][0][1]}):
            return False
        if any(path.is_symlink() for path in workspace.glob('*.py')):
            return False
        for argument, expected in TESTS[case]:
            if not valid_observation(observe(workspace,argument,timeout),expected):
                return False
        for argument, name in ERRORS.get(case,[]):
            if not valid_observation(observe(workspace,argument,timeout),name,error=True):
                return False
        return preserved(workspace,case)
    except (OSError, ValueError, KeyError, TypeError, RecursionError):
        return False


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('case')
    args = parser.parse_args()
    workspace = os.environ.get('BCODE_EVAL_WORKSPACE')
    passed = workspace is not None and grade(workspace,args.case)
    print('independent artifact and behavior oracle: ' + ('pass' if passed else 'fail'))
    sys.exit(0 if passed else 1)
