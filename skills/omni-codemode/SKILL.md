---
name: omni-codemode
description: Write deterministic, sandboxed OmniMCP Code Mode scripts using codemode.<service>.<tool>(...), 0-turn BM25 TypeScript declarations, Promise.all parallel waves, HITL approval/resume, DST chaos simulation, and LIFO Saga rollbacks.
---

# OmniMCP Code Mode Skill (`v6.8.5`)

Use Code Mode whenever an agent task requires **2+ tool calls**, data filtering/joining, conditional branching, or transactional rollbacks. Code Mode executes inside OmniMCP's pure-Rust, sub-millisecond, zero-egress (`globalOutbound: null`) sandbox.

## 1. Golden Rules

1. **Always call `codemode.<service>.<tool>(args)`** — NEVER `omni.<service>.<tool>(args)`. The Code Mode VM binds services exclusively under the `codemode` namespace.
2. **Fetch types first**: Call `get_codemode_types` with `intent: "<task description>"` to receive 0-turn BM25-ranked TypeScript `.d.ts` declarations for only the relevant services.
3. **Wrap in an async arrow function**:
   ```ts
   async () => {
     const loc = await codemode.driver_dispatch.get_driver_location({ driver_id: "drv_1" });
     return { driver: loc.driver_id, status: loc.status };
   }
   ```
4. **Filter inside the sandbox**: Use array `.filter(...)`, `.map(...)`, and `|x| expr` closures inside Code Mode so only the final compact summary returns to the LLM.
5. **Never attempt raw network/filesystem I/O**: `fetch`, `require`, and `process` are disabled by design (`globalOutbound: null`). All external side effects must go through `codemode.<service>.<tool>(...)`.

## 2. Parallel Waves & Columnar Pushdown

Use `Promise.all` (or independent awaits eligible for SSA wave scheduling) to fan out read calls concurrently:

```ts
async () => {
  const [driver, balance] = await Promise.all([
    codemode.driver_dispatch.get_driver_location({ driver_id: "drv_42" }),
    codemode.accounting.get_balance({ account_id: "acc_42" }),
  ]);
  return { driver_status: driver.status, balance: balance.amount };
}
```

## 3. HITL Approval & Separation of Duties

For mutating workflows (refunds, deletes, deployments), pass `require_mutation_approval: true`:

- Execution pauses at the mutating call and returns:
  ```json
  { "status": "APPROVAL_REQUIRED", "execution_id": "exec_..." }
  ```
- A **distinct Approver principal** (Separation of Duties: initiator cannot self-approve) resumes execution via `resume_codemode` or the Governance Studio Approval Inbox.

## 4. Deterministic Simulation Testing (DST) & LIFO Sagas

Before running a complex multi-mutation workflow in production:

1. Call `simulate_codemode` with `{ "code": "...", "seed": 42, "runs": 5, "fault_rate": 0.2 }` to verify deterministic behavior under injected faults.
2. When a downstream step fails after earlier mutations succeeded, OmniMCP automatically executes registered **LIFO Saga compensations** in reverse order to restore consistency.

See `references/codemode-patterns.ts` for copy-ready patterns.
