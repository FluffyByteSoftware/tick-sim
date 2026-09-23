# tick-sim

A small, standalone benchmark that answers one question about game server
loops:

> Does a fixed-rate tick actually start on time under load, and does pinning
> the tick thread to a single CPU core make a measurable difference?

It runs a 20-ticks-per-second loop (50 ms per tick by default) on its own
thread. Each tick updates a subset of a large pile of fake world objects. It
records how late each tick started and how long its work took. You can run it
with the tick thread free to move between cores or pinned to one core, with or
without competing CPU load, and compare the numbers.

This was written as a throwaway measurement for a hobby RPG server project. It
is not a library and not a general-purpose profiler. It's shared in case the
approach or the numbers are useful to someone else making the same decision.

## What it does

1. **Builds a fake world.** It allocates `objects` structs of exactly 256 bytes
   each. Each struct holds a position, a velocity, and padding that stands in
   for everything else a real game object carries. Every byte is written up
   front, so the operating system commits the memory before timing starts.
2. **Picks NPCs.** Every `npc_every`-th object is an "NPC" that gets updated
   each tick (`position += velocity * dt`). With `scatter=1`, NPCs are visited
   in shuffled order, so the CPU's prefetcher can't predict the next one. This
   is closer to how scattered objects behave in a real world.
3. **Optionally adds load.** It can spawn `load` busy threads that compete with
   the tick thread for CPU time.
4. **Runs the tick loop.** Ticks are scheduled on absolute deadlines
   (`start + n × period`) rather than a fixed sleep after each tick, so errors
   don't accumulate. If a tick overruns so far that the next deadline has
   already passed, that deadline is counted as missed and skipped. There are no
   catch-up ticks.
5. **Reports.** After the run, it prints one summary block. The first 20 ticks
   are excluded from the stats as warm-up.

## Simulated players

With `players=N`, the program adds network traffic on top of the NPC work,
using plain UDP sockets over loopback (`127.0.0.1`) and no extra crates.

- **Clients.** One separate thread plays all N players, each with its own UDP
  socket. Every player sends an input packet `input_hz` times per second and
  reads any snapshots the server has sent it.
- **Server.** The server socket is non-blocking and lives on the tick thread.
  Each tick does three things in order:
    1. Drains every input packet that arrived since the last tick and applies
       it to that player (records the sequence number, nudges the position).
    2. Updates the NPCs, as without players.
    3. Sends each player one snapshot containing a tick number, the last input
       sequence it received from that player, the player's own position, and
       the positions of `snapshot_npcs` NPCs. Each player gets a different slice
       of the NPC list, so snapshot building reads from across the world.

Packet layouts (all numbers little-endian):

```
input    (client -> server): [player id u32][sequence u32][move x f32][padding to input_bytes]
snapshot (server -> client): [tick u32][last input sequence u32][own x,y,z f32][x,y,z f32 per NPC]
```

With the defaults, a snapshot is 1,220 bytes, which fits in a single UDP
datagram on a normal network.

Doing the networking inline on the tick thread is deliberately the worst case
for tick timing: every system call lands inside the tick's measured work. A
server that moves networking onto its own thread would see less of this cost
on the tick thread, and more contention between threads instead.

## Voxel world

With `voxel_chunks` above zero, the program also keeps a voxel world. It is
cut into chunks of 32 × 32 × 32 voxels. Each voxel is a 2-byte block id
(air, stone, dirt or grass), so one chunk is 32,768 voxels in 64 KiB, stored
as one flat array. The world is generated as rolling ground: stone, a few
layers of dirt, grass on top, air above.

Every tick, after the NPCs, the voxel world gets randomized upkeep:

- **Random ticks.** In every chunk, `random_ticks` voxels are picked at random
  and checked against simple rules: grass with a block on top of it turns to
  dirt, and dirt open to the sky next to grass turns to grass. Block games use
  this to make slow changes happen across the world without checking every
  voxel every tick. The default of 24 per chunk is the same rate as 3 per
  16 × 16 × 16 section.
- **Edits.** `voxel_edits` random voxels anywhere in the world are dug out or
  filled in with dirt, standing in for players digging and building.

A chunk that changes is marked dirty. The tick copies dirty chunks and
hands the copies to a separate saver thread through a queue, in one of two
ways:

