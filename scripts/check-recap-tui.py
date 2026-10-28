#!/usr/bin/env python3
"""Offline acceptance against real recap subprocesses; never builds or calls a provider.

Coordinator sequence (Cargo slot required for the first two commands only):
  RECAP_TUI_FIXTURE_DIR=/absolute/fresh/fixture cargo test -p bcode_recap_cli --features tui \
    --test engine_fixture -- --nocapture
  cargo build -p bcode_recap_cli --features tui --bin recap
  # Supply the existing bcode_terminal_grid_probe build from tui-components too.
  python3 scripts/check-recap-tui.py --binary /absolute/recap \
    --grid-probe /absolute/bcode_terminal_grid_probe \
    --fixture-store /absolute/fresh/fixture/store --artifacts /absolute/output

Optional --plan JSON is an array of journeys:
 [{"name":"reader", "args":["tui"], "env":{"NO_COLOR":"1"},
   "steps":[{"expect":["Recap"],"send":"\\t"},
            {"expect":["Library"],"resize":[40,12]},
            {"expect":["Library"],"send":"\\u0003"}], "exit_code":0}]
Every step expects current *rendered grid* content, never ANSI history. Resize
is applied before waiting. A zero-size step may use alive:true instead of expect.
Every matched frame retains its ANSI capture for styled replay. With --styled-probe,
BMUX owns ANSI parsing and positioned styled runs; .styled.svg preserves those colors.
--playwright-module renders SVG artifacts to PNG using an already-installed browser.
Without --styled-probe the plain .svg is text-only and cannot prove palette fidelity.
Keys follow a matched screen; no blind sleep-based navigation. Each matched frame
produces a grid and SVG (semantic text only, not a claim of color fidelity).
The probe does not expose styled cells; color journeys prove operation, not
palette correctness. No dependency installation or provider spending is allowed.
"""

import argparse
import errno
import fcntl
import hashlib
import html
import json
import os
from pathlib import Path
import pty
import select
import shutil
import signal
import struct
import subprocess
import termios
import time

MAX_CAPTURE = 8 * 1024 * 1024
ENTER = b"\x1b[?1049h"
LEAVE = b"\x1b[?1049l"


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def svg_grid(text, path):
    """Readable, browser-viewable terminal-grid artifact, not an ANSI dump."""
    lines = text.splitlines() or [""]
    # Geometry is presentation of a probe-owned grid, not a layout implementation.
    body = ''.join(f'<tspan x="16" y="{28 + i * 20}">{html.escape(line)}</tspan>'
                   for i, line in enumerate(lines))
    path.write_text('<svg xmlns="http://www.w3.org/2000/svg" width="1440" '
                    f'height="{max(80, len(lines) * 20 + 32)}">'
                    '<rect width="100%" height="100%" fill="#171c25"/>'
                    '<text fill="#e9edf5" font-family="monospace" font-size="16" '
                    f'xml:space="preserve">{body}</text></svg>')


def pump(master, capture, wait=0.05):
    if select.select([master], [], [], wait)[0]:
        try:
            chunk = os.read(master, 65536)
        except OSError as error:
            if error.errno != errno.EIO:
                raise
            return
        capture.extend(chunk)
        require(len(capture) <= MAX_CAPTURE, 'PTY output exceeded 8 MiB bound')


def resize(slave, columns, rows):
    require(0 <= columns <= 300 and 0 <= rows <= 150, 'invalid terminal dimensions')
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', rows, columns, 0, 0))


