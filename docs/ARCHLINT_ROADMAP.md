# archlint → Blocking CI: Roadmap

**Goal:** archlint-rs блокирует merge в deskd при архитектурных нарушениях.

**Status:** archlint в CI на каждый PR (informational). Не блокирует.

---

## Current State (2026-03-31)

### archlint-rs scan results on deskd

```
Health:           50/100
Components:       36
Links:            86
Violations:       10
  fan_out:        1  (main = 20, limit 10)
  dip:            9  (modules with structs but no traits)
  layer:          0  (see note below)
  forbidden_deps: 0  (see note below)
  cycles:         0
  taboo:          0
```

### What archlint-rs can do now

| Rule | Status | Notes |
|------|--------|-------|
| fan_out / fan_in | ✅ Works | Detects coupling, configurable thresholds |
| dip | ✅ Works | Finds modules without trait abstractions |
| cycles | ✅ Works | No cycles detected |
| layer violations | ✅ Implemented (archlint#93) | Needs directory-based structure to match |
| forbidden_deps | ✅ Implemented (archlint#95) | Needs directory-based structure to match |
| module groups | ✅ Implemented (archlint#96) | Auto-detects domain/, ports/, infra/ |
| perf (CC, nesting) | ⚠️ High false positives | archlint#94 still open |
| config schema | ⚠️ | archlint#92 still open |

### Why 0 layer violations

Layer rules match by **directory path** (`src/domain/`, `src/infra/`, etc.). Most deskd modules are still in flat `src/` — they don't fall into any layer directory. Layer enforcement will activate as modules move to target directories during refactoring.

---

## Blocking Criteria

All must be true to make archlint blocking:

| Criterion | Current | Target | How |
|-----------|---------|--------|-----|
| Health score | 50 | ≥70 | Refactor deskd (Phase 1-2) |
| Fan-out | 20 (main) | ≤10 | Break up main.rs (#143) |
| DIP violations (service modules) | 9 | 0 | Add traits (#138, #140) |
| DIP false positives (data types) | ~4 | 0 | archlint: exclude pure data types, OR add DIP exceptions |
| Layer violations | not checked | 0 | Move modules to domain/ports/infra/app dirs |
| Perf false positives | ~80% | <10% | archlint#94 |
| All deskd arch issues closed | 2/8 | 8/8 | Phase 1-3 |

---

## Track 1: archlint fixes (upstream maintainer)

| Issue | What | Status | Blocker for CI? |
|-------|------|--------|-----------------|
| #92 | Config schema warning | Open | No |
| #93 | Layer violation detection | ✅ Closed | — |
| #94 | Perf false positives | **Open** | **Yes** — 875 noise issues |
| #95 | Forbidden deps | ✅ Closed | — |
| #96 | Module groups | ✅ Closed | — |
| #97 | Layer enforcement not triggering | **Open** | **Yes** — 0 violations on known-bad code |
| NEW | DIP exceptions for data types | Not filed | **Yes** — domain::message is pure data, no trait needed |

**Minimum from upstream maintainer:** #94 (perf) + #97 (layer enforcement) + DIP data type exceptions.

## Track 2: deskd refactoring (Kira)

| Issue | What | Status | Phase |
|-------|------|--------|-------|
| #138 | AgentRunner trait + domain extraction | ✅ PR #146 merged (partial) | 1 |
| #139 | MessageBus trait | Open | 1 |
| #140 | Store traits | Open | 1 |
| #141 | Break up worker::run() | Open | 2 |
| #142 | Consolidate inboxes | Open | 2 |
| #143 | Break up main.rs | Open | 2 |
| #144 | Fix config module | Open | 2 |
| #145 | Tests for agent/worker/mcp | Open | 3 |

### Phase 1: Traits (enables layer enforcement)
- Move remaining types to `domain/` (context, statemachine, config types, inbox)
- Implement Executor trait, wire ClaudeExecutor + AcpExecutor
- Implement MessageBus trait, extract UnixBus
- Implement Store traits, extract FileStores
- **After this:** archlint layer rules activate (modules are in directories)

### Phase 2: Decomposition (drives health score up)
- Break up main.rs → cli/ + serve.rs (fan-out 20 → ≤7)
- Break up worker::run() → Worker struct with injected deps
- Consolidate inbox systems
- Fix config circular deps
- **After this:** fan-out ≤10, fan-in ≤8, DIP violations on service modules → 0

### Phase 3: Testing
- Tests for agent, worker, mcp using InMemoryBus + MockExecutor
- Integration tests with in-memory infrastructure
- **After this:** 0 untested critical modules

### Phase 4: Make blocking
- Tune thresholds based on actual post-refactoring numbers
- Set archlint quality gate: taboo violations = block merge
- Set archlint quality gate: health score regression = block merge
- Remove informational mode, enable enforcement

---

## Sequence

```
         upstream maintainer                    Kira                     We
           │                       │                        │
     #94 perf fix            #139 MessageBus trait          │
     #97 layer fix           #140 Store traits              │
     DIP exceptions          Move to domain/infra/app dirs  │
           │                       │                        │
           ▼                       ▼                        │
     ── archlint v2 ──      ── Phase 1 done ──              │
           │                       │                        │
           └───────────┬───────────┘                        │
                       ▼                                    │
              Verify: layer violations detected             │
              Verify: DIP clean on service modules          │
              Verify: perf <10% false positives             │
                       │                                    │
                       ▼                                    │
              ── Kira: Phase 2 ──                           │
              Break up main, worker, config                 │
                       │                                    │
                       ▼                                    │
              Verify: health ≥70                            │
              Verify: fan-out ≤10                           │
              Tune thresholds                               │
                       │                                    │
                       ▼                                    │
              ── Phase 4: Make blocking ──          ◄── enable
```

---

## .archlint.yaml target (after refactoring)

```yaml
rules:
  fan_out:
    threshold: 7
    level: taboo         # block merge
  fan_in:
    threshold: 8
    level: taboo
  cycles:
    enabled: true
    level: taboo
  dip:
    enabled: true
    level: taboo
    exclude:
      - "src/domain/*"   # pure data types don't need traits
      - "src/cli/*"      # CLI structs don't need traits
  isp:
    threshold: 5
    level: personal      # warn only

layers:
  - name: domain
    paths: ["src/domain"]
  - name: ports
    paths: ["src/ports"]
  - name: infra
    paths: ["src/infra", "src/adapters"]
  - name: app
    paths: ["src/app"]
  - name: cli
    paths: ["src/cli"]
  - name: main
    paths: ["src/main", "src/lib"]

allowed_dependencies:
  domain: []
  ports: [domain]
  infra: [domain, ports]
  app: [domain, ports]     # NOT infra — app works through traits
  cli: [domain, app]
  main: [domain, ports, infra, app, cli]

forbidden_deps:
  - from: domain
    to: [infra, app, cli]
    reason: "domain must be pure"
  - from: app
    to: [infra]
    reason: "app uses ports, not concrete infra"
```
