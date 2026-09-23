# tick-sim results so far

This report summarises what tick-sim has measured so far and what it means
for building the game server. It is written for readers who haven't worked
with threads or CPU scheduling; the terms are explained in the glossary at
the end.

For design work, the two sections to start from are **Summary** (what things
cost and what the server can handle) and **Design rules** (what to build
because of it). The rest of the report is the evidence behind them.

The program that produced these numbers, and how to run it yourself, is at
https://github.com/FluffyByteSoftware/tick-sim. Every result below can be
repeated with it.

**Status:** the tick timing, CPU load, simulated player, login hashing, voxel
world and disk saving tests are done. The server will be hosted on the same
desktop these tests ran on, so the numbers describe the real host,
including while a game is being played on it; see **Hosting on the dev
desktop**.

## Summary: what the server can handle

Every number here comes from one desktop machine (8 cores, 16 logical CPUs,
up to 5.0 GHz), with one thread doing all the game work, and with test work
that is simpler than real game logic. Treat them as best cases: a server CPU
may be slower per core, and real logic costs more.

### What each thing costs

| Thing | Cost | Runs on |
|---|---|---|
| Reaching one NPC (5,000 NPCs, cheap update) | about 0.02 µs per tick | Tick thread |
| One player's network traffic | about 8 µs per tick at 50 players, 4 µs at 500 | Tick thread |
| One loaded chunk's random ticks (24 checks) | about 1 µs per tick | Tick thread |
| Copying one dirty chunk for saving | about 12–15 µs, each time it is saved | Tick thread |
| One login's password check (Argon2id, 64 MiB, 2 passes) | about 70–85 ms | Its own thread |
| Writing one 1.4 MiB region file, NVMe drive | about 1.4 ms | Disk writer threads |
| Writing one 1.4 MiB region file, spinning drive | about 30 ms | Disk writer threads |
| Writing one small file per chunk, spinning drive | about 11 ms each, so at most about 90 a second | Disk writer threads |

When the machine is busy with other work, everything on the tick thread
costs 2–3 times more.

A login's password check is by far the most expensive single event, more
than a whole tick's budget. It doesn't run on the tick thread, so the tick
only feels it through competition for memory. With a limit on how many run
at once, that is small (ticks went from 2.8 ms to 4.4 ms during a 50-player
rush). With no limit, it is the worst thing measured (ticks averaged 56 ms
for about 1.6 seconds after every mass reconnect).

### The tick budget

- A tick has **50 ms**. Going over skips the next tick.
- Single ticks can take 2–3 times the average, unpredictably. So plan for an
  **average of about 20 ms**, leaving room for the bad ones.

One way to divide those 20 ms:

| Share | Budget | Limit it sets |
|---|---|---|
| Voxel upkeep (random ticks) | 5 ms | about 5,000 simulated chunks (about 160 million voxels) |
| Saving, a little every tick | 1–3 ms | measured 1.2 ms at about 100 chunks per tick |
| Networking | 0.5 ms | about 50 players (500 players measured at 2 ms) |
| **NPC behaviour** | **about 11 ms** | **at 5,000 NPCs, about 2 µs per NPC per tick** |

**The real limit is how much work each NPC does per tick, not how many NPCs
there are.** At about 2 µs each, 5,000 NPCs fit. At about 10 µs each, the
limit drops to about 1,000. 2 µs is enough for simple state checks and
movement, but not for a pathfinding search every tick.

**The number of voxels is not a limit in itself.** Voxels sitting in memory
cost nothing per tick, only memory (2 bytes each). What costs tick time is
how many chunks are *simulated* each tick and how many are *saved* each
tick.

### What hurt the tick most, worst first

1. **Password checks with no limit on how many run at once.** The whole
   tick went over budget during a login rush (result 8).
2. **Saving every changed chunk in one tick.** A 128 ms tick at 16,384
   chunks (result 9).
3. **Random ticks across a large loaded world.** A steady 17 ms at 16,384
   chunks (result 9).
4. **A busy machine.** Everything took 2–3 times longer (result 2).
5. **Networking for 50 players:** 0.4 ms (result 6).
6. **Reaching 5,000 NPCs:** 0.1 ms (result 9).

Off the tick thread, the worst problem measured was saving that couldn't
keep up with the disk: one file per chunk on the spinning drive fell a
minute behind and piled up about 4 GiB of waiting chunk copies (result 10).

The biggest cost of all isn't on this list, because it hasn't been built
yet: real NPC behaviour.

## Hosting on the dev desktop

The server will run on the desktop these tests ran on, for about 10 players
and about 10 unique NPCs, with the world kept "hot" (simulated) only near
them. That is far below the 50 players the rest of this report plans for,
so everything below has plenty of room.

