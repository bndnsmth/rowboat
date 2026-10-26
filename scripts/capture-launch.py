#!/usr/bin/env python3
"""Actual Rowboat/Wayland screenshots against the isolated launch PostgreSQL.

Requires the built release, rootless launch database, Hyprland, grim and ImageMagick.
No user app state or desktop configuration is modified.
"""
import argparse
import hashlib
import html
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time
import zipfile

ROOT = Path(__file__).resolve().parents[1]
WORK = ROOT / '.local/launch/capture'
OUT = ROOT / 'assets/launch/themes'
DISPLAY = WORK / 'display.json'
LAUNCH = WORK / 'compositor.json'
WIDTH, HEIGHT = 1920, 1200
SCALE = 2
QUERY = ROOT / 'demo/launch/orders.sql'
PALETTES = re.findall(r'\("([a-z0-9-]+)", "([^"]+)"\)',
                      (ROOT / 'crates/rowboat/src/theme_controller.rs').read_text())


def run(*args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


def output(*args):
    return subprocess.check_output(args, text=True)


def live(pid):
    path = Path('/proc') / str(pid) / 'stat'
    return path.exists() and path.read_text().split()[2] != 'Z'


def wait_for(predicate, description, seconds=20):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(.1)
    raise RuntimeError('Timed out: ' + description)


def display():
    d = json.loads(DISPLAY.read_text())
    assert str(WORK / 'nested.lua').encode() in Path(
        '/proc', str(d['pid']), 'cmdline').read_bytes(), 'Display ownership changed'
    return d


def start_display():
    WORK.mkdir(parents=True, exist_ok=True)
    if DISPLAY.exists() and live(json.loads(DISPLAY.read_text())['pid']):
        return display()
    assert not LAUNCH.exists(), 'Existing capture record: inspect/stop before another launch'
    config = WORK / 'nested.lua'
    config.write_text('''-- Project-owned screenshot display only.
hl.monitor({output="",mode="3840x2400@60",position="0x0",scale=2})
hl.config({general={gaps_in=0,gaps_out=0,border_size=0},animations={enabled=false},
  decoration={rounding=0,active_opacity=1,inactive_opacity=1,
    blur={enabled=false},shadow={enabled=false}},
  misc={disable_hyprland_logo=true,disable_splash_rendering=true},
  ecosystem={no_update_news=true,no_donation_nag=true}})
''')
    signature = os.environ.get('HYPRLAND_INSTANCE_SIGNATURE')
    instances = json.loads(output('hyprctl', '-j', 'instances'))
    parent = next(i for i in instances if i['instance'] == signature)
    with (WORK / 'bus.log').open('w') as log:
        bus = subprocess.Popen(['dbus-daemon', '--session', '--nofork', '--print-address=1'],
                               stdout=subprocess.PIPE, stderr=log, text=True,
                               start_new_session=True)
    address = bus.stdout.readline().strip()
    assert address.startswith('unix:')
    LAUNCH.write_text(json.dumps({'parent': parent['instance'], 'wayland': parent['wl_socket'],
                                 'bus_pid': bus.pid, 'bus_address': address}))
    command = 'python ' + str(Path(__file__).resolve()) + ' compositor-child'
    run('hyprctl', '-i', parent['instance'], 'dispatch',
        'hl.dsp.exec_cmd(' + json.dumps(command) +
        ',{workspace="special:rowboat-launch silent",no_focus=true,no_initial_focus=true,float=true})')

    def find():
        for i in json.loads(output('hyprctl', '-j', 'instances')):
            p = Path('/proc', str(i['pid']), 'cmdline')
            if p.exists() and str(config).encode() in p.read_bytes():
                return i
        return None
    i = wait_for(find, 'owned compositor')
    run('hyprctl', '-i', i['instance'], 'output', 'create', 'headless', 'rowboat-launch')
    run('hyprctl', '-i', i['instance'], 'eval',
        'hl.monitor({output="rowboat-launch",mode="3840x2400@60",position="0x0",scale=2})')
    run('hyprctl', '-i', i['instance'], 'eval', 'hl.monitor({output="WAYLAND-1",disabled=true})')
    d = {'pid': i['pid'], 'instance': i['instance'], 'wayland': i['wl_socket'], 'scale': SCALE}
    DISPLAY.write_text(json.dumps(d, indent=2) + '\n')
    return d


def compositor_child():
    c = json.loads(LAUNCH.read_text())
    os.environ.update(AQ_BACKEND='wayland', WAYLAND_DISPLAY=c['wayland'],
                      HYPRLAND_INSTANCE_SIGNATURE=c['parent'],
                      DBUS_SESSION_BUS_ADDRESS=c['bus_address'])
    with (WORK / 'compositor.log').open('w') as log:
        os.dup2(log.fileno(), 1)
        os.dup2(log.fileno(), 2)
    os.execvp('Hyprland', ['Hyprland', '--config', str(WORK / 'nested.lua')])


def capture(theme):
    assert theme in dict(PALETTES)
    d = display()
    assert d.get('scale') == SCALE, 'Restart the capture display for the configured pixel density'
    monitor = next(m for m in json.loads(output('hyprctl', '-i', d['instance'], '-j', 'monitors'))
                   if m['name'] == 'rowboat-launch')
    assert (monitor['width'], monitor['height'], monitor['scale']) == (WIDTH * SCALE, HEIGHT * SCALE, SCALE)
    pidfile = WORK / 'app.pid'
    assert not pidfile.exists() or not live(int(pidfile.read_text())), 'Capture app already active'
    data = WORK / 'states' / theme
    data.mkdir(parents=True, exist_ok=True)
    home = WORK / 'home'
    home.mkdir(exist_ok=True)
    env = dict(os.environ, ROWBOAT_DATA_DIR=str(data), HOME=str(home),
               ROWBOAT_PASSWORD='rowboat-demo', WAYLAND_DISPLAY=d['wayland'],
               HYPRLAND_INSTANCE_SIGNATURE=d['instance'])
    for k in ('DISPLAY', 'DBUS_SESSION_BUS_ADDRESS', 'KEEL_DATA_DIR', 'KEEL_PASSWORD',
              'ROWBOAT_URL', 'KEEL_URL'):
        env.pop(k, None)
    state_file = data / 'state.json'
    if not state_file.exists():
        run(str(ROOT / 'target/release/rowboatctl'), 'profiles', 'add-url',
            '--name', 'Northstar Supply', '--group', 'Launch demo', '--stdin',
            input='postgres://rowboat@127.0.0.1:15436/northstar?sslmode=disable\n',
            text=True, env=env, stdout=subprocess.DEVNULL)
    state = json.loads(state_file.read_text())
    assert len(state['connections']) == 1
    profile = state['connections'][0]
    assert profile['host'] == '127.0.0.1' and profile['port'] == 15436
    state.setdefault('preferences', {}).update(theme=theme, editor_font_size=18,
                                              grid_font_size=15, grid_row_height=34)
    state['history'] = []
    state_file.write_text(json.dumps(state, indent=2) + '\n')
    # --query opens its own document. Start the task-owned fixture empty to avoid
    # duplicating the same query as a recovered tab on subsequent captures.
    session = {'version': 5, 'active_tab': 0, 'tabs': [], 'workspaces': []}
    (data / 'session.json').write_text(json.dumps(session, indent=2) + '\n')
    with (WORK / f'{theme}.log').open('w') as log:
        process = subprocess.Popen(['dbus-run-session', '--', 'bash', '-c',
                                    'echo $$ > "$1"; shift; exec "$@"', 'rowboat-launch',
                                    str(pidfile), str(ROOT / 'target/release/rowboat'),
                                    '--connection', profile['id'], '--query', str(QUERY)],
                                   env=env, cwd=ROOT, stdout=log, stderr=log,
                                   start_new_session=True)
    def window():
        if not pidfile.exists():
            return None
        pid = int(pidfile.read_text())
        return next((w for w in json.loads(output('hyprctl', '-i', d['instance'], '-j', 'clients'))
                     if w['pid'] == pid and w['class'] == 'dev.bndnsmth.rowboat'), None)
    w = wait_for(window, 'Rowboat window')

    def result():
        saved = json.loads(state_file.read_text())
        return next((h for h in saved.get('history', []) if h['sql'] == QUERY.read_text()
                     and h.get('error') is None and h.get('row_count') == 100), None)
    try:
        history = wait_for(result, 'successful real 100-row query')
        time.sleep(.8)  # Allow the final frame after the durable history record.
        w = window()
        assert w['size'] == [WIDTH, HEIGHT], w['size']
        # Genuine UI column resize: enough room for the fictional customer names.
        # Fixed coordinates correspond to the fixed viewport/fonts above.
        pointer = str(ROOT / 'target/debug/rowboat-test-click')
        run(pointer, '727', '760', str(WIDTH), str(HEIGHT), '897', '760', env=env)
        # Zero-distance scroll only moves the pointer out of the composition.
        run(pointer, str(WIDTH - 1), str(HEIGHT - 1), str(WIDTH), str(HEIGHT), '0', env=env)
        time.sleep(.3)
        OUT.mkdir(parents=True, exist_ok=True)
        image = OUT / f'rowboat-{theme}.png'
        run('grim', '-g', f'{w["at"][0]},{w["at"][1]} {WIDTH}x{HEIGHT}',
            '-s', str(SCALE), str(image), env=env)
        record = {'theme': theme, 'name': dict(PALETTES)[theme], 'file': image.name,
                  'width': WIDTH * SCALE, 'height': HEIGHT * SCALE, 'scale': SCALE,
                  'logical_width': WIDTH, 'logical_height': HEIGHT, 'rows': history['row_count'],
                  'query_sha256': hashlib.sha256(QUERY.read_bytes()).hexdigest(),
                  'png_sha256': hashlib.sha256(image.read_bytes()).hexdigest(),
                  'binary_sha256': hashlib.sha256((ROOT / 'target/release/rowboat').read_bytes()).hexdigest()}
        (WORK / f'{theme}.json').write_text(json.dumps(record, indent=2) + '\n')
        print(f'{dict(PALETTES)[theme]}: actual 100-row PostgreSQL result captured', flush=True)
    finally:
        w = window()
        if w:
            run('hyprctl', '-i', d['instance'], 'dispatch',
                'hl.dsp.window.close({window="address:' + w['address'] + '"})')
        process.wait(timeout=15)


def gallery():
    records = [json.loads((WORK / f'{key}.json').read_text()) for key, _ in PALETTES]
    assert len(records) == 22
    (OUT / 'manifest.json').write_text(json.dumps({'business': 'Northstar Supply',
        'database': 'PostgreSQL 18 / northstar', 'query': QUERY.read_text(),
        'screenshots': records}, indent=2) + '\n')
    cards = ''.join(f'<a href="{r["file"]}"><img loading="lazy" src="{r["file"]}" '
                    f'alt="Rowboat in {html.escape(r["name"])}"><span>{html.escape(r["name"])}</span></a>'
                    for r in records)
    (OUT / 'index.html').write_text('''<!doctype html><html lang="en"><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1"><title>Rowboat · Theme gallery</title>
<style>body{max-width:1920px;margin:0 auto;padding:48px;background:#10131a;color:#f2f3f5;font:16px system-ui}
h1{font-size:36px;margin:0 0 8px}p{color:#a7b0c0;margin-bottom:36px}
main{display:grid;grid-template-columns:repeat(auto-fit,minmax(min(100%,720px),1fr));gap:28px}
a{color:inherit;text-decoration:none;background:#1b202c;border-radius:10px;overflow:hidden}
img{display:block;width:100%;height:auto}span{display:block;padding:15px 20px}
@media(max-width:540px){body{padding:20px}main{grid-template-columns:1fr}}</style>
<h1>Rowboat, in every color.</h1><p>22 built-in themes · Native 3840 × 2400 captures (2×) · Click a preview for full resolution</p>
<main>''' + cards + '</main></html>\n')
    thumbs = []
    font = output('fc-match', '-f', '%{file}', 'sans-serif').strip()
    assert Path(font).is_file(), 'A usable sans-serif font is required for labels'
    for r in records:
        thumb = WORK / f'{r["theme"]}-thumb.png'
        run('magick', str(OUT / r['file']), '-resize', '480x300',
            '-background', '#1b202c', '-fill', '#f2f3f5', '-font', font,
            '-gravity', 'south', '-splice', '0x38', '-pointsize', '17',
            '-annotate', '+0+10', r['name'], str(thumb))
        thumbs.append(str(thumb))
    run('magick', 'montage', *thumbs, '-tile', '4x', '-geometry', '+12+12',
        '-background', '#10131a', str(OUT / 'contact-sheet.png'))
    print('Gallery, contact sheet and manifest:', OUT)
    archive = ROOT / '.local/dist/launch/rowboat-launch-assets.zip'
    archive.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(archive, 'w', compression=zipfile.ZIP_DEFLATED, compresslevel=6) as z:
        for path in sorted(OUT.iterdir()):
            if path.is_file():
                z.write(path, 'rowboat-launch-assets/themes/' + path.name)
        z.write(OUT.parent / 'README.md', 'rowboat-launch-assets/README.md')
        z.write(QUERY, 'rowboat-launch-assets/orders.sql')
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_suffix('.zip.sha256').write_text(digest + '  ' + archive.name + '\n')


def stop_display():
    d = display()
    pidfile = WORK / 'app.pid'
    assert not pidfile.exists() or not live(int(pidfile.read_text())), 'Close the capture app first'
    c = json.loads(LAUNCH.read_text())
    assert b'dbus-daemon' in Path('/proc', str(c['bus_pid']), 'cmdline').read_bytes()
    os.kill(d['pid'], signal.SIGTERM)
    os.kill(c['bus_pid'], signal.SIGTERM)
    wait_for(lambda: not live(d['pid']) and not live(c['bus_pid']), 'capture display shutdown')
    DISPLAY.rename(WORK / 'display-stopped.json')
    LAUNCH.rename(WORK / 'compositor-stopped.json')
    print('Capture display stopped; demo PostgreSQL remains available.')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['display', 'compositor-child', 'capture', 'all', 'gallery', 'stop'])
    parser.add_argument('--theme', choices=[key for key, _ in PALETTES], default='tokyo-night')
    args = parser.parse_args()
    if args.action == 'display': start_display()
    elif args.action == 'compositor-child': compositor_child()
    elif args.action == 'capture': capture(args.theme)
    elif args.action == 'all':
        start_display()
        for key, _ in PALETTES: capture(key)
        gallery()
    elif args.action == 'gallery': gallery()
    elif args.action == 'stop': stop_display()
