# Liquidity Dynamics: FIFO vs. Cancel-Priority Scheduling

## 1. The core difference

Three scheduling variants are compared across the same slot:

```
Mode A — FIFO:                   all instructions ordered by arrival seq
Mode B — Full ACE priority:      Cancel(1) < PlaceOrder(2) < Take(3)
Mode C — Cancel-priority only:   Cancel(1) < { PlaceOrder(2) = Take(2) } by seq
```

In FIFO, a market maker's cancel and replace are adjacent in the processing queue. Each cancel
is immediately followed by its replacement — the book briefly loses one order and gets it back.
Depth never falls far from its steady-state.

In full ACE (Mode B) the scheduler enforces hard phase separation: every Cancel fires before any
PlaceOrder, and every PlaceOrder fires before any Take. Multiple MMs requoting in the same slot
drain the book entirely before any liquidity is restored.

Mode C is a middle variant: Cancels still get priority=1 and fire first, but PlaceOrders and
Takes share priority=2 and are sorted by seq within that tier. A taker who submitted before some
MMs placed their replacements executes mid-restoration — against a partially rebuilt book.

---

## 2. Scenario

```
Oracle:  100 → 103  (moves mid-slot N)

Market makers (3 orders live from the previous crank):

  MM-A   bid@97  qty=20    ask@103  qty=20
  MM-B   bid@96  qty=15    ask@104  qty=15
  MM-C   bid@95  qty=10    ask@105  qty=10
         ────────────────  ────────────────
         bid total = 45    ask total = 45

All MMs detect the oracle move and requote at oracle ± 3 (shift = +3):

  MM-A   cancel bid@97,  place bid@100     cancel ask@103,  place ask@106
  MM-B   cancel bid@96,  place bid@99      cancel ask@104,  place ask@107
  MM-C   cancel bid@95,  place bid@98      cancel ask@105,  place ask@108

Taker T: buy Take(limit=110, size=40) submitted mid-slot
```

Slot N queue (in submission / arrival order):

```
  seq= 1   MM-A  Cancel bid@97
  seq= 2   MM-A  Place  bid@100
  seq= 3   MM-A  Cancel ask@103
  seq= 4   MM-A  Place  ask@106
  seq= 5   T     Take   buy  limit=110  size=40    ← lands between MM-A and MM-B
  seq= 6   MM-B  Cancel bid@96
  seq= 7   MM-B  Place  bid@99
  seq= 8   MM-B  Cancel ask@104
  seq= 9   MM-B  Place  ask@107
  seq=10   MM-C  Cancel bid@95
  seq=11   MM-C  Place  bid@98
  seq=12   MM-C  Cancel ask@105
  seq=13   MM-C  Place  ask@108
```

---

## 3. Mode A — FIFO scheduling

Instructions processed in arrival order (seq 1 → 13). Cancel and replace always appear as
consecutive pairs; the book loses one order and gets it back in the very next step.

### 3.1 Initial orderbook

```
  ASK  105 │ ████████                  10  (MM-C)
       104 │ ██████████████            15  (MM-B)
       103 │ ████████████████████      20  (MM-A)
           │ ── oracle 100, spread ±3─ ──
        97 │ ████████████████████      20  (MM-A)
        96 │ ██████████████            15  (MM-B)
  BID  95  │ ████████                  10  (MM-C)

  Bid total = 45   Ask total = 45   Grand total = 90
```

### 3.2 Depth trace (each █ = 5 units)

```
Step  Instruction         Bid   Ask  Total  Depth bar
──────────────────────────────────────────────────────────────────────────
  0   [initial]            45    45     90  ██████████████████
  1   Cancel bid@97        25    45     70  ██████████████
  2   Place  bid@100       45    45     90  ██████████████████
  3   Cancel ask@103       45    25     70  ██████████████
  4   Place  ask@106       45    45     90  ██████████████████
  5 ► TAKE  buy×40 ──────────────────────────────────────────────────────
      post-Take            45     5     50  ██████████          fills: 15@104, 10@105, 15@106
  6   Cancel bid@96        30     5     35  ███████
  7   Place  bid@99        45     5     50  ██████████
  8   Cancel ask@104     (no-op: consumed by Take at step 5)
  9   Place  ask@107       45    20     65  █████████████
 10   Cancel bid@95        35    20     55  ███████████
 11   Place  bid@98        45    20     65  █████████████
 12   Cancel ask@105     (no-op: consumed by Take at step 5)
 13   Place  ask@108       45    30     75  ███████████████
──────────────────────────────────────────────────────────────────────────
Peak = 90    Minimum = 35 (step 6)    Book empties: NO
```

