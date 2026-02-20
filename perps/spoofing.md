# Spoofing Vulnerability Analysis: ACE Perps

## 1. The invariant that matters

The async queue sort key is:

```rust
pub struct AsyncIxKey {
    pub slot: u64,      // primary:   earlier slot always processes first
    pub priority: u64,  // secondary: only differentiates within the same slot
    pub seq: u64,       // tertiary:  FIFO tiebreak within same (slot, priority)
}
```

The queue is a min-heap over this key, so processing order is lexicographic:

```
(slot=N,   priority=1, seq=_)   ← Cancel from slot N
(slot=N,   priority=3, seq=_)   ← Take   from slot N   → Cancel wins (same slot, lower pri)
(slot=N+1, priority=1, seq=_)   ← Cancel from slot N+1
(slot=N,   priority=3, seq=_)   ← Take   from slot N   → Take wins   (earlier slot)
```

**Cancel-priority only applies when both are queued in the same slot.**
A Cancel from slot N+1 cannot overtake a Take from slot N — the Take's lower slot number
wins unconditionally. This boundary defines which attack vectors are viable.

---

## 2. Priority tiers (recap)

| Priority | Instruction | Rationale |
|---|---|---|
| 0 | Liquidate | Protocol health first |
| 1 | Cancel | Protect market makers from toxic flow |
| 2 | PlaceOrder | Add liquidity |
| 3 | Take | Consume liquidity |

Crank eligibility check:

```rust
fn has_pending_async(&self, slot: u64) -> bool {
    val.key.slot + 1 <= slot  // items from previous slot or older
}
```

Items are cranked no earlier than the slot after they were submitted — ensuring at least one full
slot of priority ordering is established before execution.

---

## 3. Cross-block attack: timed PlaceOrder → shred-reactive Cancel

This is the most realistic production attack. It is cross-block (order genesis and cancel
execution span two different confirmed blocks) but uses intra-slot shred streaming for the
cancel reaction.

### 3.1 Setup and execution

```
╔══════════════════════════════════════════════════════════════════════════════════════╗
║  SLOT N  (ending window, ~350–400 ms into the slot)                                  ║
╠══════════════════════════════════════════════════════════════════════════════════════╣
║  t=350ms  MM sends PlaceOrder TX (large ask @ 150, size=100)                         ║
║           → TX lands in block N near the end of the slot                             ║
║           → Queue entry: PlaceOrder(slot=N, pri=2, seq=X)                            ║
║           → Margin debited: 10% of notional = 1500 units                             ║
║                                                                                      ║
║  Taker observes nothing yet — the order is in the queue, not the book                ║
╚══════════════════════════════════════════════════════════════════════════════════════╝
                        │
                        │  ~400 ms: block N finalizes
                        ▼
╔══════════════════════════════════════════════════════════════════════════════════════╗
║  SLOT N+1  (~400 ms window)                                                          ║
╠══════════════════════════════════════════════════════════════════════════════════════╣
║  t=+0ms   Slot N+1 begins                                                            ║
║                                                                                      ║
║  t=+30ms  Crank TX submitted & included                                              ║
║           → PlaceOrder(N,2,X) is eligible: N+1 ≤ N+1 ✓                               ║
║           → settlement: margin debited, ask placed at price=150, seq=X on book       ║
║           → Shred broadcast: crank TX confirmed                                      ║
║                                ┌──────────────────────────────────────────────────┐  ║
║  t=+40ms  Taker bot sees shred │ "crank processed, ask @ 150 now live on book"    │  ║
║           (or polls account    └──────────────────────────────────────────────────┘  ║
║            data via shred-acc  )                                                     ║
║                                                                                      ║
║  t=+50ms  Taker submits Take TX (buy 100 @ limit 150)                                ║
║           → Take queued: Take(slot=N+1, pri=3, seq=Y)                                ║
║           → Shred broadcast immediately ◄─────────────────────────────────────────┐  ║
║                                                                                   │  ║
║  t=+60ms  MM's ShredStream listener fires on Taker's Take TX                      │  ║
║           → MM constructs Cancel TX (price=150, side=ask, order_seq=X)           ─┘  ║
║           → Cancel TX sent directly to leader (co-located, <5ms RTT)                 ║
║                                                                                      ║
║  t=+65ms  Cancel TX included in block N+1                                            ║
║           → Cancel queued: Cancel(slot=N+1, pri=1, seq=Z)                            ║
║                                                                                      ║
║  Block N+1 finalizes. Queue now contains:                                            ║
║    ┌─ Take  (slot=N+1, pri=3, seq=Y)                                                 ║
║    └─ Cancel(slot=N+1, pri=1, seq=Z)                                                 ║
╚══════════════════════════════════════════════════════════════════════════════════════╝
                        │
                        │  ~400 ms: block N+1 finalizes
                        ▼
╔══════════════════════════════════════════════════════════════════════════════════════╗
║  SLOT N+2  — Crank processes N+1 items (N+1+1 = N+2 ≤ current_slot ✓)               ║
╠══════════════════════════════════════════════════════════════════════════════════════╣
║                                                                                      ║
║  Queue sorted by (slot, priority, seq):                                              ║
║    1st: Cancel(N+1, pri=1, seq=Z)   ← LOWER priority value wins                      ║
║    2nd: Take  (N+1, pri=3, seq=Y)                                                    ║
║                                                                                      ║
║  STEP 1 — Cancel executes:                                                           ║
║    cancel_ask(OrderId { price_key: 150, seq: X })                                    ║
║    → order found and removed from book ✓                                             ║
║    → margin returned to MM: +1500 units ✓                                            ║
║    → MM net cost: only transaction fees                                              ║
║                                                                                      ║
║  STEP 2 — Take executes:                                                             ║
║    execute_take(taker, limit=150, size=100, side=buy)                                ║
║    → best_ask() returns None (book is empty)                                         ║
║    → filled = 0                                                                      ║
║    → taker's margin is debited for nothing ✗                                         ║
║    → taker pays TX fees, receives 0 fill ✗                                           ║
║                                                                                      ║
╚══════════════════════════════════════════════════════════════════════════════════════╝

OUTCOME:
  MM:    Placed order → never filled → margin returned → net cost = 2× TX fees only
  Taker: Submitted real take → paid fees → received 0 fill → harmed
```

