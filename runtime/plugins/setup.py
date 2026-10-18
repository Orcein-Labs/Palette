#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Prepare Palette on Linux, register it with local clients, and check one call."""
import argparse
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading
import time

PACKAGE = Path(__file__).resolve().parent
# A source checkout or release archive holds this file at runtime/plugins/;
# a client bundle holds it beside sdk/.
CHECKOUT = PACKAGE.parents[1]
DEFAULT_REPO = CHECKOUT if (CHECKOUT/'Project.toml').is_file() else PACKAGE/'sdk'
HOME = Path(os.environ.get('XDG_DATA_HOME') or Path.home()/'.local/share')/'palette'
SYSTEM_PATH = '/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin'


def step(text, command, env, quiet=False):
    """Run one preparation step, printing elapsed time while it is quiet.
    A quiet step's standard output, a machine-readable record, is shown only
    if the step fails."""
    print(f'==> {text}', flush=True)
    done = threading.Event()
    started = time.monotonic()
    def tick():
        while not done.wait(30):
            print(f'    still working, {int(time.monotonic() - started)} s', flush=True)
    threading.Thread(target=tick, daemon=True).start()
    try:
        result = subprocess.run(command, env=env, stdout=subprocess.PIPE if quiet else None, text=True)
    finally:
        done.set()
    if code := result.returncode:
        if quiet:
            print(result.stdout, end='')
        raise SystemExit(f'{text} failed (exit {code}); the output above says why')


def register(launcher):
    """Add the server to the local clients that are installed, never replacing one."""
    clients = {'Claude Code': (['claude', 'mcp', 'get', 'palette'], ['claude', 'mcp', 'add', '--scope', 'user', 'palette', '--', str(launcher)]),
               'Codex': (['codex', 'mcp', 'get', 'palette'], ['codex', 'mcp', 'add', 'palette', '--', str(launcher)])}
    for client, (get, add) in clients.items():
        found = shutil.which(add[0])
        # Inside WSL, PATH also lists Windows programs; install.ps1 registers those.
        if not found or (os.environ.get('WSL_DISTRO_NAME') and found.startswith('/mnt/')):
            continue
        existing = subprocess.run(get, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=120)
        if existing.returncode == 0 and str(launcher) in existing.stdout:
            print(f'{client} already uses this installation.')
        elif existing.returncode == 0:
            print(f'{client} already has a server named palette; left as it is. To use this one: '
                  f'{add[0]} mcp remove palette, then {shlex.join(add)}')
        elif subprocess.run(add, stdin=subprocess.DEVNULL, capture_output=True, timeout=120).returncode == 0:
            print(f'Registered with {client}.')
        else:
            print(f'Could not register with {client}; run: {shlex.join(add)}')