Steps 8 and 12 are no-ops because those ask levels were swept by the Take at step 5.

### 3.3 Book at moment of Take execution (step 5)

The Take fires after MM-A has fully requoted but before MM-B and MM-C have done anything.

```
  ASK  106 │ ████████████████████      20  (MM-A NEW)
       105 │ ████████                  10  (MM-C OLD — not yet cancelled)
       104 │ ██████████████            15  (MM-B OLD — not yet cancelled)
           │ ─────── oracle 103 ───────
       100 │ ████████████████████      20  (MM-A NEW)
        96 │ ██████████████            15  (MM-B OLD — not yet touched)
  BID  95  │ ████████                  10  (MM-C OLD — not yet touched)

  Mixed book: MM-A fully updated; MM-B and MM-C still at pre-oracle-move prices.
```

### 3.4 Taker fill

```
  Fill   Price    Qty    Note
  ─────────────────────────────────────────────────────────────
    1      104     15    MM-B OLD ask — not yet cancelled
    2      105     10    MM-C OLD ask — not yet cancelled
    3      106     15    MM-A NEW ask
  ─────────────────────────────────────────────────────────────
  Total     40 units     Avg fill = (15×104 + 10×105 + 15×106) / 40
                                  = (1560 + 1050 + 1590) / 40 = 105.00
```

---

## 4. Mode B — ACE full cancel-priority scheduling

Same queue, but sorted by `(priority, seq)`. The crank drains all Cancels before processing
any PlaceOrders, then drains all PlaceOrders before any Takes.

### 4.1 Cancel phase (priority=1)

All six cancels fire in their original seq order before any replacement lands.

```
Step  Instruction         Bid   Ask  Total  Depth bar
──────────────────────────────────────────────────────────────────────────
  0   [initial]            45    45     90  ██████████████████
  C1  Cancel bid@97        25    45     70  ██████████████
  C2  Cancel ask@103       25    25     50  ██████████
  C3  Cancel bid@96        10    25     35  ███████
  C4  Cancel ask@104       10    10     20  ████
  C5  Cancel bid@95         0    10     10  ██
  C6  Cancel ask@105        0     0      0  (empty)
```

After the cancel phase the book is completely empty:

```
  ASK  (no orders)
       ─────── oracle 103 ───────
  BID  (no orders)

  Bid total = 0   Ask total = 0   Grand total = 0
```

### 4.2 Place phase (priority=2)

All six placements land, also in their original seq order.

```
Step  Instruction         Bid   Ask  Total  Depth bar
──────────────────────────────────────────────────────────────────────────
  P1  Place  bid@100       20     0     20  ████
  P2  Place  ask@106       20    20     40  ████████
  P3  Place  bid@99        35    20     55  ███████████
  P4  Place  ask@107       35    35     70  ██████████████
  P5  Place  bid@98        45    35     80  ████████████████
  P6  Place  ask@108       45    45     90  ██████████████████
```

After the place phase the book is fully restored at the new oracle-referenced prices:

```
  ASK  108 │ ████████                  10  (MM-C NEW)
       107 │ ██████████████            15  (MM-B NEW)
       106 │ ████████████████████      20  (MM-A NEW)
           │ ─────── oracle 103 ───────
       100 │ ████████████████████      20  (MM-A NEW)
        99 │ ██████████████            15  (MM-B NEW)
  BID  98  │ ████████                  10  (MM-C NEW)

  Bid total = 45   Ask total = 45   Grand total = 90
```

### 4.3 Take phase (priority=3)

```
  Fill   Price    Qty    Note
  ─────────────────────────────────────────────────────────────
    1      106     20    MM-A NEW ask — fully consumed
    2      107     15    MM-B NEW ask — fully consumed
    3      108      5    MM-C NEW ask — partial (10 − 5 = 5 remaining)
  ─────────────────────────────────────────────────────────────
  Total     40 units     Avg fill = (20×106 + 15×107 + 5×108) / 40
                                  = (2120 + 1605 + 540) / 40 = 106.625
```

---

## 5. Mode C — cancel-priority only (Place and Take share priority=2)