| Resource | Expected use | Based on |
|---|---|---|
| Tick thread | about 6 ms of every 50 ms tick (about 12% of one logical CPU) on a quiet machine, about 9 ms with a game running, before real NPC logic | results 10 and 13, with 4,096 hot chunks and 5,000 NPCs |
| Saver thread (compressing) | under 10% of one logical CPU; much less with a save interval (rule 17) | result 10 |
| Disk writer threads | mostly waiting on the disk, very little CPU | result 10 |
| Login worker | about 70 ms of one logical CPU per login | result 11 |
| Networking on the tick | about 0.14 ms per tick | result 10 |
| Upload bandwidth | about 24 KB/s per player (a 1.2 KB update 20 times a second), about 2 Mbit/s for 10 players | calculated from packet sizes, not measured |
| Memory | about 0.5 GiB typical: the world (256 MiB at 4,096 chunks), the saver's compressed copy (about 90 MiB), NPCs (about 12 MiB), 64 MiB during each login, plus the disk writer's cache (usually small, capped at 2 GiB) | results 9–11 |
| Disk, with a 30-second save interval | at most about 3 MiB/s even if every region changed; far less in real play | calculated from result 10 |

In total that is roughly one of the desktop's 16 logical CPUs and about half a
gigabyte of memory, which leaves nearly everything for playing on the same
machine.

Recommendations for this host:

- **Save the world to the NVMe drive** (or the SSD), with a save interval
  per region (rule 17). The spinning drive works with region files and an
  interval, but it was already at its limit in the worst-case test, and it
  cannot handle one file per chunk at all. Keep it for backups.
- **Leave the 4 GiB/minute disaster of result 10 impossible by design:** a
  bounded handoff between the tick and the saver (rule 18), and region files
  instead of one file per chunk (rule 19).
- **Plan the tick budget for game night, not a quiet machine.** With a game
  running, the average tick was about 1.4 times slower, the slow ticks about
  twice as slow, and one tick reached 88 ms (result 13). The 20 ms average
  target (rule 1) still leaves room, but the extra time real NPC logic can
  use is closer to 10 ms than 14.
- **If game-night spikes become a problem,** raise the server's tick thread
  priority rather than pinning it (rule 3).

## The question

The game server runs a **tick**: every 50 milliseconds (20 times a second) it
wakes up, updates the world, talks to players, and goes back to sleep until
the next tick. We wanted to know:

1. Does each tick actually start on time, or does it wake up late?
2. How much world (how many NPCs) can one tick update before it runs out of
   time?
3. Does "pinning" the tick to one fixed CPU core help, or is it not worth
   adding?
4. How much do connected players cost?

## The test machine

- Intel CPU with 8 physical cores and 16 logical CPUs (hyperthreading), up
  to 5.0 GHz
- Nobara Linux with the KDE Plasma desktop running
- All tests ran for 15 seconds (300 ticks) unless noted

## How the test works, briefly

The program fills memory with fake world objects of 256 bytes each. Every
tenth object is treated as an NPC and gets its position updated on every
tick. That update is deliberately cheap (a little arithmetic per NPC), so the
test measures the cost of *reaching* each NPC in memory rather than any real
game logic.

Tests were run under several conditions:

- **Idle:** the machine running only the test and a normal desktop.
- **Full load:** 16 extra threads doing nonstop arithmetic, keeping every
  logical CPU busy. This is a harsh worst case.
- **In order vs. shuffled:** NPCs updated in the order they sit in memory,
  or in a random order. Random order is closer to how a real world's NPCs
  would be scattered.
- **50 players:** 50 simulated players sending input 30 times a second and
  receiving a world update (about 1.2 KB) from the server every tick.
- **Free vs. pinned:** the tick allowed to run on any CPU core, or fixed to
  one.
- **Voxel world:** a block world of up to 537 million voxels, kept up to date
  every tick and saved every 5 seconds (see result 9).

For each tick, two things were recorded: how late it woke up, and how long
its work took. A tick has 50 ms of budget. If the work takes longer than
that, the next tick is missed.

## How to read a tick-sim result

Every run prints one block of lines. Here is a real one, from the largest
voxel test (537 million voxels, 5,000 NPCs, 50 players), with each line
explained below it.