def smoke(launcher, timeout=600):
    """Ask the installed server for 1 + 1 over MCP stdio and return the value."""
    requests = [{'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {
                    'protocolVersion': '2024-11-05', 'capabilities': {}, 'clientInfo': {'name': 'palette-setup', 'version': '1'}}},
                {'jsonrpc': '2.0', 'method': 'notifications/initialized'},
                {'jsonrpc': '2.0', 'id': 2, 'method': 'tools/call', 'params': {'name': 'palette', 'arguments': {'code': '1 + 1'}}}]
    stdin = ''.join(json.dumps(r) + '\n' for r in requests)
    # The server reports its progress on stderr; it is shown only on failure.
    with tempfile.TemporaryFile('w+') as log, \
         subprocess.Popen([str(launcher)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, text=True) as server:
        server.stdin.write(stdin); server.stdin.flush()
        timer = threading.Timer(timeout, server.kill); timer.start()
        try:
            for line in server.stdout:
                reply = json.loads(line)
                if reply.get('id') == 2:
                    if 'error' in reply:
                        raise RuntimeError(reply['error'].get('message', reply['error']))
                    text = ''.join(c.get('text', '') for c in reply['result'].get('content', []))
                    try:
                        return json.loads(text).get('data')
                    except (ValueError, AttributeError):
                        raise RuntimeError(f'unexpected reply: {text[:500]}')
        except Exception:
            server.kill(); log.seek(0); print(log.read()[-4000:], end='', file=sys.stderr)
            raise
        finally:
            timer.cancel(); server.stdin.close(); server.kill()
        log.seek(0); print(log.read()[-4000:], end='', file=sys.stderr)
    raise RuntimeError('the server exited before it answered')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo-dir', type=Path, default=DEFAULT_REPO)
    parser.add_argument('--output-dir', type=Path, default=HOME/'client')
    parser.add_argument('--data-dir', type=Path, default=HOME/'data')
    parser.add_argument('--depot-dir', type=Path, default=HOME/'depot')
    parser.add_argument('--connect-only', action='store_true', help='Use a runtime already built and prewarmed; do not prepare again')
    parser.add_argument('--wsl-distribution', help='Generate Windows Claude configuration targeting this WSL distribution')
    parser.add_argument('--no-register', action='store_true', help='Do not add the server to Claude Code or Codex')
    parser.add_argument('--no-check', action='store_true', help='Do not make a test call at the end')
    args = parser.parse_args()
    if sys.platform != 'linux':
        parser.error('Run setup in Linux or WSL2; the supervised host requires bubblewrap')
    repo, output, data, depot = (p.resolve() for p in (args.repo_dir, args.output_dir, args.data_dir, args.depot_dir))
    for command in ('julia', 'bwrap'):
        if not shutil.which(command):
            parser.error(f'missing prerequisite on PATH: {command}')
    if not (repo / 'Project.toml').is_file():
        parser.error(f'not a Palette SDK: {repo}')
    for private in (data, depot, output):
        if private == repo or repo in private.parents:
            parser.error('Keep data, depot, and generated configuration outside the SDK tree')
    workspace = (data/'workspace').resolve()
    for protected in (repo, depot, output, (data/'state').resolve(), (data/'worlds').resolve()):
        if workspace == protected or workspace.is_relative_to(protected) or protected.is_relative_to(workspace):
            parser.error('Workspace must not overlap source, depot, configuration, or host state')
    host = repo / 'runtime/host/target/release/palette-host'
    # The client starts the server with this environment, not the shell's, so
    # it names the Julia found now and keeps packages beside the other data.
    launch_env = {'PALETTE_REPO':str(repo),'PALETTE_HOST':str(host),'PALETTE_DATA_HOME':str(data),
                  'PALETTE_STATE_DIR':str(data/'state'),'PALETTE_WORKSPACE_STATE_ROOT':str(data/'worlds'),
                  'OPERATOR_WORKSPACE':str(data/'workspace'),'PALETTE_PACKAGE_DEPOT':str(data/'packages'),
                  'JULIA_DEPOT_PATH':str(depot),'JULIA_PKG_OFFLINE':'true',
                  'PATH':str(Path(shutil.which('julia')).parent)+':'+SYSTEM_PATH}
    if not args.connect_only:
        env = dict(os.environ, **{k: v for k, v in launch_env.items() if k not in ('JULIA_PKG_OFFLINE', 'PATH')})
        step('Installing Julia packages', ['julia', '--project='+str(repo), '-e', 'using Pkg; Pkg.instantiate()'], env)
        # A release archive carries a prebuilt host; a checkout builds its own.
        if (repo/'.git').exists() and shutil.which('cargo') or not host.is_file():
            if not shutil.which('cargo'):
                parser.error('This source has no prebuilt host: install Rust (https://rustup.rs) or use the release installer')
            step('Building the sandbox host', ['cargo','build','--manifest-path',str(repo/'runtime/host/Cargo.toml'),'--release','--locked'], env)
        step('Preparing the sandbox (the first run takes several minutes)',
             [sys.executable,str(repo/'runtime/security/prewarm_depot.py'),'--project-dir',str(repo),'--host-bin',str(host)], dict(env, JULIA_PKG_OFFLINE='true'), quiet=True)
    for path in (repo/'Manifest.toml', host):
        if not path.is_file():
            parser.error(f'missing prepared runtime file: {path}')
    if not os.access(host, os.X_OK):
        parser.error(f'host is not executable: {host}')
    (data/'workspace').mkdir(parents=True, exist_ok=True)
    output.mkdir(parents=True, exist_ok=True)
    plugin = output/'palette-local-plugin'
    # The self-contained upload omits nested plugin declarations from its SDK.
    # Materialize the client template only in the external installation folder.
    template = next((p for p in (PACKAGE/'templates/palette-plugin.json', PACKAGE/'palette/plugin.json') if p.is_file()), None)
    plugin.mkdir(parents=True, exist_ok=True)
    if template and not (plugin/'plugin.json').exists():
        shutil.copyfile(template, plugin/'plugin.json')
    subprocess.run([sys.executable,str(repo/'runtime/security/install_operator_plugin.py'),'--format','portable','--plugin-dir',str(plugin),'--workspace-dir',str(data/'workspace'),'--repo-dir',str(repo)],check=True,stdout=subprocess.DEVNULL)
    shutil.copyfile(PACKAGE/'LICENSE', plugin/'LICENSE')
    (plugin/'NOTICE').write_text('Palette client integration\nCopyright (c) 2026 Matt C\nApache-2.0. Core runtime is separately licensed AGPL-3.0-only.\n')
    inherited = next(p for p in (PACKAGE/'licenses/MIT-INHERITED.txt', PACKAGE.parent/'licenses/MIT-INHERITED.txt') if p.is_file())
    shutil.copyfile(inherited, plugin/'MIT-INHERITED.txt')
    manifest_path = plugin/'plugin.json'
    manifest = json.loads(manifest_path.read_text()); manifest['license'] = 'Apache-2.0'
    manifest_path.write_text(json.dumps(manifest, indent=2)+'\n')
    entry={'command':sys.executable,'args':[str(repo/'runtime/security/serve_palette.py')],'env':launch_env}
    config_path=plugin/'mcp.json'
    config=json.loads(config_path.read_text());config['mcpServers']['palette']={'type':'stdio',**entry}
    config_path.write_text(json.dumps(config,indent=2)+'\n')
    launcher=output/'palette-mcp'
    launcher.write_text('#!/bin/sh\nexec env '+ ' '.join(shlex.quote(k+'='+v) for k,v in launch_env.items())+' '+shlex.quote(sys.executable)+' '+shlex.quote(entry['args'][0])+' "$@"\n')
    launcher.chmod(0o755)
    # Windows clients launch WSL explicitly; the launcher carries the environment.
    desktop=entry
    if args.wsl_distribution:
        desktop={'command':'wsl.exe','args':['--distribution',args.wsl_distribution,'--exec',str(launcher)]}
    (output/'palette-launch.json').write_text(json.dumps(desktop,indent=2)+'\n')
    (output/'generic-mcp.json').write_text(json.dumps({'mcpServers':{'palette':desktop}},indent=2)+'\n')
    if not args.no_register:
        register(launcher)
    if not args.no_check:
        print('==> Checking one call: 1 + 1', flush=True)
        try:
            value = smoke(launcher)
        except Exception as exc:
            raise SystemExit(f'Palette is installed, but the test call failed: {exc}')
        if value != 2:
            raise SystemExit(f'Palette is installed, but 1 + 1 returned: {value!r}')
        print('    1 + 1 = 2. Palette is ready.')
    print(f'Client files: {output}\nstdio MCP command: {launcher}')

if __name__ == '__main__':
    main()