- **All at once** (the default): every dirty chunk, every `save_every_ms`.
- **A little every tick** (`save_per_tick=N`): up to N dirty chunks per tick,
  carrying on from where the last tick stopped.

The saver compresses each copy with run-length encoding, as a save file
would. Without `disk_dir`, it then discards the result.

### Saving to disk

With `disk_dir` set, saves go to real files, using the same technique as the
game server's disk writer (`src/disk.rs` is a cut-down copy of it):

- Chunks are grouped into region files of `chunks_per_file` chunks. Files are
  only ever replaced whole, so the saver keeps a compressed copy of every
  chunk and rebuilds a region's whole file whenever any chunk in it changes.
- Each file goes into a cache that keeps only the newest copy per file, and
  the saver carries on straight away.
- A background writer thread takes the whole cache at once. It writes each
  file as `name.tmp` with `disk_threads` threads, forces each one onto the
  disk, renames them over the real names, and then forces the folder onto
  the disk once for the whole batch. A crash can never leave half a file.
- When the cache is full (`disk_max_files` or `disk_max_mib`), the saver
  waits for the writer. While it waits, chunk copies pile up in its queue.

The files go in a `tick-sim-save` folder inside `disk_dir`, which is deleted
at the end of the run unless `keep_files=1`. Which drive `disk_dir` is on
matters a great deal: a spinning drive and an SSD will give very different
results.

Every chunk gets random ticks, as if the whole world were loaded at once. A
real server would only do this near players, so treat `voxel_chunks` as the
number of chunks currently loaded.

| `voxel_chunks` | Voxels | Memory |
|---|---|---|
| 1,024 | 33.5 million | 64 MiB |
| 4,096 | 134 million | 256 MiB |
| 16,384 | 537 million | 1 GiB |

## Periodic background hashing

Game servers hash passwords when players log in, typically with Argon2id,
which is deliberately expensive. With `hash_every_ms` above zero, the program
simulates bursts of logins: every `hash_every_ms`, the hasher threads wake up,
compute `hash_burst` real Argon2id hashes between them (using the `argon2`
crate), and go back to sleep.

Argon2 is memory-hard. Each hash fills `hash_mem_kib` of RAM and reads it back
in an unpredictable order. That makes it a different kind of stress from the
`load` threads, which only do arithmetic: it competes with the tick thread for
CPU cache and memory bandwidth, the same resources the NPC update depends on.

The defaults (19 MiB, 2 passes, 1 lane) are a common Argon2id baseline. Set
them to whatever your real server uses.

Each tick is labelled by whether a hashing burst was running when it started
or finished, and the report shows the two groups separately. That makes it
easy to see whether logins disturb the ticks that happen during them.

## Requirements