```
=== objects 50000 (12 MiB) | NPCs 5000 (in order) | free | load 0 | players 50 | 50 ms x 30 s ===
ticks 568  missed deadlines 11  work > period 5  worst tick used 329.9% of budget
wake late  min     7.879  avg    75.067  p50    59.976  p99   180.760  max   195.661  (us)
work time  min    15.994  avg    18.538  p50    16.815  p99    34.443  max   164.966  (ms)
  npc part min     0.092  avg     0.114  p50     0.104  p99     0.202  max     0.351  (ms)
  net part min     0.344  avg     0.404  p50     0.388  p99     0.617  max     0.848  (ms)
  voxels   min    15.508  avg    16.894  p50    16.307  p99    23.818  max    44.652  (ms)
  saving   min   113.419  avg   127.864  p50   127.390  p99   146.376  max   146.376  (ms)
packets    inputs sent 46950 recv 44963 (send failed 0) | snapshots sent 29400 recv 29350 (dropped 0) | errors 0
voxels     16384 chunks = 536.9M voxels (1024 MiB) | random ticks 24/chunk | edits 100/tick | changes 101.1/tick | chunks saved 37688
saver      compressed 37688 chunks, 2355.5 MiB -> 810.4 MiB, busy 1766 ms in total
```

**The `===` line** is the setup, so every pasted result says what produced
it: 50,000 objects of which 5,000 are NPCs, the tick free to run on any CPU
core, no extra CPU load, 50 players, a 50 ms tick, 30 seconds.

**The `ticks` line** is the headline:

- `ticks 568` ran, out of roughly 600 possible in 30 seconds (the first 20
  are left out of the stats as a warm-up).
- `missed deadlines 11`: eleven times, a tick ran so long that the next
  one's start time had already passed, and that tick was skipped.
- `work > period 5`: five ticks took longer than 50 ms themselves.
- `worst tick used 329.9% of budget`: the slowest tick took 3.3 times the
  50 ms it was allowed.

**The rows of numbers** all have the same shape. Each describes one
measurement across every tick in the run:

- `min` is the best tick and `max` the worst.
- `avg` is the average.
- `p50` is the middle tick: half were faster, half slower.
- `p99` is the tick that 99% were faster than. It shows how bad the bad
  ticks usually get, without being decided by one freak tick the way `max`
  is.
- The unit is at the end: `us` is microseconds (thousandths of a
  millisecond), `ms` is milliseconds.

**`wake late`** is how late each tick started, in microseconds. 75 µs on
average is nothing against a 50 ms tick.

**`work time`** is how long each tick's work took in total, and the indented
rows below it split that total into parts:

- `npc part`: moving the NPCs, 0.11 ms.
- `net part`: reading player input and sending world updates, 0.4 ms.
- `voxels`: the voxel world's upkeep, 16.9 ms. This is where the time goes.
- `saving`: only counts ticks on which a save happened (one every 5
  seconds). Each of those took 128 ms on average, over two and a half
  ticks' worth of time. **That one row explains every missed deadline in
  this run.**

To find what is eating the budget, compare the `avg` of each part against
the `avg` of `work time`. To find what causes missed ticks, look for a part
whose `max` is over 50.

**The `packets` line** compares what was sent with what arrived. The server
received 44,963 of 46,950 player inputs: about 2,000 (4%) were lost, because
during each save the tick went so long without reading its network input
that the operating system started throwing packets away (result 7).

**The `voxels` line** describes the world: 16,384 chunks holding 537 million
voxels in 1 GiB of memory. `changes 101.1/tick` is how many voxels actually
changed each tick: the 100 player edits, plus about one change from the
block rules. `chunks saved 37688` is how many chunk copies were handed to
the saver across the run.

**The `saver` line** is the background save thread: it compressed 37,688
chunks from 2.4 GiB down to 0.8 GiB, and was busy for 1.8 seconds of the 30.

## Results

### 1. Ticks wake up on time

| Condition | Average lateness | Worst lateness seen |
|---|---|---|
| Idle | 0.06–0.09 ms | 0.4 ms |
| Full load | 0.2–0.6 ms | 5.6 ms |

Even in the worst case under full load, a tick woke up about 5 ms late out of
a 50 ms budget. On average it's well under 1 ms. Linux's ordinary sleep and
wake-up is accurate enough for a 20-ticks-per-second server. **Waking up on
time is not the problem.**

### 2. The work is what runs out of time

Average time to update the NPCs, per tick (50 ms is the limit):

| NPCs | Idle, in order | Full load, in order | Full load, shuffled |
|---|---|---|---|
| 10,000 | 0.2 ms | 0.3 ms | 0.4 ms |
| 100,000 | 2.2 ms | 4.7 ms | 7.5 ms |
| 400,000 | 8.1 ms | 22 ms | 31 ms |
| 1,000,000 | 21 ms | 55 ms (**fails**) | 77 ms (**fails**) |

That works out to roughly 20 billionths of a second (20 ns) per NPC on an
idle machine, about 55 ns under full load, and about 75 ns under full load
with shuffled access.

Three things stand out:

- **Cost grows in a straight line with NPC count.** Twice the NPCs, twice the
  time.
- **A busy machine makes the same work 2–3 times slower.** Under full load the
  tick has to share its CPU core with other threads, and the CPU runs at a
  lower clock speed when every core is busy.