### 3.2 Why timing the PlaceOrder at the END of slot N matters

Placing near the end of slot N gives the MM maximum strategic advantage in slot N+1:

```
Slot N+1 timeline (400 ms window):

  ┌───────────────────────────────────────────────────────────────────────┐
  │  0ms       50ms      100ms     200ms     300ms     400ms              │
  │  │          │         │         │         │         │                  │
  │  ├──────────┤         │         │         │         │                  │
  │  │ Crank    │         │         │         │         │                  │
  │  │ PlaceOrd │         │         │         │         │                  │
  │  │ → LIVE   │         │         │         │         │                  │
  │  │          │         │         │         │         │                  │
  │  │   Taker reacts to live order (shred/poll)        │                  │
  │  │          │←───────────────────────────────────── │ taker window     │
  │  │          │ Taker submits Take (any point in slot) │                  │
  │  │          │                                        │                  │
  │  │          │ MM sees Taker's Take shred, ~10ms RTT  │                  │
  │  │          │ MM submits Cancel ─────────────────────► both in N+1      │
  │  │          │                                                            │
  │  │          │ Available reaction window for MM: up to ~340 ms           │
  └───────────────────────────────────────────────────────────────────────┘

  If MM had placed order at START of slot N-1 instead:
  → Order is live for 2 full slots before the attack window opens
  → More exposure, more risk of being filled by an early taker
  → End-of-slot placement minimizes live exposure to a single slot gap
```

By placing at slot N's end, the order is live for the minimum possible time (~0 ms in slot N,
until crank in slot N+1) before the MM has full cancel protection via shred streaming.

---

## 4. Selective layer cancellation: fill at worse-than-expected prices

The 0-fill outcome from §3 is the visible, detectable form of harm. A more insidious variant
produces **genuine fills at prices systematically worse than what the book advertised**. The
taker is filled — so there is no obvious failure signal — but their entry price is degraded by
the MM's selective removal of the most attractive layers.

