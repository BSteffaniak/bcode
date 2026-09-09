#!/usr/bin/env python3
"""Check production Rust sources for IPC construction outside its owning packages.

This lexical guard is not Rust name resolution. Only exact cfg(test) inline
modules are excluded; unknown conditional forms are checked conservatively.
"""

from pathlib import Path
import re
import sys


TOKEN = re.compile(
    r'//[^\n]*|/\*|(?:br|r)(?P<hashes>\#*)"|'
    r'"(?:\\.|[^"\\])*"|\b[A-Za-z_][A-Za-z_0-9]*\b|[^\s]',
    re.DOTALL,
)
RAW_IPC = re.compile(r'bcode_ipc\s*::\s*Request\b|\bRequest\s*::\s*[A-Z]')


def tokens(source):
    """Return code tokens, skipping strings and nested comments."""
    result = []
    position = 0
    while match := TOKEN.search(source, position):
        value = match.group()
        position = match.end()
        if value.startswith('//') or value.startswith('"'):
            continue
        if value == '/*':
            depth = 1
            while depth:
                marker = re.search(r'/\*|\*/', source[position:])
                if marker is None:
                    raise ValueError('unterminated block comment')
                depth += 1 if marker.group() == '/*' else -1
                position += marker.end()
            continue
        if match.group('hashes') is not None:
            end = source.find('"' + match.group('hashes'), position)
            if end < 0:
                raise ValueError('unterminated raw string')
            position = end + 1 + len(match.group('hashes'))
            continue
        # A character literal can contain a brace. Lifetimes are not literals.
        if value == "'":
            character = re.match(r"(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^'\\])'", source[position:])
            if character:
                position += character.end()
                continue
        result.append(value)
    return result


def production_tokens(source):
    code = tokens(source)
    result = []
    index = 0
    test_attribute = ['#', '[', 'cfg', '(', 'test', ')', ']']
    while index < len(code):
        if code[index:index + 7] == test_attribute:
            start = index + 7
            # Skip additional attributes, but only exclude an actual inline module.
            while code[start:start + 2] == ['#', '[']:
                start += 2
                depth = 1
                while start < len(code) and depth:
                    depth += (code[start] == '[') - (code[start] == ']')
                    start += 1
            if start + 2 < len(code) and code[start] == 'mod' and code[start + 2] == '{':
                index = start + 3
                depth = 1
                while index < len(code) and depth:
                    depth += (code[index] == '{') - (code[index] == '}')
                    index += 1
                if depth:
                    raise ValueError('unterminated test module')
                continue
        result.append(code[index])
        index += 1
    return ' '.join(result).replace(': :', '::')


def violations(root):
    owners = {'client', 'server', 'ipc', 'daemon-lifecycle'}
    for path in sorted((root / 'packages').rglob('*.rs')):
        relative = path.relative_to(root)
        if relative.parts[1] in owners or any(p in {'test', 'tests'} for p in relative.parts):
            continue
        if path.name in {'test.rs', 'tests.rs'}:
            continue
        if RAW_IPC.search(production_tokens(path.read_text(encoding='utf-8'))):
            yield str(relative)


if __name__ == '__main__':
    found = list(violations(Path(__file__).resolve().parent.parent))
    if found:
        print('\n'.join(found))
        sys.exit(1)