- **Scattered data costs 40–60% more.** When NPCs are visited in memory order,
  the CPU can fetch the next one before it's needed. When they're scattered,
  it can't do that as well.

### 3. When a tick runs out of time, the tick rate halves

At 1 million NPCs under full load, every tick took longer than 50 ms. The
server didn't run "a bit late": it completed about 130 ticks in 15 seconds
instead of 300. Each overrunning tick caused the next one to be skipped, so
the server effectively ran at 10 ticks a second instead of 20.

### 4. The slowest ticks are much slower than the average, and unpredictable

The same test was run three times. Averages moved by about 10–20% between
runs. The slowest single tick moved far more. At 1 million NPCs on an idle machine with 50 players,
two identical runs gave:

| | Average | Slowest tick | Missed ticks |
|---|---|---|---|
| First run | 24 ms | 33–43 ms | 0 |
| Second run | 24–28 ms | 64–66 ms | 1–3 |

Occasional ticks took 2–3 times the average, even on an idle machine, and
there's no way to predict when.

### 5. Pinning makes no reliable difference

Across every test, pinning the tick to one core was sometimes a little better
and sometimes a little worse, with no consistent direction. The differences
were the same size as the difference between two identical runs. Under full
load, pinning can in principle make things worse, because a pinned tick can't
move away from a busy core.

### 6. Fifty players are cheap

| Condition | Network time per tick (average) | Slowest tick |
|---|---|---|
| Idle | 0.35–0.43 ms | 1.2 ms |
| Full load | 0.6–1.3 ms | 5.8 ms |

Receiving everyone's input and sending 50 world updates took under half a
millisecond per tick on an idle machine, about 7 microseconds per player. The
cost stayed the same whether the world had 10,000 NPCs or 1,000,000. No
packets were lost in any normal run.

### 7. Overrunning ticks lose player input

In one full-load run where ticks were badly overrunning (1 million NPCs,
about 10 ticks a second), the server lost 337 of about 22,500 player input
packets, around 1.5%. The operating system only holds a limited amount of
unread network data for the server. When the tick fell behind and didn't
collect it in time, new packets were thrown away.

### 8. Too many logins at once can stall the whole server

When a player logs in, the server checks their password with Argon2, a
password-hashing method that is slow and memory-hungry on purpose, so that
stolen password files are expensive to crack. At our settings (64 MiB of
memory, 2 passes), one check takes about 85 ms.

In our login design, each connection checks its own password on its own
thread, so the number of checks running at once equals the number of
players logging in at that moment. We simulated the worst normal case: all
50 players reconnecting at once after a server restart. Then we ran the
same rush again with at most 2 checks allowed at a time, and the rest
waiting in line.

| | All 50 at once | 2 at a time |
|---|---|---|
| Time for one password check | 1.2 s average, up to 1.6 s | 84 ms average, up to 98 ms |
| How long the rush lasts | about 1.6 s | about 2.1 s |
| Average tick work during the rush | 56 ms (over budget) | 4.4 ms |
| Slowest tick | 127 ms | 11 ms |
| Missed ticks (5 rushes in 60 s) | 57 | 0 |

(Ticks outside a rush averaged 2.8 ms in both runs.)

With all 50 at once, the checks fought each other for memory and CPU. Each
one took about 14 times longer than normal, and the game tick ran over
budget for the whole rush. With 2 at a time, each check ran at normal
speed, the tick barely noticed, and the whole rush finished only about half
a second later, because checks that aren't fighting each other finish much
faster.

A smaller rush tells the same story at lower cost. With 19 MiB per check
instead of 64, 50 at once still took 280 ms per check and stalled the tick,
while 4 checks every 2 seconds made the tick about 2.5 times slower for the
few ticks they overlapped, without missing any.

There is a security side to this too. The server makes every login attempt
take at least 150 ms, so an attacker can't tell from the reply time whether
a username exists (a made-up name skips the password check). With 50 checks
at once, real logins took over a second while made-up names still answered
at 150 ms, so for the length of the rush the reply time gives away which
usernames are real.

### 9. The voxel world: upkeep grows with its size, and saving all at once breaks the tick

The voxel test adds a block world on top of 5,000 NPCs and 50 players. The
world is split into **chunks** of 32 × 32 × 32 voxels (64 KiB each). Every
tick:

- **Random ticks:** in every chunk, 24 voxels are picked at random and
  checked against simple rules (grass spreads onto nearby dirt, grass under
  a block turns to dirt). Block games use this to make slow changes happen
  across the world without checking every voxel.
- **Edits:** 100 random voxels are dug out or filled in, standing in for
  players.
- **Saving:** a chunk that changed is marked **dirty**. Every 5 seconds, the
  tick copies every dirty chunk and hands the copies to a separate saver
  thread, which compresses them. Nothing was written to disk.