This is strictly more harmful than 0-fill because the taker cannot detect the attack, cannot
attribute it to manipulation (it is indistinguishable from ordinary market impact), and has no
on-chain evidence to point to.

### 4.1 How `execute_take` sweeps multiple levels

From `settlement.rs`, `execute_take` is a multi-level sweep loop:

```rust
while size > 0 {
    let Some((best_key, best_order)) = state.orderbook.best_ask() else { break; };
    if best_order.price > limit_price { break; }
    let fill_price = best_order.price;
    let fill_size  = size.min(best_order.size);
    // ... remove or partially reduce level, settle fill ...
    size -= fill_size;
    total_filled += fill_size;
}
```

Each fill calls `settle_fill`, which debits the taker's margin at `notional / 10` per level
and establishes or adds to their position at the fill price. If the MM removes the best level
before the take executes, the sweep starts from the next-best level and all subsequent fills
are at worse prices.

### 4.2 Concrete example: single-level bait removal

Setup — MM publishes a three-level ask ladder. Taker sees:

```
  Ask ladder (visible on-chain via orderbook_viewer):
  ┌─────────────────────────────┐
  │  price=150  size=10  seq=10 │  ← bait: best level, attracts taker
  │  price=152  size=10  seq=11 │  ← real
  │  price=154  size=10  seq=12 │  ← real
  └─────────────────────────────┘
```

Taker submits: `Take(limit=157, size=25, side=buy)`

**What the taker expects** (sweeping all levels within limit=157):

```
  Fill 1:  10 units @ 150   cost = 1500   margin_debited = 150
  Fill 2:  10 units @ 152   cost = 1520   margin_debited = 152
  Fill 3:   5 units @ 154   cost =  770   margin_debited =  77
             ─────────────────────────────────────────────────
  Total:   25 units         cost = 3790   margin_debited = 379
  Avg entry price: 3790 / 25 = 151.60
```

Attack: MM sees Take shred, **cancels only the 150 ask**, keeps 152 and 154 on the book.

```
  Slot N+1 queue after MM reacts:
    Cancel(N+1, pri=1)  ← targets seq=10 @ 150
    Take  (N+1, pri=3)  ← limit=157, size=25
```

**What the taker actually gets** after crank (Cancel fires first):

```
  Cancel: removes ask@150 → margin returned to MM
  Take sweeps remaining book:
    Fill 1:  10 units @ 152   cost = 1520   margin_debited = 152
    Fill 2:  10 units @ 154   cost = 1540   margin_debited = 154
    Fill 3:   5 units @ 156   cost =  780   margin_debited =  78  ← if a 156 level exists
             ─────────────────────────────────────────────────────
    Total:   25 units         cost = 3840   margin_debited = 384
    Avg entry price: 3840 / 25 = 153.60
```

**Extraction:**

```
  Advertised average entry:  151.60
  Actual average entry:      153.60
  Price slippage per unit:   +2.00   ← extracted from taker on every unit
  Total extracted:           2.00 × 25 = 50 units
  Excess margin debited:     384 - 379 = 5 units (locked in taker's position)
```

The taker's position opens 2.0 units worse than the book promised. For a long, oracle must
now move 2.0 higher just to reach the break-even the taker thought they already had.

### 4.3 Deep-ladder amplification

The attack scales directly with ladder depth. MM quotes a 10-level ask book:

```
  Visible book:  150, 151, 152, 153, 154, 155, 156, 157, 158, 159
  Real intent:   only 155–159 are genuine; 150–154 are bait
```

Taker submits `Take(limit=160, size=50)`.

```
  Expected: 10×150+10×151+10×152+10×153+10×154 = avg 152.0
  After MM cancels levels 150–154:
  Actual:   10×155+10×156+10×157+10×158+10×159 = avg 157.0

  Price delta:       +5.0 per unit
  Total extracted:   5.0 × 50 = 250 units
```

The taker swept the entire visible book in good faith and was filled entirely within their
limit price, yet received an average entry 5.0 units worse than advertised. From their
perspective this is indistinguishable from the book having been thinner than shown due to
concurrent other takers.

### 4.4 Timeline: selective layer cancellation