Cancels retain priority=1 and drain first — identical to Mode B up to that point. But
PlaceOrders and Takes are both assigned priority=2, so within that tier the crank falls back
to seq order. The Take at seq=5 lands after MM-A's two placements (seq=2, 4) but before
MM-B and MM-C have placed anything (seq=7, 9, 11, 13).

### 5.1 Cancel phase (priority=1) — identical to Mode B

```
Step  Instruction         Bid   Ask  Total  Depth bar
──────────────────────────────────────────────────────────────────────────
  0   [initial]            45    45     90  ██████████████████
  C1  Cancel bid@97        25    45     70  ██████████████
  C2  Cancel ask@103       25    25     50  ██████████
  C3  Cancel bid@96        10    25     35  ███████
  C4  Cancel ask@104       10    10     20  ████
  C5  Cancel bid@95         0    10     10  ██
  C6  Cancel ask@105        0     0      0  (empty)
──────────────────────────────────────────────────────────────────────────
```

### 5.2 Place+Take phase (priority=2, ordered by seq)

PlaceOrders and the Take are merged into a single priority tier and processed by seq. The book
is being rebuilt from scratch when the Take fires mid-way through the restoration.

```
Step  Instruction         Bid   Ask  Total  Depth bar
──────────────────────────────────────────────────────────────────────────
  P1  Place  bid@100       20     0     20  ████
  P2  Place  ask@106       20    20     40  ████████
  5 ► TAKE   buy×40 ──────────────────────────────────────────────────────
      post-Take            20     0     20  ████        20/40 filled, 20 unfilled
  P3  Place  bid@99        35     0     35  ███████
  P4  Place  ask@107       35    15     50  ██████████
  P5  Place  bid@98        45    15     60  ████████████
  P6  Place  ask@108       45    25     70  ██████████████
──────────────────────────────────────────────────────────────────────────
Peak = 90    Minimum = 0 (after C6)    Book empties: YES
```

### 5.3 Book at moment of Take execution

The Take fires after MM-A's ask@106 lands (seq=4) but before MM-B's bid@99 (seq=7).
Only MM-A's pair of orders exists on the book.

```
  ASK  106 │ ████████████████████      20  (MM-A NEW — only ask available)
           │ ─────── oracle 103 ───────
       100 │ ████████████████████      20  (MM-A NEW — only bid)
  BID

  Thin book: one MM has placed, two have not. Take exhausts the only available ask.
```

### 5.4 Taker fill

```
  Fill   Price    Qty    Note
  ─────────────────────────────────────────────────────────────
    1      106     20    MM-A NEW ask — fully consumed
  ─────────────────────────────────────────────────────────────
  Requested:  40 units
  Filled:     20 units   (50% fill rate — book exhausted before Take completed)
  Unfilled:   20 units   (no asks remained; unmatched portion of Take is lost)
  Avg price:  106.00
```

The taker hits no stale prices — Cancels already drained the old quotes — but only gets half
their requested size because MM-B and MM-C's replacements haven't landed yet.

---

## 6. Side-by-side comparison

### 6.1 Depth curves

```
FIFO (Mode A)
──────────────────────────────────────────────────────────────────────────
 90 ┤○   ○   ○                         ○   ○   ○
 70 ┤  ○   ○
 65 ┤            ○   ○
 55 ┤              ○
 50 ┤            ○ ○
 35 ┤               ○
  0 ┤
    └────────────────────────────────────────────────────────────────────→
      0   1   2   3   4   T   6   7   8   9  10  11  12  13
                      ↑
                   Take (mid-crank, against partly-stale book)
  Min depth: 35   Book empties: No

Mode C — cancel-priority only, Place+Take share pri=2
──────────────────────────────────────────────────────────────────────────
 90 ┤○
 70 ┤   ○                                                ○
 60 ┤                                                ○
 50 ┤        ○                                   ○
 40 ┤            ○                          ○
 35 ┤                ○               ○
 20 ┤                    ○       ○        ○
 10 ┤                       
  0 ┤                        ●← empty     ↑ Take (partial: 20/40 filled)
    └────────────────────────────────────────────────────────────────────→
      0  C1  C2  C3  C4  C5  C6  P1  P2   T  P3  P4  P5  P6
         [──── cancel phase ────][─── place+take by seq ───]
  Min depth: 0   Book empties: Yes   Take fires mid-restoration

Mode B — full ACE priority (Cancel < Place < Take)
──────────────────────────────────────────────────────────────────────────
 90 ┤ ○                                             ○
 80 ┤
 70 ┤     ○                                      ○
 55 ┤
 50 ┤                                                     ○
 40 ┤        ○                               ○
 35 ┤            ○                       ○
 20 ┤                ○               ○
 10 ┤                    ○       ○
  0 ┤                        ●← empty                     ↑ Take
    └────────────────────────────────────────────────────────────────────→
      0  C1  C2  C3  C4  C5  C6  P1  P2  P3  P4  P5  P6   T
         [──── cancel phase ────][───── place phase ─────]  ↑
                                                          Take (post-recovery)
  Min depth: 0   Book empties: Yes   Take fires after full restoration
```