| Chunks | Voxels | Memory | Upkeep per tick | Chunks copied per save | Time for a save tick | Missed ticks in 30 s |
|---|---|---|---|---|---|---|
| 1,024 | 34 million | 64 MiB | 1.2 ms | about 1,020 (all) | 12.6 ms | 0 |
| 4,096 | 134 million | 256 MiB | 4.4 ms | about 3,750 (92%) | 58 ms | 5 |
| 16,384 | 537 million | 1 GiB | 16.9 ms | about 7,540 (46%) | 128 ms | 11 |

What the table shows:

- **5,000 NPCs cost almost nothing.** Moving them took about 0.1 ms per tick
  in every run.
- **Upkeep grows in a straight line with the number of chunks:** about 1 ms
  per 1,000 chunks, or about 43 billionths of a second per voxel checked. At
  16,384 chunks it took a third of the tick budget on its own.
- **Almost all of that checking finds nothing to do.** Of the roughly
  393,000 voxels checked per tick in the largest world, about one changed.
  Most checks land on stone or air, which no rule ever changes.
- **Saving is the part that breaks.** Copying one chunk costs about 12–17
  microseconds, and every dirty chunk is copied in the same tick. At 4,096
  chunks every save tick went over budget; at 16,384 each save took 128 ms,
  skipping two or three ticks and losing player input.
- **Nearly the whole world was dirty at every save** in the smaller worlds,
  because the 100 edits a tick were spread across the entire world at
  random. Real players' edits would cluster around where they are standing,
  so real saves would usually be much smaller. But explosions, large
  building projects or many players spread out could still produce big
  saves.
- **The saver thread itself is cheap.** Compressing everything took 1.8
  seconds of work in 30 seconds, about 6% of one CPU core. The simple
  compression used only shrank chunks to about a third, so a real save
  format should do better.

This test world is a worst case in one important way: every chunk contains
a stretch of ground surface. In a real world, most chunks are either all
sky or all solid rock.

### 10. Saving a little every tick fixes the spikes; the disk decides the rest

These runs used the 134-million-voxel world (4,096 chunks), 5,000 NPCs and
50 players (10 in the NVMe run), for 60 seconds, copying up to 200 dirty
chunks per tick to the saver instead of all of them every 5 seconds. With a
disk, chunks were grouped into region files of 64 chunks (about 1.4 MiB
each), written the same way as the game server's disk writer: each file
written in full to a temporary name, forced onto the disk, then renamed
over the old one. The writer merges saves: if a region is saved again while
its previous copy is still waiting to be written, only the newest copy is
written.

| Run | Average tick | Saving per tick, avg (worst) | Missed ticks | Files written | Data written | Saving finished after the game |
|---|---|---|---|---|---|---|
| No disk | 6.0 ms | 1.2 ms (2.3 ms) | 0 | – | – | about 0.3 s later |
| Spinning drive, 64-chunk regions | 6.0 ms | 0.9 ms (3.8 ms) | 0 | 2,049 | 2.8 GiB, 45 MiB/s | about 4 s later |
| Spinning drive, one file per chunk | 7.3 ms | 2.3 ms (55 ms) | 1 | 10,543 | 0.2 GiB, 1.9 MiB/s | about 63 s later |
| NVMe drive, 64-chunk regions | 6.2 ms | 1.0 ms (4.1 ms) | 0 | 43,047 | 58 GiB, 990 MiB/s | under 1 s later |

(How far behind saving was is worked out from the simulated players, who
carried on sending until saving had finished; the program now reports it
directly.)

What the table shows:

- **Saving a little every tick removed the spikes.** Instead of a 58 ms tick
  every 5 seconds (result 9), each tick spent about 1 ms saving, with no
  missed ticks. In practice about 100 chunks were copied per tick, one per
  change.
- **But it saves every change separately.** A chunk changed on ten ticks is
  copied ten times: 120,664 chunk copies in a minute, against about 37,500
  when saving everything every 5 seconds. Waiting a little before saving a
  chunk would merge those.
- **On the spinning drive, the writer's merging is what kept it working.**
  The saver handed over 61,347 region files; all but 2,049 were replaced by
  a newer copy before they were written. Even so, the drive was busy for the
  entire run: it was at its limit. A slower disk doesn't break this design;
  it just means more merging, and a longer delay before a change is safely
  on disk.
- **On the NVMe drive, the same design wrote 58 GiB in one minute.** The fast
  disk finished each batch so quickly that almost nothing was merged: every
  region file was rewritten about 11 times a second. That is pointless work,
  and at that rate for hours (about 3.5 TB an hour) it would wear out a
  typical consumer SSD, which is rated for a few hundred terabytes written in
  its lifetime, within weeks. Writing as fast as the disk allows is the wrong
  goal; each region needs a minimum time between saves.