```
SLOT N (end)
  MM publishes ask ladder: 150, 152, 154 (all queued, cranked next slot)
  ─────────────────────────────────────────────────────────────────────
SLOT N+1
  t=+30ms   Crank: PlaceOrder(N) processed
            → asks @ 150/152/154 all live on book
            → Shred: "book updated, 3 levels live"

  t=+40ms   Taker sees full ladder: 10@150, 10@152, 10@154
            Taker submits Take(limit=157, size=25)
            → Take queued(N+1, pri=3)
            → Shred: "Take TX from taker"

  t=+50ms   MM sees Take shred
            Decision: cancel only the BEST level (150)
            → Cancel TX targets seq=10 @ 150
            → Cancel queued(N+1, pri=1)

  Block N+1 finalizes:
    Queue: Cancel(N+1,1) | Take(N+1,3)
  ─────────────────────────────────────────────────────────────────────
SLOT N+2 — Crank
  Step 1: Cancel(N+1,1)  → ask@150 removed. Margin returned to MM.
  Step 2: Take(N+1,3)    → execute_take sweeps:
           best_ask() = 152 → fill 10@152 ✓ (within limit=157)
           best_ask() = 154 → fill 10@154 ✓
           best_ask() = ??? → fill 5@next or loop exits if none
           filled ≠ 25 and avg_entry = 153+ instead of 151.6

  ┌────────────────────────────────────────────────────────────────┐
  │  TAKER SEES:  "Filled 25 @ avg 153.60"                        │
  │  TAKER THINKS:"Normal slippage, market was thinner than shown" │
  │  REALITY:     "Best level was phantom bait, cancelled on cue"  │
  └────────────────────────────────────────────────────────────────┘
```

### 4.5 Why this is harder to defend against than 0-fill

| | 0-fill outcome | Worse-price-fill outcome |
|---|---|---|
| Taker knows they were harmed | Yes — fill size = 0 is obvious | No — fill happened, price looks like slippage |
| On-chain attribution | Cancel + Take both visible | Cancel of a specific level; looks routine |
| Regulatory footprint | Obvious failed execution | Indistinguishable from normal market impact |
| MM risk of detection | Moderate | Low |
| Taker's recourse | Can retry immediately | Retry compounds the harm; book still shows bait |
| MM profit per event | 0 (no fill, margin returned) | Price delta × fill size (stolen spread) |

The MM can tune the attack continuously: in benign market conditions, let orders fill normally
to maintain the appearance of a legitimate quoting strategy. In adversarial conditions (large
taker, unfavorable delta), selectively cancel the best levels. The on-chain record shows a
typical cancel followed by a normal fill — identical to a legitimate MM updating their quotes.

---

## 5. Pure intra-block attack (pre-existing live order)

This is the simpler variant where orders are already live on the book from prior cranks.
No cross-slot timing is required.

```
TIME (ms)    SLOT N (block being built)               SHRED STREAM     MM BOT
────────────────────────────────────────────────────────────────────────────────
t=0          Block N opens
             │ MM's ask LIVE on book from previous crank
             │
t=60         │ Taker TX received and processed
             │ Take queued(N, pri=3, seq=Y)
             │                                         ─shred──────►  MM sees Take
t=70         │                                                         constructs Cancel
t=80         │ Cancel TX lands                         ◄──Cancel──────
             │ Cancel queued(N, pri=1, seq=Z)
             │
t=400        Block N finalizes

SLOT N+1 — Crank:
  Cancel(N,1,Z) → order removed, margin returned ✓
  Take(N,3,Y)   → nothing to fill ✗
```

