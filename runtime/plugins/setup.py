#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Prepare Palette on Linux, register it with local clients, and check it works."""
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
HOME = Path(os.environ.get('PALETTE_HOME') or Path(os.environ.get('XDG_DATA_HOME') or Path.home()/'.local/share')/'palette')
CONFIG = Path(os.environ.get('XDG_CONFIG_HOME') or Path.home()/'.config')/'palette'
SYSTEM_PATH = '/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin'
# The capabilities every installation deploys into its runtime root.
BUNDLED_CAPABILITIES = ('teamlab',)


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


def write_atomic(path, text, mode=0o644):
    """Replace a file in one rename, so a client never starts a half-written one."""
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f'.{path.name}.{os.getpid()}')
    temporary.write_text(text)
    temporary.chmod(mode)
    os.replace(temporary, path)


def deploy_capabilities(repo, runtime_root):
    """Copy the bundled capabilities into the runtime root, leaving any other
    capability there as it is."""
    for name in BUNDLED_CAPABILITIES:
        source = repo/'runtime/integrations'/name
        manifest = json.loads((source/'capability.json').read_text())
        staged = runtime_root/f'.{name}.{os.getpid()}'
        shutil.rmtree(staged, ignore_errors=True)
        staged.mkdir(parents=True)
        for file in ('capability.json', manifest['source']):
            shutil.copyfile(source/file, staged/file)
        target, retired = runtime_root/name, runtime_root/f'.{name}.retired.{os.getpid()}'
        if target.exists():
            target.rename(retired)
        staged.rename(target)
        shutil.rmtree(retired, ignore_errors=True)


def merge_ceiling(repo, path):
    """Write the capability ceiling, or add to the operator's own: a kernel
    reaches the runtime's capabilities (TeamLab among them) through the
    palette.runtime host request. Nothing else in an existing file changes."""
    sys.path.insert(0, str(repo/'runtime/security'))
    from provisioning import OperatorConfigError, validate_ceiling
    try:
        ceiling = json.loads(path.read_text()) if path.exists() else {}
        validate_ceiling(ceiling)
    except (ValueError, OperatorConfigError) as exc:
        raise SystemExit(f'The capability ceiling {path} is not valid ({exc}); fix or move it and run setup again')
    grant = ceiling.setdefault('host_request', {})
    types = grant.setdefault('allowed_types', [])
    if 'palette.runtime' in types and path.exists():
        return
    if 'palette.runtime' not in types:
        types.append('palette.runtime')
    write_atomic(path, json.dumps(validate_ceiling(ceiling), indent=2)+'\n', 0o600)


def _claude_command(stdout):
    return next((line.split(':', 1)[1].strip() for line in stdout.splitlines()
                 if line.strip().startswith('Command:')), '')


def _codex_command(stdout):
    return json.loads(stdout)['transport'].get('command', '')


def _grok_command(stdout):
    return next((s.get('command', '') for s in json.loads(stdout)
                 if s.get('name') == 'palette' and s.get('scope', 'user') == 'user'), None)


def _agy_command(stdout):
    # NAME  TYPE  STATUS  COMMAND/URL
    return next((line.split(None, 3)[3] for line in stdout.splitlines()[1:]
                 if line.split(None, 1)[:1] == ['palette'] and len(line.split(None, 3)) == 4), None)


# Each client: how to read the command a server named palette runs (None when
# there is none), how to add one and how to remove one.
CLIENTS = {
    'Claude Code': (['claude', 'mcp', 'get', 'palette'], _claude_command,
                    ['claude', 'mcp', 'add', '--scope', 'user', 'palette', '--'], ['claude', 'mcp', 'remove', 'palette']),
    'Codex': (['codex', 'mcp', 'get', 'palette', '--json'], _codex_command,
              ['codex', 'mcp', 'add', 'palette', '--'], ['codex', 'mcp', 'remove', 'palette']),
    'Grok': (['grok', 'mcp', 'list', '--json'], _grok_command,
             ['grok', 'mcp', 'add', '-s', 'user', 'palette'], ['grok', 'mcp', 'remove', '-s', 'user', 'palette']),
    'Antigravity': (['agy', 'mcp', 'list'], _agy_command,
                    ['agy', 'mcp', 'add', 'palette'], ['agy', 'mcp', 'remove', 'palette']),
}


def _run(command):
    return subprocess.run(command, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=120)


def registered_command(get, parse):
    """The command a client's palette server runs, or None when it has none."""
    found = _run(get)
    # `claude mcp get` and `codex mcp get` exit nonzero for an unknown name.
    if found.returncode != 0:
        return None
    try:
        return parse(found.stdout)
    except (ValueError, KeyError, TypeError):
        return ''