- **One file per chunk on the spinning drive fails.** Forcing each small file
  onto the disk separately costs about 11 ms, so the drive managed about 90
  files a second while about 2,000 chunks a second were changing. The
  writer's cache was full for the whole run, the saver spent 115 seconds
  waiting on it, and meanwhile about 63,000 chunk copies (about 4 GiB of
  memory) piled up in the queue between the tick and the saver, because
  nothing limited it. Saving finished a minute after the game stopped. The
  one missed tick, a 55 ms save, probably came from that memory pile-up.
- **The tick itself barely noticed the disk** in the other runs, because all
  disk work happens on other threads.
- **No player input was lost** in any of these runs.

The test world is a worst case: its 100 edits a tick land anywhere in the
world at random, so nearly every region changes constantly. Real players'
edits cluster where they are, so far fewer regions would change.

### 11. One login worker confirmed

The 50-player login rush from result 8 was run again with a single hashing
worker, at the real settings (64 MiB, 2 passes).

- Each password check took 72 ms on average, 118 ms at worst.
- Each rush took about 3.6 seconds to clear (50 × 72 ms).
- Ticks during a rush averaged 3.5 ms against 2.6 ms outside one, with none
  over budget. That is less disturbance than the limit of 2 (1.6 ms extra).

One worker is enough, and costs the game almost nothing. The slowest check,
118 ms, is still under the 150 ms minimum login time, but not by a wide
margin, so that minimum shouldn't be lowered.

### 12. 500 players: cheap for the tick, but the network buffer overflows

With 500 simulated players and no voxel world, networking took 2.0 ms per
tick, about 4 µs per player: cheaper per player than at 50, because some of
the cost is fixed. The tick had plenty of room.

But the server received only 142,306 of 449,500 player inputs: over two
thirds were lost. 500 players sending 30 inputs a second is 15,000 packets
a second into one socket. The operating system only holds a few hundred
small packets for a socket by default, and tick-sim only reads it once per
tick, so most of them were thrown away before they were read. A network
thread that reads continuously, and a larger receive buffer (rule 22), are
what fix this. At 10 players it doesn't arise.

### 13. Game night: hosting while playing on the same PC

The 10-player run from result 10 (NVMe drive, 64-chunk regions) was repeated
with a game running on the desktop at the same time.

| | Quiet machine | Game running |
|---|---|---|
| Average tick | 6.2 ms | 9.0 ms |
| Slow ticks (99th percentile) | 10.3 ms | 22.4 ms |
| Slowest tick | 15 ms | 88 ms |
| Missed ticks in 60 s | 0 | 1 |
| Voxel upkeep, average | 5.0 ms | 7.1 ms |
| Wake-up delay, slowest | 0.45 ms | 1.8 ms |
| Most chunk copies waiting for the saver | 97 | 265 |

- **Everything on the tick got about 1.4 times slower on average,** less
  than the 2–3 times seen when every CPU was kept busy on purpose (result 2).
- **The slow ticks got about twice as slow,** and one tick took 88 ms,
  almost all of it inside the voxel upkeep. The tick thread most likely lost
  its CPU to the game partway through that work.
- **Waking up on time stayed fine:** even the latest wake-up was under 2 ms.
- **Saving kept up,** with a larger but still small queue.

Hosting on the gaming PC works at this scale. The cost is less headroom and
the occasional spike, not a server that can't keep up.

## Design rules

These rules follow from the results above. Each gives the number behind
it, so it can be re-checked if the hardware or the design changes.

### The tick

1. **Keep the average tick under about 20 ms of its 50.** Single ticks run
   2–3 times the average. Anything whose worst case goes over about 30 ms
   should be treated as a bug.
2. **Give every system on the tick its own time budget, and measure it
   separately** (NPCs, voxels, saving, networking), the way tick-sim splits
   its work time into parts. Averages are what to compare; differences under
   about 20% are within run-to-run noise.
3. **Waking up on time is not a problem.** Normal sleeping is accurate
   enough (under 0.1 ms late on an idle machine). Don't add CPU pinning: it
   made no reliable difference. If timing ever becomes a problem, try raising
   the tick thread's priority instead.
4. **Decide what happens when a tick overruns.** Without a policy, one
   overrun skips a whole tick, and a server that keeps overrunning runs at
   half speed. (This is being designed separately.)
5. **Give the server machine headroom.** Other heavy programs on the same
   machine, including a game client during development, make everything 2–3
   times slower.

### NPCs

6. **At 5,000 NPCs, budget about 2 µs of behaviour per NPC per tick** (about
   11 ms in total). Reaching them costs almost nothing (0.1 ms); their logic
   is the cost.
7. **Never run expensive NPC work for every NPC every tick.** Pathfinding
   and searches get spread across ticks with a per-tick cap, or only run for
   NPCs near players.
