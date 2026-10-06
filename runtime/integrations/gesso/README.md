# Optional Gesso capability

This integration exposes the existing Gesso importer, GPT-2 tokenizer, CPU
Session engine, fork operation, and receipts to a Palette Julia worker. Gesso
stays out of Palette's `Project.toml`; the trusted installer adds a copied
Gesso package only to Palette's separate optional package environment.

Run the installer as an operator action with the already-present Gesso source,
local SmolLM2 snapshot, Palette package store, model artifact store, and
capability registry paths:

```sh
python3 runtime/integrations/gesso/install.py \
  --gesso-source '/path/to/Gesso' \
  --model-source '/path/to/Gesso/snapshots/SmolLM2-135M' \
  --package-store '/var/lib/palette/packages' \
  --artifact-store '/var/lib/palette/gesso-models' \
  --runtime-root '/opt/palette-capabilities'
```

The install requires a clean committed Gesso `Project.toml`, `src/`, and `ext/`
frontier, copies those tracked package files and the seven fixed checkpoint
files, verifies SHA-256 inventories, and calls `Pkg.develop` with Julia package
offline mode enabled. It does not mutate Gesso, download models, or modify the
Palette project. The output names the installed artifact directory; add that
exact directory to the trusted server profile's `read_roots`. Keep the model
artifact directory and optional package environment server-owned and read-only
to workers.

From a Palette Julia worker, discovery lists `gesso`; invoke it through the
existing runtime ABI:

```julia
Palette.runtime("discover")
Palette.runtime("invoke", Dict("id" => "gesso",
    "arguments" => Dict("action" => "diagnose")))
Palette.runtime("invoke", Dict("id" => "gesso",
    "arguments" => Dict("action" => "generate", "prompt" => "Hello",
                         "max_new_tokens" => 2)))
```

Supported actions are `diagnose`, `load`, `tokenize`, `decode`, `generate`,
`fork`, `profile`, `cuda`, and `lava`. The model identity and artifact path are fixed at install time.
Generation accepts a 1–16 token limit and up to 2048 UTF-8 bytes of prompt.
After installing or updating optional packages, prepare the server profile so
new workers inherit compatible package caches. After a worker restart, run
`diagnose`, then `load`, then `generate` as separate Palette turns. Diagnostics
initialize the package; loading constructs the model. Combining cold package
initialization, model loading, and generation can exceed the host's turn limit
and stop the worker. Model objects are loaded again after revival; the saved
research world and broker receipts persist independently.

Generation returns actual generated 0-based token IDs, Gesso-decoded text,
Gesso engine receipts, and page-table profiling without returning the prompt.
Tokenize/decode requests are limited to 128 IDs/tokens. `fork` prefills one prompt, creates two
Gesso prefix-sharing children, decodes each branch once, and reports Gesso KV
measurements.

CPU is the verified execution path. CUDA and Lava diagnostics report optional
package, extension, and device-construction outcomes. Requesting an unavailable
backend raises Gesso's typed resource-limit error; the adapter never retries on
CPU. This integration makes no latency, quality, or hardware-support claim.

The operator can run `probe.jl` from the installed package environment with the
expected package source SHA/revision and copied artifact root to perform a
source-gated, real CPU generation check. A requested CUDA or Lava probe raises
Gesso's typed resource-limit error if its package, extension, or usable device
is unavailable; the probe never substitutes CPU execution.