def register(launcher, replace=False):
    """Add the server to each local client that is installed. A client that
    already runs this launcher is left alone; one whose palette server runs
    something else keeps it unless replace is set."""
    for client, (get, parse, add, remove) in CLIENTS.items():
        found = shutil.which(add[0])
        # Inside WSL, PATH also lists Windows programs; install.ps1 registers those.
        if not found or (os.environ.get('WSL_DISTRO_NAME') and found.startswith('/mnt/')):
            continue
        add = add + [str(launcher)]
        existing = registered_command(get, parse)
        if existing == str(launcher):
            print(f'{client} already uses this installation.')
            continue
        if existing is not None and not replace:
            print(f'{client} already has a server named palette ({existing or "unreadable"}); left as it is. '
                  f'To use this one, run setup again with --replace, or: {shlex.join(remove)}; {shlex.join(add)}')
            continue
        if existing is not None:
            _run(remove)
        if _run(add).returncode == 0 and registered_command(get, parse) == str(launcher):
            print(f'{"Moved" if existing is not None else "Registered"} {client} to this installation.')
        else:
            print(f'Could not register with {client}; run: {shlex.join(add)}')


class Server:
    """An installed server spoken to over MCP stdio, as a client would."""

    def __init__(self, launcher, timeout=600):
        # The server reports its progress on stderr; it is shown only on failure.
        self.log = tempfile.TemporaryFile('w+')
        self.process = subprocess.Popen([str(launcher)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=self.log, text=True)
        self.timer = threading.Timer(timeout, self.process.kill)
        self.timer.start()
        self.ids = iter(range(1, 1 << 30))
        self.request('initialize', {'protocolVersion': '2024-11-05', 'capabilities': {},
                                    'clientInfo': {'name': 'palette-setup', 'version': '1'}})
        self.send({'jsonrpc': '2.0', 'method': 'notifications/initialized'})

    def send(self, message):
        self.process.stdin.write(json.dumps(message)+'\n')
        self.process.stdin.flush()

    def request(self, method, params):
        ident = next(self.ids)
        self.send({'jsonrpc': '2.0', 'id': ident, 'method': method, 'params': params})
        for line in self.process.stdout:
            reply = json.loads(line)
            if reply.get('id') == ident:
                if 'error' in reply:
                    raise RuntimeError(reply['error'].get('message', reply['error']))
                return reply['result']
        raise RuntimeError('the server exited before it answered')

    def evaluate(self, code):
        """The value of one palette call."""
        result = self.request('tools/call', {'name': 'palette', 'arguments': {'code': code}})
        text = ''.join(c.get('text', '') for c in result.get('content', []))
        try:
            reply = json.loads(text)
        except ValueError:
            raise RuntimeError(f'unexpected reply: {text[:500]}')
        if not isinstance(reply, dict) or 'data' not in reply:
            raise RuntimeError(f'the call failed: {text[:500]}')
        return reply['data']

    def close(self, failed=False):
        self.timer.cancel()
        self.process.kill()
        self.process.wait()
        if failed:
            self.log.seek(0)
            print(self.log.read()[-4000:], end='', file=sys.stderr)
        self.log.close()


def check(launcher):
    """Evaluate x = 41, then x + 1 in a second call, and read the TeamLab shop."""
    print('==> Checking the installation: x = 41, then x + 1', flush=True)
    server = Server(launcher)
    try:
        server.evaluate('x = 41')
        if (value := server.evaluate('x + 1')) != 42:
            raise RuntimeError(f'x + 1 returned {value!r}, not 42: the second call did not see x')
        print('    x + 1 = 42: state persists between calls.')
        status = server.evaluate('Palette.runtime("invoke", Dict("id"=>"teamlab", '
                                 '"arguments"=>Dict{String,Any}("action"=>"status")))["result"]')
        if not isinstance(status, dict) or 'listed' not in status:
            raise RuntimeError(f'TeamLab status returned {status!r}')
        print(f'    TeamLab: {status["listed"]} listings in the shop at {status["dir"]}.')
    except Exception:
        server.close(failed=True)
        raise
    server.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo-dir', type=Path, default=DEFAULT_REPO)
    parser.add_argument('--output-dir', type=Path, default=HOME/'client')
    parser.add_argument('--data-dir', type=Path, default=HOME/'data')
    parser.add_argument('--depot-dir', type=Path, default=HOME/'depot')
    parser.add_argument('--runtime-root', type=Path, default=HOME/'capabilities',
                        help='Capability registry (PALETTE_RUNTIME_ROOT); TeamLab is deployed into it')
    parser.add_argument('--ceiling', type=Path, default=HOME/'config/ceiling.json',
                        help='Capability ceiling (PALETTE_CAPABILITY_CEILING); created, or added to')
    parser.add_argument('--host-commands', default=str(CONFIG/'host-commands.json'),
                        help='Host commands file (PALETTE_HOST_COMMANDS), used when it exists; empty for none')
    parser.add_argument('--connect-only', action='store_true', help='Use a runtime already built and prewarmed; do not prepare again')
    parser.add_argument('--wsl-distribution', help='Generate Windows Claude configuration targeting this WSL distribution')
    parser.add_argument('--no-register', action='store_true', help='Do not add the server to local agent clients')
    parser.add_argument('--replace', action='store_true', help='Take over a client\'s palette server that runs something else')
    parser.add_argument('--no-check', action='store_true', help='Do not make test calls at the end')
    args = parser.parse_args()
    if sys.platform != 'linux':
        parser.error('Run setup in Linux or WSL2; the supervised host requires bubblewrap')
    repo, output, data, depot, runtime_root, ceiling = (p.resolve() for p in (
        args.repo_dir, args.output_dir, args.data_dir, args.depot_dir, args.runtime_root, args.ceiling))
    host_commands = Path(args.host_commands).expanduser().resolve() if args.host_commands else None
    if host_commands and not host_commands.is_file():
        host_commands = None
    for command in ('julia', 'bwrap'):
        if not shutil.which(command):
            parser.error(f'missing prerequisite on PATH: {command}')
    if not (repo / 'Project.toml').is_file():
        parser.error(f'not a Palette SDK: {repo}')
    for private in (data, depot, output, runtime_root, ceiling):
        if private == repo or repo in private.parents:
            parser.error('Keep data, depot, capabilities and generated configuration outside the SDK tree')
    workspace = (data/'workspace').resolve()
    for protected in (repo, depot, output, runtime_root, ceiling, (data/'state').resolve(), (data/'worlds').resolve()):
        if workspace == protected or workspace.is_relative_to(protected) or protected.is_relative_to(workspace):
            parser.error('Workspace must not overlap source, depot, capabilities, configuration, or host state')
    host = repo / 'runtime/host/target/release/palette-host'
    # The client starts the server with this environment, not the shell's, so
    # it names the Julia found now and keeps packages beside the other data.
    launch_env = {'PALETTE_REPO':str(repo),'PALETTE_HOST':str(host),'PALETTE_DATA_HOME':str(data),
                  'PALETTE_STATE_DIR':str(data/'state'),'PALETTE_WORKSPACE_STATE_ROOT':str(data/'worlds'),
                  'OPERATOR_WORKSPACE':str(data/'workspace'),'PALETTE_PACKAGE_DEPOT':str(data/'packages'),
                  'PALETTE_RUNTIME_ROOT':str(runtime_root),'PALETTE_CAPABILITY_CEILING':str(ceiling),
                  **({'PALETTE_HOST_COMMANDS':str(host_commands)} if host_commands else {}),
                  'JULIA_DEPOT_PATH':str(depot),'JULIA_PKG_OFFLINE':'true',
                  'PATH':str(Path(shutil.which('julia')).parent)+':'+SYSTEM_PATH}
    # Preparing starts sessions under the environment the server gets, so the
    # capabilities and ceiling it names exist first.
    deploy_capabilities(repo, runtime_root)
    merge_ceiling(repo, ceiling)
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
    launcher = output/'palette-mcp'
    entry = {'command':sys.executable,'args':[str(repo/'runtime/security/serve_palette.py')],'env':launch_env}
    script = ('#!/bin/sh\n# Palette-managed launcher\nexec env '+' '.join(shlex.quote(k+'='+v) for k,v in launch_env.items())
              +' '+shlex.quote(sys.executable)+' '+shlex.quote(entry['args'][0])+' "$@"\n')
    if not args.no_check:
        # Check the new launcher before it replaces the old one, so a failed
        # update leaves the installation that worked.
        trial = output/f'.palette-mcp.trial.{os.getpid()}'
        write_atomic(trial, script, 0o755)
        try:
            check(trial)
        except Exception as exc:
            raise SystemExit(f'The check failed, so this installation was not switched on: {exc}')
        finally:
            trial.unlink(missing_ok=True)
    write_atomic(launcher, script, 0o755)
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
    config_path=plugin/'mcp.json'
    config=json.loads(config_path.read_text());config['mcpServers']['palette']={'type':'stdio',**entry}
    config_path.write_text(json.dumps(config,indent=2)+'\n')
    # Windows clients launch WSL explicitly; the launcher carries the environment.
    desktop=entry
    if args.wsl_distribution:
        desktop={'command':'wsl.exe','args':['--distribution',args.wsl_distribution,'--exec',str(launcher)]}
    (output/'palette-launch.json').write_text(json.dumps(desktop,indent=2)+'\n')
    (output/'generic-mcp.json').write_text(json.dumps({'mcpServers':{'palette':desktop}},indent=2)+'\n')
    if not args.no_register:
        register(launcher, replace=args.replace)
    if host_commands:
        print(f'Host commands: {host_commands}')
    print(f'Client files: {output}\nstdio MCP command: {launcher}')

if __name__ == '__main__':
    main()