8. **Group NPCs by what they are doing, and only do the work each group
   needs.** An idle NPC shouldn't pay for pathfinding.
9. **Work out steady changes only when they are needed.** Anything that
   changes at a steady rate (health regeneration, hunger, crop growth) can
   store its value and the tick it was last updated, and calculate the
   current value when something looks at it. Then it costs nothing per tick.
10. **Keep the data the tick touches small and close together.** Scattered
    access was 40–60% slower. Store frequently updated fields (position,
    velocity) together, apart from rarely used data.

### The voxel world

11. **Chunks are 32 × 32 × 32 voxels with 2-byte block ids (64 KiB),** as
    tested.
12. **Keep simulated chunks separate from visible chunks.** Only chunks near
    a player get random ticks (a "simulation distance", smaller than the
    view distance). Keep the total simulated chunks across all players to
    about 5,000 (about 5 ms).
13. **Skip chunks that can't change.** A chunk that is all air or all stone
    has nothing for the block rules to act on. In the test, only about one
    of 393,000 checks per tick changed anything. Mark such chunks and skip
    them, and store a chunk that is all one block as a single value instead
    of 64 KiB.

### Saving

14. **Never copy every changed chunk in one tick.** At about 15 µs per chunk,
    7,500 changed chunks took 128 ms.
15. **Save a fixed number of chunks per tick.** Measured: about 100 chunks
    per tick cost 1.2 ms, with no spikes and no missed ticks (result 10).
    The alternative, sending the saver a list of changed voxels so it keeps
    its own copy of the world, is untested.
16. **Compression and disk writes happen off the tick thread.** The saver
    used under 10% of one CPU core, and disk writes never slowed the tick
    unless memory piled up (result 10).
17. **Give each region a minimum time between saves,** for example 30
    seconds, and merge the changes in between. Writing as fast as the disk
    allows wrote 58 GiB a minute to the NVMe drive, enough to wear out an SSD
    in weeks. The cost is that a crash can lose up to that much time, which
    the disk writer's design already accepts (players roll back to their
    last save that reached the disk).
18. **Never let the handoff from the tick to the saver grow without limit,
    and never let the tick wait on it.** Use a queue with a fixed size. When
    it is full, leave the chunk marked dirty and try again next tick: the
    dirty flag already merges further changes for free. An unlimited queue
    grew to about 4 GiB when the disk fell behind.
19. **Never save one file per chunk with each file forced onto the disk
    separately,** above all on a spinning drive: about 90 files a second
    was its ceiling. Group chunks into region files; 64 chunks per file
    worked on both drives.

### Networking

20. **Network threads never touch game state.** They hand messages to the
    tick through queues, and take answers back the same way. When the tick
    read the network itself, every overrun lost player input (up to 4%).
21. **50 players cost about 0.4 ms per tick,** about 8 µs each.
22. **Read the UDP socket continuously on the network thread, and ask the
    operating system for a larger receive buffer.** At 500 players the
    default buffer overflowed and lost over two thirds of player input
    (result 12). At 10 players it is insurance.
23. **One thread per TCP connection is fine at up to 100 connections.** Idle
    connection threads waking every 50 ms cost almost nothing. Messages
    queued for a TCP client can wait up to one read timeout (50 ms) before
    they are sent.

### Logins and password hashing

24. **Never let password checks run without a limit.** 50 at once made each
    one take 1.2 seconds instead of 85 ms, put the whole tick over budget,
    and let reply times reveal which usernames exist.
25. **Use one long-lived hashing worker thread.** Connection threads send it
    a request through a queue and wait for the answer. It stays running for
    the life of the server, and sleeps when the queue is empty. Measured
    (result 11): each check took about 72 ms, a 50-player rush cleared in
    about 3.6 seconds, and ticks during the rush took about 0.9 ms more than
    usual, with none over budget.
26. **Don't keep the worker hashing when there is nothing to check.** Hashing
    continuously, to keep its cost steady, would turn the brief cost of a
    login rush into a permanent one on every tick.
27. **Send unknown usernames through the same queue,** checked against a
    fixed dummy hash. Both kinds of login then take the same path and the
    same time, so reply times can't reveal which usernames exist.
28. **Don't count time spent waiting in the queue against the login
    deadline,** and tell the waiting client where it is in the queue, so a
    long line doesn't look like a dead server.
29. **Let each address have only one login waiting at a time,** so one
    address can't fill the queue and lock everyone else out.
30. **Optionally, reuse the worker's hashing memory from one login to the
    next,** instead of allocating 64 MiB each time. The argon2 crate has a
    variant of its hashing function that takes memory the caller provides.
    The benefit is unmeasured.

#### Pacing logins to one per second