- Rust (stable), installed via [rustup](https://rustup.rs)
- Linux is the tested platform. The `core_affinity` crate also supports
  Windows. On macOS, pinning isn't supported by the OS, and the output will
  report `FAILED` for pinned runs.
- Enough RAM for the object counts you choose. Each object is 256 bytes, so
  1 million objects is about 244 MiB and 10 million is about 2.4 GiB.

## Building and running

Always build in release mode. Debug builds of Rust are 10–50× slower, and the
timings would be meaningless.

```sh
git clone https://github.com/FluffyByteSoftware/tick-sim.git
cd tick-sim
cargo run --release
```

Settings are passed as `key=value` pairs, in any order:

```sh
cargo run --release -- objects=1000000 pin=3 load=16 seconds=30
```

| Setting     | Default | Meaning                                                    |
|-------------|---------|------------------------------------------------------------|
| `objects`   | 100000  | Number of 256-byte objects held in memory                  |
| `npc_every` | 10      | Every K-th object is an NPC updated each tick              |
| `scatter`   | 0       | `1` visits NPCs in shuffled order instead of memory order  |
| `pin`       | none    | Logical CPU number to pin the tick thread to, or `none`    |
| `period_ms` | 50      | Tick period in milliseconds                                |
| `seconds`   | 20      | How long to run                                            |
| `load`      | 0       | Number of busy background threads competing for the CPU    |
| `players`   | 0       | Number of simulated players exchanging UDP packets         |
| `input_hz`  | 30      | Input packets each player sends per second                 |
| `input_bytes` | 64    | Size of each input packet in bytes (12 to 1400)            |
| `snapshot_npcs` | 100 | NPC positions included in each per-player snapshot         |
| `hash_every_ms` | 0   | Run a burst of Argon2id hashes every this many ms (0 = off) |
| `hash_burst` | 1      | Hashes per burst, split across the hash threads            |
| `hash_threads` | 1    | Background threads doing the hashing                       |
| `hash_mem_kib` | 19456 | Argon2 memory per hash, in KiB                            |
| `hash_passes` | 2     | Argon2 passes over that memory (time cost)                 |
| `hash_lanes` | 1      | Argon2 lanes (parallelism parameter)                       |
| `voxel_chunks` | 0    | Voxel chunks of 32×32×32 voxels (64 KiB each); 0 = off     |
| `random_ticks` | 24   | Random voxel checks per chunk per tick                     |
| `voxel_edits` | 100   | Random voxels dug out or filled in per tick                |
| `save_every_ms` | 5000 | Copy *all* dirty chunks to the saver this often; 0 = never |
| `save_per_tick` | 0   | Instead, copy up to this many dirty chunks every tick      |
| `disk_dir` | none     | Write saves to real files in `disk_dir/tick-sim-save`      |
| `chunks_per_file` | 64 | Chunks per region file                                    |
| `disk_threads` | 8    | Threads the disk writer uses per batch                     |
| `disk_max_files` | 100 | Disk cache limit, in files                                |
| `disk_max_mib` | 2048 | Disk cache limit, in MiB                                   |
| `keep_files` | 0      | `1` leaves the save files on disk after the run            |

### Sweep script

`run.sh` builds once, then runs four object counts (100k, 1M, 4M, 10M), each
unpinned and pinned to CPU 3, for 15 seconds apiece. Any extra settings you
give it are passed through to every run:

```sh
chmod +x run.sh
./run.sh                      # idle machine
./run.sh load=16              # every logical CPU busy
./run.sh load=16 scatter=1    # busy, with shuffled NPC access
./run.sh players=50           # 50 simulated players, idle machine
./run.sh players=50 load=16   # 50 simulated players, every CPU busy
./run.sh players=50 hash_every_ms=2000 hash_burst=8 hash_threads=4
                              # 50 players plus a burst of 8 logins every 2 s
```

Edit the CPU number and the object counts in `run.sh` to suit your machine.
Drop the 10M count if you're short on RAM.

## Choosing a CPU to pin to

Run `lscpu -e` and look at the `CORE` column. Logical CPUs that share a `CORE`
number are hyperthread siblings on the same physical core. For example, on an
8-core/16-thread Intel chip, CPU 3 and CPU 11 are often siblings. If the
sibling of your pinned CPU is busy, you're partly measuring hyperthreading
contention rather than pinning itself, so keep that in mind when reading
results.

On chips with a mix of performance and efficiency cores, the `MAXMHZ` column
tells them apart. Pin to a performance core.

## Reading the output

Each run prints a block like this (values shown as placeholders):

```
=== objects 1000000 (244 MiB) | NPCs 100000 (in order) | pin CPU 3 (ok) | load 16 | players 50 | 50 ms x 15 s ===
ticks N  missed deadlines N  work > period N  worst tick used X% of budget
wake late  min ...  avg ...  p50 ...  p99 ...  max ...  (us)
work time  min ...  avg ...  p50 ...  p99 ...  max ...  (ms)
  npc part min ...  avg ...  p50 ...  p99 ...  max ...  (ms)
  net part min ...  avg ...  p50 ...  p99 ...  max ...  (ms)
packets    inputs sent N recv N (send failed N) | snapshots sent N recv N (dropped N) | errors N
```

The `npc part`, `net part` and `packets` lines only appear when `players` is
above zero. With `voxel_chunks` set, these lines are added:

```
  voxels   min ...  avg ...  p50 ...  p99 ...  max ...  (ms)
  saving   min ...  avg ...  p50 ...  p99 ...  max ...  (ms)
voxels     4096 chunks = 134.2M voxels (256 MiB) | random ticks 24/chunk | edits 100/tick | changes N/tick | chunks saved N
saver      compressed N chunks, N MiB -> N MiB, busy N ms in total
```

The `saving` row only counts ticks on which a save happened. With
`disk_dir` set, three more lines describe the disk:

```
disk       region files of 64 chunks | N files handed to the writer, N replaced a copy still waiting
writer     N batches, N files written (0 failed), N MiB in N s busy (N MiB/s) | biggest batch N files | longest N ms
cache full saver waited N times, N s in total | the writer's last batches took N s
```

With any saving on, the `saver` line is followed by one more:

```
saver      finished N s after the last tick (how far behind it was)
```

- **most waiting for saver** (on the `voxels` line) is the largest number of
  chunk copies ever queued for the saver at once. If it keeps growing with
  run length, the saving pipeline can't keep up.
- **cache full** shows how often, and for how long, the disk writer's cache
  was full.
- **saver finished N s after the last tick** is how long the saver and disk
  writer needed to finish everything still queued when the run ended: how
  far behind saving had fallen. Near zero means it kept up. With `hash_every_ms` set, five more lines follow:

```
hashing    every 2000 ms: 8 hash(es) on 4 thread(s), Argon2id 19456 KiB x 2 passes x 1 lane(s)
ticks      during hashing N (over budget N)  quiet N  hashes done N
late quiet min ...  avg ...  p50 ...  p99 ...  max ...  (us)
late hash  min ...  avg ...  p50 ...  p99 ...  max ...  (us)
work quiet min ...  avg ...  p50 ...  p99 ...  max ...  (ms)
work hash  min ...  avg ...  p50 ...  p99 ...  max ...  (ms)
hash time  min ...  avg ...  p50 ...  p99 ...  max ...  (ms)
```

- **wake late** is how long after its scheduled start each tick actually
  began, in microseconds. This is scheduler jitter, and it's the number pinning
  might change. Expect a floor of roughly 50–100 µs even on an idle Linux
  machine, because the kernel deliberately lets ordinary threads' sleeps run
  slightly long ("timer slack") to batch wake-ups.
- **work time** is how long the NPC update took, in milliseconds. This grows
  with the NPC count and with how scattered the NPCs are in memory.
- **missed deadlines** counts ticks that finished after the next tick should
  already have started. The object count at which this stops being zero is
  where the loop breaks.
- **voxels** is the time spent on edits and random ticks each tick. **saving**
  is the time the tick spent copying dirty chunks for the saver, on the ticks
  that saved. Watch its max: every chunk changed since the last save is
  copied in one go.
- **npc part** and **net part** split the work time into NPC updating and
  networking (receiving inputs plus building and sending snapshots).
- **packets** compares what was sent with what arrived. A small shortfall at
  the end is normal, since clients keep sending briefly after the last tick.
  A large gap in inputs means packets were lost because the server fell
  behind draining its socket. `dropped` counts snapshots the server couldn't
  send because its send buffer was full.
- **late hash / work hash** versus **late quiet / work quiet** compare ticks
  that overlapped a hashing burst with ticks that didn't. **hash time** is how
  long each individual hash took, which tells you how long a player waits at
  login.
- **worst tick used X% of budget** shows how close the slowest tick came to
  the full period.
- **p99** (the 99th percentile) is usually more informative than **max**. A
  single freak spike shouldn't decide anything on its own.

## Caveats

- **Pinning does not reserve a core.** It stops the tick thread from moving
  between cores, but other threads and processes can still run on that core.
  Under heavy load, an unpinned thread can move to whichever core frees up
  first, while a pinned one has to wait its turn. Pinning coming out equal or
  worse is a legitimate result. Truly reserving a core needs kernel-level
  isolation (`isolcpus`) or real-time scheduling (`SCHED_FIFO`), which this
  tool doesn't test.
- **The built-in load is pure arithmetic.** The `load` threads compete for CPU
  time but don't pressure memory bandwidth. For memory pressure, run a tool
  like `stress-ng --vm` alongside instead.
- **CPU frequency scaling affects results.** The tick thread sleeps most of
  each period, and the CPU may clock down in between. That shows up in work
  time. Your power profile or governor setting matters, so note it alongside
  your results.
- **Loopback is not a real network.** Packets never leave the machine, so
  there is no real latency, packet loss, or bandwidth limit. What is measured
  is the CPU cost of the socket system calls and the packet handling. The
  simulated clients also run on the same machine, so their CPU use competes
  with the server's in a way real remote players' wouldn't.
- **The workload is synthetic.** A position update over 256-byte structs is a
  stand-in for real game logic, not a model of it. Use the numbers to compare
  configurations against each other, not to predict a real server's capacity.

## Author

Jacob Chacko

## License

MIT. See [LICENSE](LICENSE).