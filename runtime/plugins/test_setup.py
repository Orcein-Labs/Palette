# SPDX-License-Identifier: Apache-2.0
"""setup.py against stand-ins: client CLIs that keep their servers in a file,
and a Palette server that answers the installation check."""
import importlib.util
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest
from unittest import mock

PLUGINS = Path(__file__).resolve().parent
REPO = PLUGINS.parents[1]
SETUP = PLUGINS/'setup.py'
spec = importlib.util.spec_from_file_location('palette_setup', SETUP)
setup = importlib.util.module_from_spec(spec)
spec.loader.exec_module(setup)

# One stand-in for claude, codex, grok and agy: it answers the commands
# setup.py runs in each CLI's own output format, keeping one server per CLI.
CLI = textwrap.dedent('''\
    #!{python}
    import json, os, sys
    from pathlib import Path
    name, args = Path(sys.argv[0]).name, sys.argv[1:]
    state = Path(os.environ['STUB_STATE'])/(name + '.json')
    command = json.loads(state.read_text())['command'] if state.exists() else None
    with open(Path(os.environ['STUB_STATE'])/'calls.log', 'a') as log:
        log.write(name + ' ' + ' '.join(args) + '\\n')
    def store(value):
        state.write_text(json.dumps({{'command': value}})) if value else state.unlink(missing_ok=True)
    if args[:1] != ['mcp']:
        sys.exit(2)
    verb = args[1]
    if verb == 'add':
        if command and name in ('claude', 'codex'):
            sys.exit(1)  # these two refuse to add over an existing server
        store(args[-1])
    elif verb == 'remove':
        if not command:
            sys.exit(1)
        store(None)
    elif name == 'claude' and verb == 'get':
        if not command:
            sys.exit(1)
        print(f'palette:\\n  Scope: User config\\n  Status: Connected\\n  Type: stdio\\n  Command: {{command}}\\n  Args: \\n')
    elif name == 'codex' and verb == 'get':
        if not command:
            sys.exit(1)
        print(json.dumps({{'name': 'palette', 'transport': {{'type': 'stdio', 'command': command, 'args': []}}}}))
    elif name == 'grok' and verb == 'list':
        others = [{{'name': 'other', 'command': '/bin/other', 'args': [], 'enabled': True, 'scope': 'user'}}]
        print(json.dumps(others + ([{{'name': 'palette', 'command': command, 'args': [], 'enabled': True, 'scope': 'user'}}] if command else [])))
    elif name == 'agy' and verb == 'list':
        print('NAME     TYPE   STATUS   COMMAND/URL')
        print('other    stdio  enabled  /bin/other')
        if command:
            print(f'palette  stdio  enabled  {{command}}')
    else:
        sys.exit(2)
''')

# A Palette server that keeps x between calls and answers TeamLab's status,
# or, with FAKE_PALETTE=forget, loses x between calls.
SERVER = textwrap.dedent('''\
    import json, os, sys
    if out := os.environ.get('FAKE_PALETTE_ENV'):
        open(out, 'w').write(json.dumps(dict(os.environ)))
    world = {}
    for line in sys.stdin:
        message = json.loads(line)
        if 'id' not in message:
            continue
        result = {}
        if message['method'] == 'tools/call':
            code = message['params']['arguments']['code']
            if code == 'x = 41':
                world['x'] = 41
                reply = {'data': 41}
            elif code == 'x + 1':
                reply = {'data': world['x'] + 1} if 'x' in world and os.environ.get('FAKE_PALETTE') != 'forget' \\
                    else {'error': 'UndefVarError: x not defined'}
            elif '"teamlab"' in code:
                reply = {'data': {'agent': 'palette-setup', 'dir': '/shop', 'pinned': [], 'new': [], 'listed': 0, 'unread': 0}}
            result = {'content': [{'type': 'text', 'text': json.dumps(reply)}]}
        print(json.dumps({'jsonrpc': '2.0', 'id': message['id'], 'result': result}), flush=True)
''')

CLIENTS = ('claude', 'codex', 'grok', 'agy')


def make_bin(directory):
    """Stand-in client CLIs, plus julia and bwrap that do nothing."""
    directory.mkdir(parents=True)
    for name in CLIENTS:
        (directory/name).write_text(CLI.format(python=sys.executable))
        (directory/name).chmod(0o755)
    for name in ('julia', 'bwrap'):
        (directory/name).write_text('#!/bin/sh\nexit 0\n')
        (directory/name).chmod(0o755)


