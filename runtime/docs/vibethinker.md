# Local VibeThinker-3B

Palette can run an operator-provisioned VibeThinker-3B GGUF with llama.cpp through
the existing `host_command` capability. The model runs on the host and returns
text to Julia. Install a compatible `llama-cli` and obtain the model separately;
weights and inference binaries are not included in the SDK or plugin archive.

Generate a command entry using your actual host paths:

```sh
python3 runtime/security/vibethinker.py config \
  --llama-cli /absolute/path/to/llama-cli \
  --model /absolute/path/to/VibeThinker-3B-Q8_0.gguf \
  --library-dir /absolute/path/to/llama-libraries \
  > /absolute/path/to/vibethinker-commands.json
export PALETTE_HOST_COMMANDS=/absolute/path/to/vibethinker-commands.json
```

Omit `--library-dir` if your installation already resolves its libraries.
The generated object can be merged with existing host-command entries before
selecting it with `PALETTE_HOST_COMMANDS`. Restart the adapter to load the
configuration. Each entry fixes the binary, model, context, token count,
threads, GPU layers and timeout; the kernel supplies one prompt only.
CPU execution is the default. To enable an available host GPU, generate the
configuration with `--gpu-layers 99`. No GPU device mount into Julia is needed.

In a Palette call, start inference asynchronously so model loading and generation
do not consume the interactive turn deadline:

```julia
vibe_job = Palette.host_command("vibethinker", "What is 6 times 7?"; wait=false)
```

In a later call, inspect completion:

```julia
vibe_result = Palette.host_command_poll(vibe_job["job"])
```

While running, the result contains `running => true`. On completion inspect
`returncode`, `timed_out`, `stdout` and `stderr`. Output is the broker's bounded
text projection (last 16 KiB); reaching the configured token limit can leave
an answer incomplete. Defaults are 512 generated tokens, a 4096-token context,
four CPU threads and a 180-second broker timeout. Change these at configuration
time when the task needs a larger reasoning budget. An unconfigured command is
denied. Closing the owning session terminates its inference process; jobs and
model memory do not survive Julia revival. Model output is returned as text
and is not executed as Julia or shell code.

The runner uses VibeThinker's ChatML template, accepts prompts up to 4096 bytes,
and replaces itself with `llama-cli` so the existing broker owns process cleanup.
It passes a minimal environment without model API or tunnel credentials.

For an older active deployment whose package is still named `Neura`, retain its
runtime and saved worlds until portable data has been exported and restoration
under `Palette` has been checked. See [runtime identity changes](install.md#prelaunch-runtime-identity-change).