This variant requires no special timing of PlaceOrder but DOES require an already-live order
(from a prior slot's crank). The cross-block attack (§3) extends this by making the order appear
live exactly when the MM wants it to, then immediately protecting it.

The selective layer variant (§4) applies here equally: the MM may cancel only the best level
and allow the take to fill against the remaining levels at worse prices.

---

## 6. Cross-block phantom liquidity (no shreds, RPC-only)

A simpler attack available to any MM without shred access.

The MM places large layered orders and cancels them before any taker can engage. Since the RPC
polling latency is ~400 ms (one block), the MM can only observe the queue state after a block
finalizes. This means:

- If a Take lands in slot N, the MM can only see it after block N closes
- A Cancel submitted in slot N+1 has key `(N+1, pri=1)` — which loses to Take `(N, pri=3)` because N < N+1

**Cross-block reactive cancellation therefore fails.** Slot ordering unconditionally protects the
taker against an RPC-speed adversary.

What a RPC-only MM CAN do is cycle continuously:

```
Block N:   PlaceOrder → orders queued
Block N+1: Crank → orders LIVE (book looks deep to observers)
Block N+2: (watch) no taker yet → Cancel both sides
Block N+3: Crank → orders removed, margin refunded
Block N+4: PlaceOrder again at updated quotes
...repeat indefinitely...
```

The MM cancels preemptively (not reactively) before any taker can act. Cost is 2 TX fees per
cycle (~5000 lamports × 2 ≈ $0.001 at current prices). The book always appears liquid to
observers running `orderbook_viewer` but no genuine fill intent exists.

The cross-block attack from §3 is strictly better: it's reactive rather than speculative, so
the MM can leave orders up indefinitely and only cancel the moment a real taker engages.

---

## 7. Cross-block slot-ordering defeat (why naive cross-slot cancel fails)

```
SLOT N     │ Take queued  (slot=N,   pri=3, seq=Y)
           │ Block N finalizes, MM sees Take via RPC
SLOT N+1   │ MM submits Cancel  (slot=N+1, pri=1, seq=Z)

Queue at crank time (slot N+2):
  Take  (slot=N,   pri=3) → sort key: (N,   3, Y)
  Cancel(slot=N+1, pri=1) → sort key: (N+1, 1, Z)

  N < N+1 → Take processes FIRST regardless of priority values.
  Taker is protected. Cancel removes an already-filled order (no-op).
```

This is why shred streaming is necessary for the attack. An RPC-latency adversary always sees
the Take one slot too late to queue a same-slot Cancel.

---

## 8. Full lifecycle: shred-based spoofing with price extraction

Two variants of the same loop, depending on whether the MM wants to avoid all fills or
extract value through degraded execution:

```
┌────────────────────────────────────────────────────────────────────────────────────────────┐
│ SLOT        MM ACTION                  TAKER VIEW              ON-CHAIN TRUTH              │
├────────────────────────────────────────────────────────────────────────────────────────────┤
│                         ── VARIANT A: full cancel (0-fill) ──                              │
│ N (end)     PlaceOrder(ask@150×100)    —                       Order in queue              │
│ N+1 (start) [crank fires]              "100 @ 150 live!"       PlaceOrder cranked          │
│ N+1         TAKER submits Take×100     "My order queued"       Take(N+1,3) queued          │
│             MM cancels all@150         —                       Cancel(N+1,1) queued        │
│ N+2 (crank) —                          "Filled 0"              Cancel→Take fills nothing   │
│             PlaceOrder(ask@150×100)    "Book liquid again"      Order in queue again        │
├────────────────────────────────────────────────────────────────────────────────────────────┤
│                   ── VARIANT B: selective cancel (fill at worse price) ──                  │
│ N (end)     PlaceOrder(150×10,         —                       3 levels in queue           │
│             152×10, 154×10)                                                                │
│ N+1 (start) [crank fires]              "Ladder: 150/152/154"   All 3 levels live           │
│ N+1         TAKER submits Take×25      "Sweeping ladder"       Take(N+1,3) queued          │
│             MM cancels ONLY@150        —                       Cancel(N+1,1) queued        │
│ N+2 (crank) —                          "Filled 20 @ avg 153"   Cancel@150 → Take sweeps   │
│             —                          "Expected avg 151.6"    152+154; taker overpays     │
│             —                          "Must be slippage…"     MM pockets 2.0/unit delta  │
│             PlaceOrder(150×10,         "Ladder restored"        New bait layer queued      │
│             152×10, 154×10)                                                                │
└────────────────────────────────────────────────────────────────────────────────────────────┘

OBSERVED BY MARKET:  Persistent deep book, tight spread, normal-looking executions
VARIANT A REALITY:   Every genuine taker engagement results in 0 fill
VARIANT B REALITY:   Takers fill at manufactured worse prices; delta extracted silently
```

---

## 9. Asymmetry table

| Dimension | MM (with shreds) | Taker |
|---|---|---|
| Order visibility | Sees taker intent via shreds ~10ms after TX broadcast | Sees book state after block finalizes (~400ms lag) |
| Cancel window | Entire remaining slot duration (~340ms) | N/A |
| Cost of failed attempt | 2× TX fees (~$0.001) | 1× TX fee + margin locked until crank |
| Information at time of decision | Knows taker is engaging AND knows taker's limit price | Does not know MM will cancel or selectively cancel |
| Outcome (full cancel) | Cancel wins via priority; margin returned; 0 fills | 0 fill, fees paid, no recourse |
| Outcome (selective cancel) | Worst levels survive; taker pays delta × size per fill | Filled within limit price but at worse avg entry |
| Detectability | Low: cancel + fill look like routine quote updates | None: fill-at-worse-price is indistinguishable from slippage |
| Profit per event | 0 (full cancel) or price_delta × fill_size (selective) | Entry price degraded by delta; locked in position |

---

## 10. Root cause

The cancel-priority guarantee (Cancel=1, Take=3) was designed to protect LPs from toxic
order flow in a *symmetric* information environment: if a taker and a canceller both submit
blindly in the same slot, the cancel wins, protecting the LP.

Shred streaming breaks the symmetry in two ways:

**First**, the MM gains guaranteed advance notice of any Take entering the current block,
converting a defensive mechanism into an offensive one. The taker's intent is revealed before
the block closes; the MM's cancel is invisible to the taker until the next block.

**Second**, and more importantly, the MM can exploit their knowledge of the taker's limit
price (visible in the Take TX) to make a targeted decision: cancel only the layers that would
have given the taker a good fill, leave the layers that give a bad fill. The take executes
within the limit price — so from the taker's perspective the order worked — but the entry
price has been silently degraded. The economic harm is hidden inside what looks like ordinary
market impact.

The two-slot minimum delay (`queued_slot + 1 <= crank_slot`) governs **when** items are
cranked, not **who can observe what** before submitting. The observation window at the shred
level (~10–30ms) is orders of magnitude smaller than the slot duration (400ms), so a
shred-subscribed MM can almost always fit a Cancel into the same slot as any observed Take.

The protocol cannot distinguish a legitimate defensive cancel from a spoofing cancel — or a
legitimate quote update from a selective-layer bait-and-switch — because they produce
identical on-chain state transitions.

---

## 11. Potential mitigations (sketch, not implemented)

| Mitigation | Addresses | Mechanism | Tradeoff |
|---|---|---|---|
| **Commit-reveal for takes** | Full cancel + selective cancel | Taker commits to a hash in slot N, reveals params in slot N+1; MM cannot know target (or limit price) until reveal slot | Adds 1-slot latency to all takes; requires two TXs |
| **Cancel cooldown** | RPC-only phantom cycling | Cancelled orders cannot be re-placed for K slots | Limits cycling frequency; skilled MM works around it |
| **Cancel penalty** | All cancel variants | Cancelled orders forfeit a % of reserved margin | Raises cost of spoofing; calibrating % is difficult |
| **Minimum live duration** | Cross-block end-of-slot timing (§3) | PlaceOrder cannot be cancelled until it has been live for ≥ M slots | Forces genuine fill exposure window; directly attacks §3.2 timing trick |
| **Taker priority for aged orders** | Selective cancel + full cancel | If order was on book for ≥ M slots, a same-slot Cancel cannot overtake a Take | Protects takers after an order ages in; does not help for freshly-cranked orders |
| **Limit price concealment** | Selective layer cancel (§4) specifically | Taker submits encrypted limit price, revealed only at crank time | Prevents MM from knowing which layers to keep; complex cryptographic addition |
| **Partial-fill protection** | Selective cancel | If a Take fills < X% of requested size, reserve a portion of the MM's margin as penalty | Penalises artificial slippage; hard to distinguish from genuine thin books |
| **Auction-based matching** | All variants | Replace priority queue with sealed-bid matching across a time window | Fundamentally different design; eliminates shred-reactive advantage entirely |
