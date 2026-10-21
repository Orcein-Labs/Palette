# Palette licensing boundaries

Palette Core is distributed under **AGPL-3.0-only**, not "or later". The root
LICENSE contains version 3 of the GNU Affero General Public License. This is
an outgoing licensing policy for this distribution; it does not revoke licenses
already granted for earlier releases or change third-party ownership.

| Component | License for Palette-owned material |
| --- | --- |
| Julia kernel and operator, `src/` | AGPL-3.0-only |
| Supervised Rust host, `runtime/host/` | AGPL-3.0-only |
| Julia worker, `runtime/scripts/session_loop.jl` | AGPL-3.0-only |
| Broker, authority, execution and revival machinery in `runtime/security/` | AGPL-3.0-only |
| Client packages, installers and packaging helpers under `runtime/plugins/` | Apache-2.0 |
| Local client installation: `runtime/security/install_operator_plugin.py` | Apache-2.0 |
| Public API/protocol/install docs, `runtime/docs/`, and root README/contribution/security/briefing docs | Apache-2.0 |
| Tests and other source/tooling not explicitly excepted above | AGPL-3.0-only |

The MCP execution server and broker are Core. An Apache-licensed client launcher
calling that server does not relicense the server. Future standalone plugin SDK,
client libraries and community plugin templates are intended to use Apache-2.0;
this distribution does not pretend those future projects already exist.

`Apache-2.0.txt` contains the permissive component license.
`MIT-INHERITED.txt` preserves the original MIT grant and copyright notices for
inherited material from Mario Zechner and Prime Intellect, and the earlier MattC
release. Third-party portions retain their original permissions and notices.
Dependencies have their own licenses and are not relicensed by this policy.

Palette-owned additions and the combined Core distribution use the stated
outgoing license. No exclusive ownership of inherited material is claimed.
Palette names and logos are identifiers; these software licenses do not grant
trademark rights. No trademark registration or commercial licensing program is
asserted here.