def make_repo(root):
    """An SDK whose server is the stand-in above, with setup's real helpers."""
    security = root/'runtime/security'
    security.mkdir(parents=True)
    for name in ('provisioning.py', 'filesystem_layout.py', 'install_operator_plugin.py'):
        shutil.copyfile(REPO/'runtime/security'/name, security/name)
    (security/'palette_mcp_client.py').write_text('')
    (security/'serve_palette.py').write_text(SERVER)
    # Preparing starts real sessions, which read the ceiling and capabilities.
    (security/'prewarm_depot.py').write_text(
        'import os, pathlib, sys\n'
        'sys.exit(0 if pathlib.Path(os.environ["PALETTE_CAPABILITY_CEILING"]).is_file()\n'
        '         and (pathlib.Path(os.environ["PALETTE_RUNTIME_ROOT"])/"teamlab").is_dir() else 3)\n')
    shutil.copytree(REPO/'runtime/integrations/teamlab', root/'runtime/integrations/teamlab')
    shutil.copytree(REPO/'runtime/plugins/palette', root/'runtime/plugins/palette')
    shutil.copyfile(REPO/'Project.toml', root/'Project.toml')
    (root/'Manifest.toml').write_text('')
    host = root/'runtime/host/target/release/palette-host'
    host.parent.mkdir(parents=True)
    host.write_text('#!/bin/sh\n')
    host.chmod(0o755)
    return root


class SetupTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix='palette-setup-'))
        self.addCleanup(shutil.rmtree, self.tmp)
        self.state = self.tmp/'clients'
        self.state.mkdir()
        make_bin(self.tmp/'bin')

    def calls(self):
        log = self.state/'calls.log'
        return log.read_text().splitlines() if log.exists() else []

    def registered(self, client):
        path = self.state/f'{client}.json'
        return json.loads(path.read_text())['command'] if path.exists() else None

    def test_ceiling_is_created_or_added_to(self):
        ceiling = self.tmp/'config/ceiling.json'
        setup.merge_ceiling(REPO, ceiling)
        self.assertEqual(json.loads(ceiling.read_text()), {'host_request': {'allowed_types': ['palette.runtime']}})
        self.assertEqual(ceiling.stat().st_mode & 0o777, 0o600)
        own = {'host_request': {'allowed_types': ['other.type']}, 'network_access': {'allowed': True, 'allowed_hosts': ['example.org']}}
        ceiling.write_text(json.dumps(own))
        setup.merge_ceiling(REPO, ceiling)
        merged = json.loads(ceiling.read_text())
        self.assertEqual(merged['host_request']['allowed_types'], ['other.type', 'palette.runtime'])
        self.assertEqual(merged['network_access'], own['network_access'])
        # A ceiling that already grants it is not rewritten.
        before = ceiling.stat().st_mtime_ns
        setup.merge_ceiling(REPO, ceiling)
        self.assertEqual(ceiling.stat().st_mtime_ns, before)
        for broken in ('{not json', json.dumps({'no_such_category': {}})):
            ceiling.write_text(broken)
            with self.assertRaises(SystemExit):
                setup.merge_ceiling(REPO, ceiling)
            self.assertEqual(ceiling.read_text(), broken)

    def test_capabilities_are_replaced_and_others_kept(self):
        root = self.tmp/'capabilities'
        (root/'teamlab').mkdir(parents=True)
        (root/'teamlab/stale.jl').write_text('')
        (root/'gesso').mkdir()
        (root/'gesso/capability.json').write_text('{}')
        setup.deploy_capabilities(REPO, root)
        source = REPO/'runtime/integrations/teamlab'
        manifest = json.loads((source/'capability.json').read_text())
        self.assertEqual(sorted(p.name for p in (root/'teamlab').iterdir()), sorted(['capability.json', manifest['source']]))
        self.assertEqual((root/'teamlab'/manifest['source']).read_text(), (source/manifest['source']).read_text())
        self.assertTrue((root/'gesso/capability.json').exists())
        self.assertEqual(sorted(p.name for p in root.iterdir()), ['gesso', 'teamlab'])

    def test_registration_adds_reuses_reports_and_replaces(self):
        launcher = '/opt/palette/client/palette-mcp'
        env = {'PATH': f'{self.tmp/"bin"}:/usr/bin:/bin', 'STUB_STATE': str(self.state)}
        with mock.patch.dict(os.environ, env), mock.patch('sys.stdout'):
            os.environ.pop('WSL_DISTRO_NAME', None)
            setup.register(launcher)
            self.assertEqual([self.registered(c) for c in CLIENTS], [launcher]*4)
            first = len(self.calls())
            setup.register(launcher)
            self.assertFalse([c for c in self.calls()[first:] if ' add ' in f' {c} '], 'a second run must not add again')
            other = '/home/me/rnd/palette/palette-mcp'
            for client in CLIENTS:
                (self.state/f'{client}.json').write_text(json.dumps({'command': other}))
            setup.register(launcher)
            self.assertEqual([self.registered(c) for c in CLIENTS], [other]*4, 'another server is left alone')
            setup.register(launcher, replace=True)
            self.assertEqual([self.registered(c) for c in CLIENTS], [launcher]*4)
        # The other servers grok and agy list are never touched.
        self.assertFalse([c for c in self.calls() if 'other' in c])

    def test_installation_switches_only_after_its_check_passes(self):
        home = self.tmp/'home'
        first = make_repo(self.tmp/'sdk-1')
        env_out = self.tmp/'server-env.json'
        env = {'HOME': str(home), 'PATH': f'{self.tmp/"bin"}:/usr/bin:/bin', 'STUB_STATE': str(self.state),
               'FAKE_PALETTE_ENV': str(env_out), 'PALETTE_TEAMLAB_DIR': ''}
        env = {k: v for k, v in env.items() if v}

        def run(repo, *extra, **more):
            return subprocess.run([sys.executable, str(SETUP), '--repo-dir', str(repo), *extra],
                                  env=dict(env, **more), capture_output=True, text=True, timeout=120)

        done = run(first)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertIn('Preparing the sandbox', done.stdout)
        self.assertIn('x + 1 = 42', done.stdout)
        self.assertIn('TeamLab: 0 listings', done.stdout)
        palette = home/'.local/share/palette'
        launcher = palette/'client/palette-mcp'
        self.assertEqual([self.registered(c) for c in CLIENTS], [str(launcher)]*4)
        self.assertIn(str(first), launcher.read_text())
        self.assertTrue((palette/'capabilities/teamlab/capability.json').is_file())
        self.assertEqual(json.loads((palette/'config/ceiling.json').read_text())['host_request']['allowed_types'], ['palette.runtime'])
        served = json.loads(env_out.read_text())
        self.assertEqual(served['PALETTE_RUNTIME_ROOT'], str(palette/'capabilities'))
        self.assertEqual(served['PALETTE_CAPABILITY_CEILING'], str(palette/'config/ceiling.json'))
        self.assertNotIn('PALETTE_TEAMLAB_DIR', served)
        self.assertNotIn('PALETTE_HOST_COMMANDS', served)
        self.assertNotIn('PALETTE_READ_ROOTS', served)
        self.assertEqual(sorted(p.name for p in (palette/'client').iterdir()),
                         ['generic-mcp.json', 'palette-launch.json', 'palette-local-plugin', 'palette-mcp'])

        # An update whose server loses state between calls is not switched on.
        second = make_repo(self.tmp/'sdk-2')
        failed = run(second, FAKE_PALETTE='forget')
        self.assertNotEqual(failed.returncode, 0)
        self.assertIn('was not switched on', failed.stderr)
        self.assertIn(str(first), launcher.read_text())
        self.assertNotIn(str(second), (palette/'client/palette-launch.json').read_text())

        # Host commands are passed on when the operator's file exists.
        commands = home/'.config/palette/host-commands.json'
        commands.parent.mkdir(parents=True)
        commands.write_text('{}')
        done = run(second)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertIn(str(second), launcher.read_text())
        self.assertEqual(json.loads(env_out.read_text())['PALETTE_HOST_COMMANDS'], str(commands))

        # So are read roots, one a line; a relative or missing one is refused.
        readable = [self.tmp/'project one', self.tmp/'project-two']
        for path in readable:
            path.mkdir()
        roots = home/'.config/palette/read-roots'
        for bad in ('relative/dir', str(self.tmp/'missing')):
            roots.write_text(bad + '\n')
            refused = run(second, '--no-check')
            self.assertNotEqual(refused.returncode, 0)
            self.assertIn('read root', refused.stderr)
        roots.write_text(f'# projects the kernel reads\n{readable[0]}\n\n  {readable[1]}  \n')
        done = run(second)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(json.loads(env_out.read_text())['PALETTE_READ_ROOTS'], ':'.join(map(str, readable)))
        self.assertIn(shlex.quote(f'PALETTE_READ_ROOTS={readable[0]}:{readable[1]}'), launcher.read_text())
        self.assertFalse([p for p in (palette/'client').iterdir() if p.name.startswith('.')], 'no trial files left behind')


if __name__ == '__main__':
    unittest.main()
