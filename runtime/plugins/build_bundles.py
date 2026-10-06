#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Build reproducible client bundles from an explicit committed Palette revision."""
import argparse
import hashlib
import io
import json
from pathlib import Path
import subprocess
import tarfile
import zipfile

ROOT = Path(__file__).resolve().parents[2]


def validate_plugin_archive(data, *, desktop=False):
    """Reject ambiguous client packages before handing them to an uploader."""
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        names = archive.namelist()
        if archive.testzip() is not None:
            raise ValueError('Corrupt client archive')
        if any(n.endswith('.zip') for n in names):
            raise ValueError('Client archive must not contain nested ZIPs')
        if desktop:
            if 'manifest.json' not in names or 'server/index.cjs' not in names:
                raise ValueError('Desktop MCPB requires its root manifest and bridge')
            return
        manifests = [n for n in names if Path(n).name == 'plugin.json']
        if len(manifests) != 1:
            raise ValueError('Expected a single plugin archive')
        manifest = Path(manifests[0])
        root = manifest.parent.parent if manifest.parent.name.startswith('.') else manifest.parent
        if str(root) != '.' and (len(root.parts) != 1 or
                               any(not n.startswith(str(root)+'/') for n in names)):
            raise ValueError('Plugin root must have no sibling files')


def zip_bytes(files):
    stream = io.BytesIO()
    with zipfile.ZipFile(stream, 'w', compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        for name, data in sorted(files.items()):
            info = zipfile.ZipInfo(name, (2026, 10, 4, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o100644 << 16
            archive.writestr(info, data)
    return stream.getvalue()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--revision', default='HEAD')
    parser.add_argument('--output-dir', type=Path, required=True)
    args = parser.parse_args()
    revision = subprocess.check_output(['git','rev-parse',args.revision+'^{commit}'],cwd=ROOT,text=True).strip()
    raw = subprocess.check_output(['git','archive','--format=tar',revision],cwd=ROOT)
    with tarfile.open(fileobj=io.BytesIO(raw)) as archive:
        source = {m.name: archive.extractfile(m).read() for m in archive if m.isfile()}
    assert not any('.archive' in Path(n).parts or '.git' in Path(n).parts for n in source)
    version=json.loads(source['runtime/plugins/palette/plugin.json'])['version']
    inherited=source['runtime/licenses/MIT-INHERITED.txt']
    license_text=source['runtime/licenses/Apache-2.0.txt']
    # A client upload must declare exactly one plugin. The SDK's repository
    # marketplace and client manifests are authoring files, not nested plugins.
    excluded = {'.agents/plugins/marketplace.json',
                'Palette-Launch-Bundles.zip',
                'runtime/plugins/palette/plugin.json',
                'runtime/plugins/claude/manifest.json'}
    shared={
        **{'sdk/'+n:d for n,d in source.items() if n not in excluded},
        'templates/palette-plugin.json':source['runtime/plugins/palette/plugin.json'],
        'setup.py':source['runtime/plugins/setup.py'],
        'INSTALL.md':source['runtime/plugins/DISTRIBUTION.md'],
        'BRAIN_BLAST.md':source['BRAIN_BLAST.md'].replace(b'(runtime/docs/', b'(sdk/runtime/docs/'),
        'LICENSE':license_text,
        'NOTICE':source['NOTICE'],
        'licenses/AGPL-3.0-only.txt':source['LICENSE'],
        'licenses/MIT-INHERITED.txt':inherited,
        'licenses/BOUNDARIES.md':source['runtime/licenses/README.md'],
        'SOURCE.json':json.dumps({'repository':'https://github.com/latentcollapse/Palette.jl','revision':revision,'plugin_version':version,'runtime':'source; prepare on Linux or WSL2','extra_language_toolchains':False,'sdk_omitted_authoring_manifests':sorted(excluded)},indent=2).encode()+b'\n'
    }
    claude={n.removeprefix('runtime/plugins/claude/'):d for n,d in source.items() if n.startswith('runtime/plugins/claude/')}
    claude.update({'LICENSE':license_text,'NOTICE':source['NOTICE'],'MIT-INHERITED.txt':inherited})
    mcpb=zip_bytes({**shared,**claude})
    portable={n.removeprefix('runtime/plugins/palette/'):d for n,d in source.items() if n.startswith('runtime/plugins/palette/')}
    portable['README.md']=source['runtime/plugins/DISTRIBUTION.md']
    portable_manifest=json.loads(portable['plugin.json'])
    claude_plugin_manifest={key:portable_manifest[key] for key in
                            ('name','version','description','author','license','repository')}
    claude_plugin_manifest.update(mcpServers='./.mcp.json', skills='./skills/')
    claude_plugin={**shared,
        **{n:d for n,d in portable.items() if n.startswith(('skills/','assets/'))},
        '.claude-plugin/plugin.json':json.dumps(claude_plugin_manifest,indent=2).encode()+b'\n',
        '.mcp.json':json.dumps({'mcpServers':{'palette':{'command':'palette-mcp'}}},indent=2).encode()+b'\n'}
    outputs={'Palette-Claude.mcpb':mcpb,'Palette-Claude.zip':zip_bytes(claude_plugin),
             'Palette-ChatGPT.zip':zip_bytes({'palette/'+n:d for n,d in {**shared,**portable}.items()})}
    args.output_dir.mkdir(parents=True,exist_ok=True)
    checks=[]
    for name,data in outputs.items():
        validate_plugin_archive(data, desktop=name.endswith('.mcpb'))
        (args.output_dir/name).write_bytes(data)
        with zipfile.ZipFile(io.BytesIO(data)) as archive:
            assert archive.testzip() is None
        checks.append(hashlib.sha256(data).hexdigest()+'  '+name)
    (args.output_dir/'SHA256SUMS.txt').write_text('\n'.join(checks)+'\n')
    print(json.dumps({'revision':revision,'artifacts':list(outputs),'output_dir':str(args.output_dir.resolve())},indent=2))

if __name__ == '__main__':
    main()