### 6.2 Outcome table

```
Metric                        FIFO (A)       Cancel-only (C)    Full ACE (B)
──────────────────────────────────────────────────────────────────────────────
Minimum book depth               35                0                  0
Book reaches empty               No               Yes                Yes
Stale prices visible to taker   Yes (104, 105)    No                 No
Taker fill quantity              40               20 (partial)        40
Taker avg fill price            105.00           106.00             106.625
Taker fill rate                 100%              50%               100%
Taker pays extra vs Mode A        —              +1.00/unit         +1.625/unit
MM stale orders swept by Take   Yes               No                 No
MM requote protection           Partial           Full               Full
Post-crank book depth            75               70                 55
──────────────────────────────────────────────────────────────────────────────
```

Mode C produces the worst taker outcome: oracle-current prices (no stale sweeps) but only a
partial fill. The 20 unfilled units are simply lost — they don't rest on the book or retry.
From a taker's perspective this is worse than Mode A (partial fill at current prices) and
worse than Mode B (full fill at current prices).

### 6.3 Book at moment of Take execution — all three modes

```
Mode A (FIFO) — mixed stale/new:

  ASK  106 │ ████████████████████  20  MM-A NEW
       105 │ ████████              10  MM-C OLD  ← stale (should be 108)
       104 │ ██████████████        15  MM-B OLD  ← stale (should be 107)
           │ ─────── spread ───────
       100 │ ████████████████████  20  MM-A NEW
        96 │ ██████████████        15  MM-B OLD  ← stale
  BID  95  │ ████████              10  MM-C OLD  ← stale
```
```
Mode C (cancel-only) — clean but thin:

  ASK  106 │ ████████████████████  20  MM-A NEW  ← only ask; exhausted by Take
           │ ─────── spread ───────
       100 │ ████████████████████  20  MM-A NEW  ← only bid
  BID
           │ (MM-B, MM-C not yet placed)
```
```
Mode B (full ACE) — clean and fully restored:

  ASK  108 │ ████████              10  MM-C NEW
       107 │ ██████████████        15  MM-B NEW
       106 │ ████████████████████  20  MM-A NEW
           │ ─────── spread ───────
       100 │ ████████████████████  20  MM-A NEW
        99 │ ██████████████        15  MM-B NEW
  BID  98  │ ████████              10  MM-C NEW
```

---

## 7. The V-shaped depth curve

Every crank cycle in ACE (Modes B and C) produces a characteristic V-shaped depth profile —
the book empties during the cancel phase and refills during the place phase. The difference is
where the Take lands on that V:

```
Total book depth — one crank cycle

Mode B (full priority):

  90% ┤ ████████████████   (pre-crank, stable)             ████████████████   (post-crank)
      │ ████████████████                                   ████████████████
  50% ┤ ████████████████       █████████████████████████████████████████████
      │ ████████████████   ██████████████████████████████████████████████████
   0% ┤                ═══  (momentarily empty)                              ↑ Take
      └────────────────────────────────────────────────────────────────────────→
               cancel phase        place phase fully completes     Take fires here

Mode C (cancel-only priority):

  90% ┤ ████████████████   (pre-crank, stable)        ████████████████████████   (post-crank)
      │ ████████████████                             ████████████████████████████
  50% ┤ ████████████████      ██████████████████████████████████████████████████
      │ ████████████████   ███████████████████████████████████████████████████████
   0% ┤                ═══  (empty)  ↑ Take fires mid-restoration
      └────────────────────────────────────────────────────────────────────────→
               cancel phase       partial place phase   Take   remaining places
```

