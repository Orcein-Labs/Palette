# Generic toolchain provisioning

Palette can run a fixed host tool through the existing `host_command` broker.
The operator chooses the executable, working directory, fixed arguments,
artifacts, library directory, timeout, and whether the worker may supply one
plain text input. Palette adds no model identity, prompt template, or output
interpretation. The selected process receives a minimal environment without
the adapter's credentials.

Generate a command entry from operator-selected files:

```sh
python3 runtime/security/provisioning.py config \
  --name inference \
  --executable /opt/toolchain/bin/infer \
  --cwd /opt/models \
  --artifact /opt/models/model.bin \
  --fixed-arg=--threads=4 \
  --input-mode text \
  --input-prefix=--prompt \
  > /absolute/path/to/host-commands.json
export PALETTE_HOST_COMMANDS=/absolute/path/to/host-commands.json
```

The generated command fixes the binary and artifact paths. Text mode accepts
one string of at most 4096 bytes, with no leading option marker. It does not
accept caller-selected flags. Omit `--input-mode text` for a tool that takes no
worker-supplied input. The output file contains a `PALETTE_HOST_COMMANDS`
mapping and can be merged with other operator-owned commands.

The capability ceiling is a separate host-owned JSON file named by
`PALETTE_CAPABILITY_CEILING`. For example, allow one package and the runtime
registry bridge explicitly:

```json
{
  "package_management": {"allowed_packages": ["Parsers"], "offline": true},
  "host_request": {"allowed_types": ["palette.runtime"]}
}
```

The adapter reads and freezes this file at startup. `PALETTE_HOST_COMMANDS`
entries are merged into its host-command ceiling; conflicting names fail
closed. Requests cannot choose or change either configuration file.

Julia packages approved through `package_management` are installed by the
host broker into the durable store named by `PALETTE_PACKAGE_DEPOT` (default
`~/.palette/packages`). The store is outside session scratch and routed
workspaces. Workers mount it read-only, and their writable compiled-cache
depot remains session-local. The same package environment is reused after a
session or adapter restart. Worker networking stays disabled. The optional
`offline` flag defaults to `false` for broker-managed package fetches; set it
to `true` to require already prepared registry/source content. Every request
receives an audit receipt.

Use the command from Julia:

```julia
job = Palette.host_command("inference", "What is 6 times 7?"; wait=false)
result = Palette.host_command_poll(job["job"])
```

An unconfigured command or package is denied. Host commands run with the
adapter account's OS permissions, so only configure binaries and artifacts
that are safe to invoke under that account.