def pty_case(binary, args, env, work, name, expected, steps=None, probe=None):
    directory = work / name
    directory.mkdir()
    master, slave = pty.openpty()
    resize(slave, 120, 40)
    original = termios.tcgetattr(slave)
    capture = bytearray()
    child = None
    report = {'args': args, 'frames': [], 'accepted': False}
    columns, rows = 120, 40
    try:
        def controlling_terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        child = subprocess.Popen([str(binary), *args], cwd=work, env=env,
                                 stdin=slave, stdout=slave, stderr=slave,
                                 preexec_fn=controlling_terminal)
        for index, step in enumerate(steps or []):
            require(step.get('expect') or step.get('alive'), 'step needs visible expectations')
            if 'resize' in step:
                columns, rows = step['resize']
                resize(slave, columns, rows)
                child.send_signal(signal.SIGWINCH)
            deadline = time.monotonic() + step.get('timeout', 10)
            not_before = time.monotonic() + 0.2
            matched = False
            grid = ''
            while time.monotonic() < deadline:
                pump(master, capture)
                require(child.poll() is None, f'{name}: exited before step {index}')
                if step.get('alive'):
                    # Explicit liveness hold is for zero-sized terminals only.
                    require(columns == 0 or rows == 0, 'alive-only step requires zero size')
                    if time.monotonic() + 0.25 >= deadline:
                        matched = True
                        break
                    continue
                require(probe is not None and columns and rows, 'grid probe/nonzero size required')
                (directory / 'terminal.capture').write_bytes(capture)
                result = subprocess.run([str(probe), str(directory / 'terminal.capture'),
                                         str(columns), str(rows)], stdin=subprocess.DEVNULL,
                                        capture_output=True, timeout=10, check=True)
                require(len(result.stdout) <= MAX_CAPTURE, 'probe output bound exceeded')
                grid = result.stdout.decode('utf-8', 'replace')
                text = grid.casefold()
                if (time.monotonic() >= not_before
                        and all(word.casefold() in text for word in step.get('expect', []))
                        and all(word.casefold() not in text for word in step.get('forbid', []))):
                    matched = True
                    break
            (directory / f'{index:02d}.capture').write_bytes(capture)
            (directory / f'{index:02d}.grid.txt').write_text(grid)
            svg_grid(grid, directory / f'{index:02d}.svg')
            require(matched, f'{name}: step {index} timed out: {step}; current grid: {grid!r}')
            require(not termios.tcgetattr(master)[3] & (termios.ICANON | termios.ECHO),
                    f'{name}: matched screen without raw mode')
            report['frames'].append({'index': index, 'size': [columns, rows], 'step': step})
            if step.get('send'):
                os.write(master, step['send'].encode())
        deadline = time.monotonic() + 15
        while child.poll() is None and time.monotonic() < deadline:
            pump(master, capture)
            if not steps:
                require(termios.tcgetattr(master) == original,
                        f'{name}: headless command changed terminal mode')
        require(child.poll() is not None, f'{name}: did not exit naturally')
        for _ in range(20):
            before = len(capture)
            pump(master, capture, 0.01)
            if before == len(capture):
                break
        require(child.returncode == expected, f'{name}: exit {child.returncode}, expected {expected}')
        require(termios.tcgetattr(master) == original, f'{name}: original termios not restored')
        if steps:
            require(ENTER in capture, f'{name}: no alternate-screen entry')
            require(capture.count(ENTER) == capture.count(LEAVE), f'{name}: unbalanced alternate screen')
            require(capture.rfind(LEAVE) > capture.rfind(ENTER), f'{name}: alternate screen not restored')
        else:
            require(b'\x1b' not in capture, f'{name}: headless command emitted terminal controls')
        report['accepted'] = True
        return bytes(capture)
    except BaseException as error:
        report['failure'] = str(error)
        raise
    finally:
        report['natural_exit'] = child.returncode if child else None
        report['tty_restored_before_cleanup'] = termios.tcgetattr(master) == original
        if child and child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=3)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=3)
        (directory / 'terminal.capture').write_bytes(capture)
        (directory / 'evidence.json').write_text(json.dumps(report, indent=2))
        os.close(master)
        os.close(slave)