In Mode B, the Take always rides the right side of the V — the book is at its post-recovery
maximum. In Mode C, the Take lands on the ascending slope of the V — the book is partially
rebuilt but not complete. The earlier the taker's seq relative to the MMs' placements, the
thinner the book they execute against.

---

## 8. Multi-MM amplification

The V-trough deepens linearly with the number of MMs requoting simultaneously. The key
difference between Modes B and C is where within the ascending slope the Take fires:

```
                    FIFO (A)             Mode C               Mode B
                    ────────             ──────               ──────
1 MM requoting:

  depth  90 ─ ○─○─○─○─○─○─○      90 ─ ○           ○        90 ─ ○               ○
         70 ─       (small dips)    0 ─    ○───○ ↑ ○         35 ─    ○           ○
          0 ─                               Take               0 ─       ○───────○
                                           (partial)                  cancel   place
                                                                       (V of depth 1)

3 MMs requoting (our scenario):

  depth  90 ─ ○─○─○─○─○─○─○      90 ─ ○                ○    90 ─ ○                             ○
         35 ─       (moderated)    0 ─    ○─────○ ↑ ○         0 ─    ○─────────────○
          0 ─                                 Take                   (wider empty trough)
                                          (partial: ~1/3
                                           of restores done)

10 MMs requoting:

  depth  90 ─ ○─○─○─○─○─○─○      90 ─ ○                 ○   90 ─ ○                                           ○
          0 ─                      0 ─     ○──────────○ ↑ ○    0 ─     ○──────────────────────────────────○
                                                      Take                10 cancel steps     10 place steps
                                               (partial: only                 (extended empty trough)
                                                1 MM's ask filled)
```

In Mode C with many MMs, the taker's effective fill rate degrades as N grows: the Take fires
at its fixed seq position while the total number of placements before it stays the same (only
MM-A's two orders land before seq=5). The taker always fills against just MM-A's ask@106 — 20
units regardless of how many MMs there are — while requested size may be much larger.

---

## 9. Implications

### For takers

**Mode A (FIFO):** Full fill guaranteed if book has enough depth. Partly fills against stale
(pre-oracle-move) prices, which can be beneficial if the oracle moved against the taker's
direction, or harmful if the taker is sweeping old levels that should have been requoted.

**Mode C (cancel-only):** No stale prices, but the taker competes with incoming placements for
queue position. A taker with a low seq (submitted before most MMs placed) fires into a
partially restored book and receives a partial fill. The unfilled units are gone — they don't
rest as a resting order or retry in a later slot.

**Mode B (full ACE):** Full fill against a clean, fully current book. Higher average price than
Mode A but guaranteed complete execution if the limit price covers available depth. The "price
discovery tax" (1.625 extra per unit in the example) is the cost of unambiguous current-price
fills.

### For market makers

All three modes with cancel priority (B and C) provide the same stale-price protection: the
cancel always fires before any taker can sweep. The difference is operational: in Mode B, MMs
know their replacements are always live before any take runs, so they can quote tighter without
fear of partial-slot exposure. In Mode C, there is a window where a taker and a MM's placement
race on seq, adding uncertainty about whether the replacement was visible when the take fired.

### For the book-as-signal

```
Mode A:  stable depth throughout crank — reliable near-real-time signal
Mode C:  depth collapses to 0, then climbs; Take interrupts the climb — depth is unreliable
         during the restoration window
Mode B:  depth collapses to 0, then climbs fully, then Take fires — predictable two-phase
         signal; by the time a Take can execute the book is always at full restored depth
```

### Summary table

```
Property                      FIFO (A)      Cancel-only (C)    Full ACE (B)
────────────────────────────────────────────────────────────────────────────
Minimum depth during crank    ~steady-state  0                  0
Book stability (depth)        High           Low                Low
MM stale-price protection     Partial        Full               Full
Taker fill rate               100%           Seq-dependent      100%
Taker fill price (requoting)  Partly stale   Fully current      Fully current
Cancel-race safety            No             Yes                Yes
Predictable book at Take time No (mixed)     No (partial)       Yes (full)
────────────────────────────────────────────────────────────────────────────
```

Mode C occupies an awkward middle ground: it delivers MM protection identical to Mode B, but
degrades taker fill rates without any compensating benefit. A taker in Mode C may receive fewer
units at a higher per-unit price than in Mode A, making it the strictest regime from the
taker's perspective. Mode B at least guarantees a complete fill at current prices.