Pacing logins to one per second, with the rest waiting in the queue,
protects the tick, but no better than rule 25's single worker running one
check straight after another. In both, only one check runs at a time, and a
50-player rush costs the same 50 × 85 ms, about 4.25 seconds, of hashing in
total. Pacing only spreads that out, so the rush takes about 50 seconds
instead of 4–5:

| | One worker, back to back | One per second |
|---|---|---|
| Checks running at once | 1 | 1 |
| Tick cost while a check runs | the same | the same |
| Total hashing in a 50-player rush | about 4.25 s | about 4.25 s |
| Last player in a 50-player rush waits | about 4–5 s | about 50 s |
| Queue someone can fill with junk logins | drains at 11–12 per second | drains at 1 per second |

With one per second, three other things need care:

- The current 10-second login deadline would cut off everyone past about
  tenth place in the queue, so the deadline must not count queue time (rule
  28).
- Because unknown usernames also have to go through the queue (rule 27),
  junk logins take a full second of the queue each. That makes the queue
  easier to jam, and makes rule 29 essential.
- `MAX_CONNECTIONS` (100) becomes the queue's size limit, since every waiting
  player holds a connection.

A short pause between checks (for example 100 ms) would give the machine
some breathing room without making the queue much slower. It is worth
measuring before choosing.

### Measuring

31. **Compare averages, and repeat runs.** Averages repeat to within about
    10–20%; the single slowest tick doesn't repeat at all.
32. **Test on the machine the server will run on,** before trusting any of
    these limits for real.

## Limits of these results

- Each configuration was mostly run once. Averages were checked with a repeat
  and held up; small differences between runs should still be treated with
  caution.
- One machine only: the desktop that will host the server. The disk results
  are for its NVMe drive and its spinning drive specifically.
- The players were simulated on the same machine over the loopback network,
  so there was no real network delay or loss. The tests measure the server's
  cost of handling packets, not real internet conditions.
- The NPC update is synthetic: a little arithmetic per NPC, not real
  behaviour. Real NPC logic will cost more (rule 6).
- The voxel world is a worst case: every chunk has ground surface in it,
  every chunk gets random ticks, and edits land anywhere in the world at
  random instead of near players, so nearly every region changes all the
  time.
- Only one game-night run was made, with one game. A heavier game, or more
  going on at once, will cost more.

## Still to test

- **Raising the tick thread's priority,** if game-night spikes turn out to
  matter.
- **A minimum time between region saves** (rule 17), to confirm it cuts disk
  writing to a few MiB a second.
- **A fixed-size handoff between the tick and the saver** (rule 18).
- **Skipping chunks that can't change**, with a world that has realistic
  amounts of sky and solid rock.
- **Sending the saver a list of changed voxels** instead of whole chunks.
- **Reusing hashing memory between logins** (rule 30).

## Glossary

- **Thread:** a single line of work inside a program. A program can run
  several threads at once, for example one for the game world and one for
  the network.
- **Core / logical CPU:** a physical CPU core can run one thread at a time.
  With **hyperthreading**, each core shows up as two "logical CPUs" that
  share the core, so this 8-core CPU appears as 16.
- **Tick:** one update of the game world. At 20 ticks a second, each tick has
  50 ms.
- **Lateness (jitter):** how much later than planned a tick actually started.
- **Pinning (CPU affinity):** telling the operating system to always run a
  thread on one particular logical CPU, instead of letting it move the thread
  wherever there's room.
- **Load:** other work competing for the CPU at the same time.
- **Voxel:** one block of the world, like a pixel but in 3D.
- **Chunk:** a 32 × 32 × 32 cube of voxels, handled and saved as one piece.
- **Random tick:** checking a few randomly chosen voxels per chunk each tick
  and applying block rules to them, so slow changes like grass spreading
  happen across the world without checking every voxel.
- **Dirty chunk:** a chunk that has changed since it was last saved.
- **Region file:** one file on disk holding a group of chunks (64 here), so
  the disk handles a few large files instead of thousands of small ones.
- **Forcing onto the disk (sync):** making the operating system actually
  write data to the drive now, instead of holding it in memory for a while.
  It is what makes a save survive a power cut, and it is slow, especially on
  a spinning drive.
- **Argon2 / password hashing:** turning a password into a scrambled value
  that can be checked but not reversed. Argon2 is made deliberately slow and
  memory-hungry, so guessing passwords from a stolen file takes a very long
  time.
- **Cache / prefetching:** the CPU keeps a small amount of fast memory close
  by. When data is read in a predictable order, it loads the next piece
  ahead of time. When data is scattered, it has to wait for slower main
  memory more often.
- **UDP / loopback:** UDP is a simple way of sending network packets, common
  in games. Loopback means the packets never leave the computer.
- **Average vs. worst:** the average is the typical tick. The worst is the
  single slowest tick in the run, which can be far above the average.