def redirected(binary, args, env, work, name, expected):
    with (work / f'{name}.stdout').open('wb') as out, (work / f'{name}.stderr').open('wb') as err:
        result = subprocess.run([str(binary), *args], stdin=subprocess.DEVNULL,
                                stdout=out, stderr=err, cwd=work, env=env, timeout=20)
    stdout = (work / f'{name}.stdout').read_bytes()
    stderr = (work / f'{name}.stderr').read_bytes()
    require(result.returncode == expected, f'{name}: exit {result.returncode}, expected {expected}: {stderr!r}')
    require(b'\x1b' not in stdout + stderr, f'{name}: redirected output has terminal controls')
    return stdout, stderr


def headless(binary, env, work, store):
    missing = str(work / 'does-not-exist.toml')
    cases = [
        ('bare', [], 0),
        ('help', ['--help'], 0),
        ('version', ['--version'], 0),
        ('explicit-tui', ['tui'], 1),
        ('unknown', ['--definitely-invalid'], 2),
        ('positive-budget', ['--max-document-bytes', '0', 'inspect', 'missing'], 2),
        ('unsafe-id', ['inspect', '../outside'], 2),
        ('create-error', ['create', '--source', str(work / 'absent.mp4'), '--model-config', missing], 1),
        ('refine-error', ['refine', 'missing', '--focus', 'Inspect source', '--model-config', missing], 1),
        ('inspect-missing', ['inspect', 'missing'], 1),
        ('export-missing', ['export', 'missing'], 1),
        ('doctor-error', ['doctor', '--ffmpeg', str(work / 'no-ffmpeg')], 1),
    ]
    for command in ['create', 'refine', 'inspect', 'export', 'doctor']:
        cases.append((f'{command}-help', [command, '--help'], 0))
    for name, args, code in cases:
        args = ['--store', str(store), *args]
        out, err = redirected(binary, args, env, work, name, code)
        if name == 'bare':
            require(b'Usage:' in out + err and b'create' in out + err, 'bare noTTY must show help')
        if name == 'explicit-tui':
            text = (out + err).lower()
            require(b'terminal' in text and b'headless' in text, 'nonTTY tui needs actionable failure')
        if name not in ['bare', 'explicit-tui']:
            pty_case(binary, args, env, work, f'tty-{name}', code)
    identifier = 'unicode-field-notes'
    for tty in [False, True]:
        args = ['--store', str(store), 'inspect', identifier, '--json']
        if tty:
            document = json.loads(pty_case(binary, args, env, work, 'tty-inspect-json', 0))
        else:
            document = json.loads(redirected(binary, args, env, work, 'inspect-json', 0)[0])
        require(document['revision'] == 2 and '日本語' in document['title'], 'validated fixture not preserved')
    output = work / 'exported.html'
    args = ['--store', str(store), 'export', identifier, '--output', str(output)]
    redirected(binary, args, env, work, 'export', 0)
    before = output.read_bytes()
    require(b'data:image/jpeg;base64,' in before, 'export did not embed real media')
    redirected(binary, args, env, work, 'export-conflict', 1)
    require(output.read_bytes() == before, 'failed export overwrote file')
    pty_case(binary, [*args, '--force'], env, work, 'tty-export-force', 0)
    require(output.read_bytes() == before, 'force changed document semantics')


