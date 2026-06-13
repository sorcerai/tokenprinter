# tokenprinter

Print **AI coding-agent token-usage receipts** on a thermal printer. Point it at your Claude Code / Codex / pi / Antigravity session logs and it renders a paper receipt — per-model token breakdown, real cost math, tool activity, productivity stats, and a scannable QR — on a Star **TSP654** (or any CUPS/ESC-raster receipt printer).

Inspired by [`chrishutchinson/claude-receipts`](https://github.com/chrishutchinson/claude-receipts), rebuilt in Rust and extended to be multi-agent, cost-accurate, and auto-triggering.

```
        ANTHROPIC
================================================
          TOKEN PRINTER
================================================
 Agent                          Claude Code
 Location                Somewhere on Earth
 Session   a9e3d8e9-584a-4f19-a960-e5bc661…
 Project              ~/repos/tokenprinter (HEAD)
 Date                   2026-06-13 16:21:53
 Duration                        12h 58m 00s
------------------------------------------------
 MODEL BREAKDOWN
------------------------------------------------
 claude-opus-4-8
   Input tokens                       86,296
   Output tokens                     711,885
   Cache write                     2,209,699
   Cache read                    118,126,944
   Subtotal                            $91.10
------------------------------------------------
 TOOL ACTIVITY                    (130 calls)
------------------------------------------------
   Bash      ███████████              80
   Agent     ███                      21
   Read      █                         9
   ...
------------------------------------------------
 PRODUCTIVITY
   Files changed                          36
   Lines               +7,740 / -212
   Commits                                32
------------------------------------------------
 TOKENS OVER TIME
   ▁▂▃▅▇█▆▄▃▂▁▂▄▇█▅▃▂▁▁
================================================
 SUBTOTAL                              $91.10
 Cache savings                       -$531.57
 Sales tax (vibes, 0%)                  $0.00
================================================
 TOTAL                                 $91.10
   API-equivalent — not charged on subscription
================================================
 Tokens: 121,134,824        Burn: $7.03/hr
 Cache hit rate: 99.9%

        Thank you for vibe coding!
       *** NO REFUNDS ON TOKENS ***
================================================
            [ QR: claude --resume … ]
```

## Features

- **Multi-agent.** One tool reads the session logs of four different coding agents and normalizes them to a common model.
- **Correct cost accounting.** Every token category — input, output, cache-write, cache-read — is billed at its own rate. No blended averages. Tool-reported cost wins when present; otherwise a bundled per-model price table is used.
- **Three scopes.** A single session, a daily rollup across all agents, or on-demand.
- **Auto-printing.** Claude `Stop`/`PreCompact` hooks print a receipt when a session ends (and a "pre-compaction memorial" before context compaction); a `watch` daemon does the same for Codex/pi.
- **Detail.** Per-model token breakdown, tool-call bar chart, git productivity (files/lines/commits), beads tickets opened/closed, a tokens-over-time sparkline, cache-savings, burn rate, and a native QR code.
- **Subscription-aware.** Costs are API-list-equivalent; on a flat-rate subscription the TOTAL is labeled *"not charged."*

## Supported agents

| Agent | Source | Notes |
|---|---|---|
| **Claude Code** | `~/.claude/projects/**/*.jsonl` | per-turn `message.usage` |
| **Codex** | `~/.codex/sessions/**/rollout-*.jsonl` | cumulative `token_count` events; session-granular |
| **pi** | `~/.pi/agent/sessions/**/*.jsonl` | uses pi's own reported cost |
| **Antigravity (agy)** | `~/.gemini/antigravity-cli/conversations/*.db` | token usage extracted from SQLite `gen_metadata` protobuf blobs |

## Install

Requires Rust (1.80+) and, for printing, CUPS (`lp`) with a configured receipt-printer queue.

```bash
git clone <this-repo> tokenprinter && cd tokenprinter
cargo build --release
cp target/release/tokenprinter ~/.local/bin/   # anywhere on your PATH
tokenprinter doctor                            # checks lp/git/bd + lists discovered sessions
```

## Usage

```bash
# Per-session receipt (latest Claude session), preview to terminal:
tokenprinter print --agent claude --preview

# Actually print it:
tokenprinter print --agent claude

# A specific session, or the latest of an agent:
tokenprinter print --agent codex --session <id>
tokenprinter print --agent pi --last

# Daily rollup (today, local time), per agent:
tokenprinter daily --preview
tokenprinter daily --date 2026-06-13

# Inspect raw → normalized → priced token buckets (debugging / verification):
tokenprinter print --agent claude --audit

# Health check: printer queue, git, beads, session counts, price-drift:
tokenprinter doctor

# Auto-printing:
tokenprinter install-hooks         # Claude Stop + PreCompact (edits ~/.claude/settings.json)
tokenprinter install-watcher       # writes a launchd plist for the codex/pi watch daemon
tokenprinter watch --once --preview  # one manual watch pass, no printing
```

## Configuration

Optional `~/.config/tokenprinter/config.toml` (all fields have sensible defaults):

```toml
location   = "Somewhere on Earth"   # the line under "Agent"
timezone   = "America/Chicago"      # IANA tz; controls daily-receipt day boundaries
billing    = "subscription"         # "subscription" → TOTAL labeled "not charged"; "api" → real charge
queue_name = "Star_TSP654"          # CUPS print queue
transport  = "auto"                 # auto | cups | usb
idle_seconds = 90                   # watch daemon: print a session after this many idle seconds

# section toggles
show_tools = true
show_productivity = true
show_beads = true
show_sparkline = true
show_theatrics = true
show_qr = true
```

Custom per-model prices: drop a `prices.json` next to the config (`~/.config/tokenprinter/prices.json`) to override the bundled table.

## How pricing works

For each record: `cost = input·p_in + output·p_out + cache_write·p_cw + cache_read·p_cr`, each rate taken from a per-model table (USD per 1M tokens). Cache writes bill at 1.25× (5-min) / 2× (1-hr) input; cache reads at 0.1× input. If the agent's log already carries a computed cost (pi does), that wins.

**Adapters emit non-overlapping token buckets** so the formula is uniform. Codex is the tricky one — its raw `input_tokens` *includes* cached tokens, so the adapter normalizes `input = total − cached`, `cache_read = cached`. A property test validates the invariant across thousands of real records.

Bundled rates: authoritative Anthropic pricing for Claude models; OpenAI/Codex rates from OpenRouter. Unknown models render cost as `—` rather than guessing.

## Printing

Renders to **Star Line Mode** bytes (init, bold, alignment, native QR raster, auto-cut) and sends via CUPS (`lp -o raw`) with a direct-USB (`rusb`, Star vendor `0x0519`) fallback. The QR encodes the session's resume command (e.g. `claude --resume <id>`).

## Architecture

Single binary, subcommands. The `Adapter` trait is the only agent-specific surface; everything downstream is agent-agnostic:

```
adapter.discover()/parse()  →  enrich (git + beads)  →  price  →  assemble Receipt  →  render  →  transport
```

```
src/
  model.rs              shared types (Agent, UsageRecord, SessionData, Receipt)
  adapters/{claude,codex,pi,agy}.rs   per-agent log parsers
  enrich/{git,beads}.rs               git diffstat + beads ticket correlation
  pricing.rs + prices.json            per-category hybrid pricing
  assemble.rs                         records → Receipt (totals, cache metrics, burn, sparkline)
  render.rs                           48-col text + Star Line bytes + QR raster
  transport.rs                        CUPS → USB fallback
  watch.rs                            idle-detection daemon
  triggers/{hooks,launchd}.rs         install-hooks / install-watcher
  cli.rs                              subcommand wiring
```

## Development

```bash
cargo test          # unit + integration (incl. a property test over real session logs, skipped if absent)
cargo clippy
```

## Known limitations

- **Codex is session-granular** — its logs expose only cumulative session totals, so in `daily` rollups a Codex session that spans local midnight is attributed entirely to the day of its last event (Claude/pi split per-turn).
- **agy active sessions** hold WAL locks; the adapter skips locked DBs gracefully (no token data for the currently-running session).
- **The watch daemon** only prints sessions that go idle *during its uptime* — it seeds its seen-set on startup so it never reprints the historical backlog.
- **QR + cutter** — the receipt feeds past the print-head→cutter gap before cutting; if your printer's gap differs, adjust the feed in `render.rs`.

## License

MIT