def terminal_color(value, default):
    if value is None:
        return default
    if 'Rgb' in value:
        rgb = value['Rgb']
        return '#%02x%02x%02x' % (rgb['r'], rgb['g'], rgb['b'])
    index = value['Indexed']
    palette = ['#000000', '#800000', '#008000', '#808000', '#000080', '#800080',
               '#008080', '#c0c0c0', '#808080', '#ff0000', '#00ff00', '#ffff00',
               '#0000ff', '#ff00ff', '#00ffff', '#ffffff']
    if index < 16:
        return palette[index]
    if index >= 232:
        gray = 8 + (index - 232) * 10
        return '#%02x%02x%02x' % (gray, gray, gray)
    index -= 16
    levels = [0, 95, 135, 175, 215, 255]
    return '#%02x%02x%02x' % (levels[index // 36], levels[index // 6 % 6], levels[index % 6])


def styled_svg(snapshot, destination):
    """Paint BMUX-owned positioned runs; no ANSI interpretation or text measuring."""
    width, height = snapshot['width'], snapshot['height']
    require(0 < width <= 300 and 0 < height <= 150, 'styled snapshot dimensions out of bounds')
    rectangles, labels = [], []
    for row_index, row in enumerate(snapshot['rows'][:height]):
        runs = row['runs']
        for index, run in enumerate(runs):
            style = snapshot['styles'][run['style']]
            fg = terminal_color(style['fg'], '#eeeeee')
            bg = terminal_color(style['bg'], '#000000')
            if style['inverse']:
                fg, bg = bg, fg
            start = run['start_col']
            end = runs[index + 1]['start_col'] if index + 1 < len(runs) else width
            rectangles.append(f'<rect x="{start * 10}" y="{row_index * 20}" '
                              f'width="{max(0, end-start) * 10}" height="20" fill="{bg}"/>')
            decoration = ' '.join(value for enabled, value in [
                (style['underline'], 'underline'), (style['strike'], 'line-through')] if enabled)
            labels.append(f'<text x="{start * 10}" y="{row_index * 20 + 16}" fill="{fg}" '
                          f'font-weight="{"bold" if style["bold"] else "normal"}" '
                          f'font-style="{"italic" if style["italic"] else "normal"}" '
                          f'opacity="{0.5 if style["dim"] else 1}" text-decoration="{decoration}">'
                          f'{html.escape(run["text"])}</text>')
    destination.write_text('<svg xmlns="http://www.w3.org/2000/svg" '
                           f'width="{width * 10}" height="{height * 20}" '
                           'font-family="monospace" font-size="16" xml:space="preserve">'
                           '<rect width="100%" height="100%" fill="#000000"/>'
                           + ''.join(rectangles + labels) + '</svg>')


def render_styled_artifacts(work, helper):
    for evidence in sorted(work.glob('*/evidence.json')):
        report = json.loads(evidence.read_text())
        for frame in report['frames']:
            columns, rows = frame['size']
            if not columns or not rows:
                continue
            prefix = evidence.parent / f'{frame["index"]:02d}'
            result = subprocess.run([str(helper), str(prefix.with_suffix('.capture')), str(columns), str(rows)],
                                    stdin=subprocess.DEVNULL, capture_output=True, check=True, timeout=15)
            require(len(result.stdout) <= MAX_CAPTURE, 'styled snapshot exceeds output bound')
            snapshot = json.loads(result.stdout)
            prefix.with_suffix('.styled.json').write_bytes(result.stdout)
            styled_svg(snapshot, prefix.with_suffix('.styled.svg'))


def render_pngs(work, module):
    """Optional local browser rendering; no install, downloads, or network access."""
    script = r'''
const fs = require('node:fs');
const path = require('node:path');
(async () => {
  const playwright = require(process.argv[1]);
  const browser = await playwright.chromium.launch({headless:true});
  try {
    const page = await browser.newPage({viewport:{width:1440,height:900},deviceScaleFactor:1});
    await page.route('http://**/*', route => route.abort());
    await page.route('https://**/*', route => route.abort());
    for (const directory of fs.readdirSync(process.argv[2], {withFileTypes:true})) {
      if (!directory.isDirectory()) continue;
      const root = path.join(process.argv[2], directory.name);
      for (const name of fs.readdirSync(root).filter(name => name.endsWith('.svg'))) {
        const source = path.join(root, name);
        await page.setContent('<!doctype html><html><body style="margin:0">' + fs.readFileSync(source,'utf8') + '</body></html>');
        await page.screenshot({path:source.slice(0,-4)+'.png',fullPage:true,animations:'disabled',timeout:15000});
      }
    }
  } finally { await browser.close(); }
})().catch(error => { console.error(error.message); process.exitCode=1; });
'''
    subprocess.run(['node', '-e', script, str(module.resolve(strict=True)), str(work)],
                   stdin=subprocess.DEVNULL, check=True, timeout=90)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--grid-probe', type=Path)
    parser.add_argument('--fixture-store', required=True, type=Path)
    parser.add_argument('--artifacts', required=True, type=Path)
    parser.add_argument('--plan', type=Path)
    parser.add_argument('--styled-probe', type=Path,
                        help='render_preview executable: BMUX capture/columns/rows -> styled JSON')
    parser.add_argument('--playwright-module', type=Path,
                        help='optional already-installed Playwright module for browser PNG grid artifacts')
    parser.add_argument('--headless-only', action='store_true', help='explicit partial acceptance; no TUI claim')
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    require(os.access(binary, os.X_OK), 'binary is not executable')
    work = args.artifacts.resolve()
    require(not work.exists(), 'artifact directory must be fresh (no overwritten evidence)')
    work.mkdir(parents=True)
    store = work / 'store'
    shutil.copytree(args.fixture_store.resolve(strict=True), store, symlinks=False)
    env = {key: value for key, value in os.environ.items()
           if key in ['PATH', 'TMPDIR', 'SYSTEMROOT', 'DYLD_LIBRARY_PATH']}
    env.update(HOME=str(work / 'home'), XDG_CONFIG_HOME=str(work / 'config'),
               XDG_STATE_HOME=str(work / 'state'), TERM='xterm-256color', LANG='en_US.UTF-8',
               LC_ALL='en_US.UTF-8', NO_COLOR='1')
    (work / 'home').mkdir()
    evidence = {'binary': str(binary), 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
                'headless_only': args.headless_only, 'accepted': False}
    try:
        headless(binary, env, work, store)
        if not args.headless_only:
            require(args.grid_probe, '--grid-probe required for TUI acceptance')
            probe = args.grid_probe.resolve(strict=True)
            journeys = json.loads(args.plan.read_text()) if args.plan else default_journeys()
            failures = []
            for journey in journeys:
                name = journey['name']
                require(name and all(c.isalnum() or c in '-_' for c in name), 'unsafe journey name')
                child_env = dict(env)
                child_env.pop('NO_COLOR', None)
                child_env.update(journey.get('env', {}))
                try:
                    pty_case(binary, ['--store', str(store), *journey.get('args', ['tui'])],
                             child_env, work, name, journey.get('exit_code', 0), journey['steps'], probe)
                except (AssertionError, OSError, subprocess.SubprocessError) as error:
                    failures.append({'journey': name, 'failure': str(error)})
            evidence['journey_failures'] = failures
            require(not failures, f'{len(failures)} TUI journeys failed; see per-journey evidence.json')
        evidence['accepted'] = True
        print(f'Acceptance artifacts: {work}')
    except BaseException as error:
        evidence['failure'] = str(error)
        raise
    finally:
        if args.styled_probe:
            try:
                render_styled_artifacts(work, args.styled_probe.resolve(strict=True))
                evidence['styled_capture'] = True
            except (AssertionError, OSError, subprocess.SubprocessError) as error:
                evidence['styled_capture_error'] = str(error)
                evidence['accepted'] = False
        if args.playwright_module:
            try:
                render_pngs(work, args.playwright_module)
                evidence['png_capture'] = True
            except (OSError, subprocess.SubprocessError) as error:
                evidence['png_capture_error'] = str(error)
                evidence['accepted'] = False
        (work / 'acceptance.json').write_text(json.dumps(evidence, indent=2))
        if evidence.get('styled_capture_error') or evidence.get('png_capture_error'):
            raise AssertionError(evidence.get('styled_capture_error') or evidence['png_capture_error'])


def default_journeys():
    quit_key = '\x1b[21~'  # F10; no printable q can intercept source editing.
    theme_key = '\x1b[19~'  # F8 cycles dark -> light -> reduced ANSI palette.
    return [
        {'name': 'ctrl-c-exits-focused-source', 'args': ['tui'], 'steps': [
            {'expect': ['From video to perspective.'], 'send': 'editable source'},
            {'expect': ['editable source'], 'send': '\x03'},
        ], 'exit_code': 0},
        {'name': 'ctrl-q-exits-focused-refinement', 'args': ['tui', '--id', 'unicode-field-notes'], 'steps': [
            {'expect': ['Unicode field notes'], 'send': '\x1b[14~'},
            {'expect': ['REFINE THIS RECAP'], 'send': '\x11'},
        ], 'exit_code': 0},
        {'name': 'f10-exits-focused-source', 'args': ['tui'], 'steps': [
            {'expect': ['From video to perspective.'], 'send': quit_key},
        ], 'exit_code': 0},
        {'name': 'palette-cycle-resize-teardown', 'args': ['tui', '--id', 'unicode-field-notes'], 'steps': [
            {'expect': ['Unicode field notes', '日本語'], 'send': theme_key},
            {'expect': ['Unicode field notes', '日本語'], 'send': theme_key},
            {'expect': ['Unicode field notes', '日本語'], 'send': theme_key},
            {'resize': [20, 8], 'expect': ['RECAP']},
            {'resize': [0, 0], 'alive': True, 'timeout': 1},
            {'resize': [120, 40], 'expect': ['Unicode field notes', '日本語'], 'send': quit_key},
        ]},
        {'name': 'home-input-options-themes-resize', 'args': [], 'steps': [
            {'expect': ['From video to perspective.', 'YOUR LIBRARY'], 'send': '日本語 🌱 café'},
            {'expect': ['日本語', 'café'], 'send': '\x1b[14~'},
            {'expect': ['TRANSCRIPT'], 'send': 'captions.vtt'},
            {'expect': ['captions.vtt'], 'send': '\t'},
            {'expect': ['EDITORIAL FOCUS'], 'send': 'Compare motion'},
            {'expect': ['Compare motion'], 'send': theme_key},
            {'expect': ['Compare motion', 'YOUR LIBRARY'], 'send': theme_key},
            {'expect': ['Compare motion', 'YOUR LIBRARY'], 'send': theme_key},
            {'resize': [60, 30], 'expect': ['SOURCE']},
            {'resize': [20, 8], 'expect': ['RECAP']},
            {'resize': [0, 0], 'alive': True, 'timeout': 1},
            {'resize': [120, 40], 'expect': ['Compare motion', 'YOUR LIBRARY'], 'send': quit_key},
        ]},
        {'name': 'reader-unicode-refine-export', 'args': ['tui', '--id', 'unicode-field-notes'], 'steps': [
            {'expect': ['Unicode field notes', '日本語'], 'send': '\x1b[13~'},
            {'expect': ['CONTENTS', 'Motion and evidence'], 'send': '\x1b[13~'},
            {'expect': ['Unicode field notes'], 'send': '\x1b[14~'},
            {'expect': ['REFINE THIS RECAP'], 'send': 'Inspect actual source'},
            {'expect': ['Inspect actual source'], 'send': '\x1b[14~'},
            {'expect': ['Unicode field notes'], 'send': '\x1b[15~'},
            {'expect': ['Exported', 'HTML is ready'], 'send': theme_key},
            {'expect': ['Unicode field notes'], 'send': theme_key},
            {'expect': ['Unicode field notes'], 'resize': [40, 20]},
            {'expect': ['RECAP'], 'resize': [20, 8]},
            {'expect': ['Unicode field notes', '日本語'], 'resize': [120, 40], 'send': quit_key},
        ]},
        {'name': 'generate-validation-error-teardown', 'args': ['tui'], 'steps': [
            {'expect': ['From video to perspective.', 'YOUR LIBRARY'], 'send': '\x1bOQ'},
            {'expect': ['Add a YouTube URL or a local video path first.'], 'forbid': ['Source & captions'], 'send': quit_key},
        ]},
        {'name': 'reader-load-error-teardown', 'args': ['tui', '--id', 'missing'], 'env': {'NO_COLOR': '1'},
         'steps': [{'expect': ['From video to perspective.', 'storage', 'failed'], 'send': quit_key}]},
    ]


if __name__ == '__main__':
    main()
