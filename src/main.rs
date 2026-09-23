// tick-sim -- throwaway benchmark for Stratum
// Question: does a fixed-rate tick land on time under load, and does
// pinning the tick thread to one CPU core make a measurable difference?
// Author: Jacob Chacko
//
// ALWAYS build with --release (debug builds are 10-50x slower):
//   cargo run --release -- objects=1000000 pin=3 load=16 players=50
//
// Settings (all optional, key=value, any order):
//   objects=N        fake objects held in memory, 256 bytes each    (default 100000)
//   npc_every=K      every K-th object is an NPC, updated each tick  (default 10)
//   scatter=1        visit NPCs in shuffled order instead of front-to-back
//   pin=C|none       pin the tick thread to logical CPU C           (default none)
//   period_ms=P      tick period in milliseconds                     (default 50)
//   seconds=S        how long to run                                 (default 20)
//   load=T           spawn T busy threads that compete for the CPU  (default 0)
//   players=N        simulated players talking UDP over loopback     (default 0)
//   input_hz=H       input packets each player sends per second      (default 30)
//   input_bytes=B    size of each input packet, 12 to 1400           (default 64)
//   snapshot_npcs=M  NPC positions in each per-player snapshot       (default 100)
//   hash_every_ms=T  run a burst of Argon2id hashes every T ms, 0=off (default 0)
//   hash_burst=K     hashes per burst, split across the hash threads  (default 1)
//   hash_threads=W   background threads doing the hashing            (default 1)
//   hash_mem_kib=M   Argon2 memory per hash in KiB                   (default 19456)
//   hash_passes=P    Argon2 passes over that memory (time cost)      (default 2)
//   hash_lanes=L     Argon2 lanes (parallelism parameter)            (default 1)
//   voxel_chunks=C   voxel chunks of 32x32x32 (64 KiB each), 0=off  (default 0)
//   random_ticks=R   random voxel checks per chunk per tick         (default 24)
//   voxel_edits=E    random voxels dug or filled per tick           (default 100)
//   save_every_ms=T  copy dirty chunks to the saver thread, 0=off   (default 5000)

use std::hint::black_box;
use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use argon2::{Algorithm, Argon2, Params, Version};

/// Ticks ignored in the stats while caches and CPU clocks settle.
const WARMUP_TICKS: usize = 20;

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

// `#[derive(Clone, Copy)]` makes Config copyable by value, like a plain
// C struct. Without it, handing it to another thread would *move* it.
#[derive(Clone, Copy)]
struct Config {
    objects: usize,
    npc_every: usize,
    scatter: bool,
    pin: Option<usize>, // Option = "maybe a value": Some(3) or None
    period_ms: u64,
    seconds: u64,
    load: usize,
    players: usize,
    input_hz: u32,
    input_bytes: usize,
    snapshot_npcs: usize,
    hash_every_ms: u64,
    hash_burst: usize,
    hash_threads: usize,
    hash_mem_kib: u32,
    hash_passes: u32,
    hash_lanes: u32,
    voxel_chunks: usize,
    random_ticks: usize,
    voxel_edits: usize,
    save_every_ms: u64,
}

fn parse_args() -> Config {
    let mut cfg = Config {
        objects: 100_000, // underscores in numbers are just for readability
        npc_every: 10,
        scatter: false,
        pin: None,
        period_ms: 50,
        seconds: 20,
        load: 0,
        players: 0,
        input_hz: 30,
        input_bytes: 64,
        snapshot_npcs: 100,
        hash_every_ms: 0,
        hash_burst: 1,
        hash_threads: 1,
        hash_mem_kib: 19_456, // 19 MiB, t=2, 1 lane: the common Argon2id baseline
        hash_passes: 2,
        hash_lanes: 1,
        voxel_chunks: 0,
        random_ticks: 24, // 3 per 16x16x16 section, a common block-game rate
        voxel_edits: 100,
        save_every_ms: 5000,
    };

    // args() yields the program name first, so skip(1) drops it.
    for arg in std::env::args().skip(1) {
        let (key, value) = match arg.split_once('=') {
            Some(pair) => pair,
            None => {
                eprintln!("ignoring '{arg}' (expected key=value)");
                continue;
            }
        };
        match key {
            "objects" => cfg.objects = parse_num(key, value),
            "npc_every" => cfg.npc_every = parse_num(key, value),
            "scatter" => cfg.scatter = value == "1" || value == "true",
            "pin" => {
                cfg.pin = if value == "none" {
                    None
                } else {
                    Some(parse_num(key, value))
                }
            }
            "period_ms" => cfg.period_ms = parse_num(key, value),
            "seconds" => cfg.seconds = parse_num(key, value),
            "load" => cfg.load = parse_num(key, value),
            "players" => cfg.players = parse_num(key, value),
            "input_hz" => cfg.input_hz = parse_num(key, value),
            "input_bytes" => cfg.input_bytes = parse_num(key, value),
            "snapshot_npcs" => cfg.snapshot_npcs = parse_num(key, value),
            "hash_every_ms" => cfg.hash_every_ms = parse_num(key, value),
            "hash_burst" => cfg.hash_burst = parse_num(key, value),
            "hash_threads" => cfg.hash_threads = parse_num(key, value),
            "hash_mem_kib" => cfg.hash_mem_kib = parse_num(key, value),
            "hash_passes" => cfg.hash_passes = parse_num(key, value),
            "hash_lanes" => cfg.hash_lanes = parse_num(key, value),
            "voxel_chunks" => cfg.voxel_chunks = parse_num(key, value),
            "random_ticks" => cfg.random_ticks = parse_num(key, value),
            "voxel_edits" => cfg.voxel_edits = parse_num(key, value),
            "save_every_ms" => cfg.save_every_ms = parse_num(key, value),
            _ => eprintln!("ignoring unknown setting '{key}'"),
        }
    }
    assert!(cfg.npc_every >= 1, "npc_every must be at least 1");
    assert!(cfg.input_hz >= 1, "input_hz must be at least 1");
    assert!(
        (12..=1400).contains(&cfg.input_bytes),
        "input_bytes must be between 12 and 1400"
    );
    if cfg.hash_every_ms > 0 {
        assert!(cfg.hash_threads >= 1, "hash_threads must be at least 1");
        assert!(cfg.hash_burst >= 1, "hash_burst must be at least 1");
    }
    cfg
}

// Generic helper: T is whatever number type the destination field is
// (usize, u64, ...). The compiler figures T out from where the result goes.
fn parse_num<T: std::str::FromStr>(key: &str, value: &str) -> T {
    match value.parse() {
        Ok(n) => n,
        Err(_) => panic!("bad number for {key}: '{value}'"),
    }
}

// ---------------------------------------------------------------------------
// The fake world
// ---------------------------------------------------------------------------

// One fake object: exactly 256 bytes. Position + velocity (24 bytes) are the
// data we actually touch; the padding stands in for everything else a real
// object would carry, so objects are spread across memory like real ones
// and the cache can't hold them all.
struct Object {
    position: [f32; 3],
    velocity: [f32; 3],
    #[allow(dead_code)] // never read on purpose; silences the compiler warning
    padding: [u8; 232],
}

// Compile-time size check, like a C static_assert.
const _: () = assert!(std::mem::size_of::<Object>() == 256);

fn build_world(count: usize) -> Vec<Object> {
    // (0..count) is a range; .map() turns each index into an Object;
    // .collect() gathers them into a Vec. Roughly a C for-loop filling an array.
    // Writing every byte (padding included) forces Linux to hand over the
    // memory now, instead of lazily on first touch in the middle of a tick.
    (0..count)
        .map(|i| {
            let f = i as f32;
            Object {
                position: [f, 0.0, f * 0.5],
                velocity: [1.0, 0.25, -0.5],
                padding: [(i % 251) as u8; 232],
            }
        })
        .collect()
}

/// Indices of the NPCs: every `every`-th object. With `scatter`, the visit
/// order is shuffled so the CPU's prefetcher can't predict the next one.
fn pick_npcs(count: usize, every: usize, scatter: bool) -> Vec<usize> {
    let mut npcs: Vec<usize> = (0..count).step_by(every).collect();
    if scatter {
        // Fisher-Yates shuffle with a tiny xorshift random generator,
        // so we don't need the `rand` crate for one shuffle.
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        for i in (1..npcs.len()).rev() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let j = (seed % (i as u64 + 1)) as usize;
            npcs.swap(i, j);
        }
    }
    npcs
}

/// The per-tick work: move each NPC by velocity * dt.
/// `&mut [Object]` is a mutable view into the Vec (like pointer + length in C);
/// `&[usize]` is a read-only view.
fn update_npcs(world: &mut [Object], npcs: &[usize], dt: f32) {
    for &i in npcs {
        let obj = &mut world[i];
        for axis in 0..3 {
            obj.position[axis] += obj.velocity[axis] * dt;
        }
        // Bounce so the numbers stay sane over long runs.
        if obj.position[0].abs() > 1.0e6 {
            obj.velocity[0] = -obj.velocity[0];
        }
    }
}

// ---------------------------------------------------------------------------
// The voxel world
// ---------------------------------------------------------------------------
//
// The world is cut into chunks of 32 x 32 x 32 voxels. Each voxel is a u16
// block id, so a chunk is 32,768 voxels = 64 KiB, kept as one flat array.
// Every tick does two kinds of randomized upkeep:
//
//   - Random ticks: in every chunk, `random_ticks` voxels are picked at
//     random and checked against simple block rules (grass spreading onto
//     dirt, grass dying under a block). This is how block games make slow
//     world changes happen without checking every voxel every tick.
//   - Edits: `voxel_edits` voxels anywhere in the world get dug out or
//     filled in, standing in for players digging and building.
//
// A chunk that changes is marked dirty. Every `save_every_ms`, the tick
// copies each dirty chunk and hands the copies to a saver thread, which
// compresses them the way a save file would (and then throws the result
// away: nothing is written to disk).
//
// Every chunk gets random ticks, as if the whole world were loaded. A real
// server would only do this for chunks near players, so voxel_chunks here
// means "chunks loaded right now".

const CHUNK_SIDE: usize = 32;
const CHUNK_VOXELS: usize = CHUNK_SIDE * CHUNK_SIDE * CHUNK_SIDE; // 32,768

// Block ids. `const` values can be used as patterns in a `match`.
const AIR: u16 = 0;
const STONE: u16 = 1;
const DIRT: u16 = 2;
const GRASS: u16 = 3;

/// Where voxel (x, y, z) sits in its chunk's flat array. y changes slowest,
/// so each horizontal layer is one unbroken run of 32 x 32 = 1,024 voxels.
fn voxel_index(x: usize, y: usize, z: usize) -> usize {
    (y * CHUNK_SIDE + z) * CHUNK_SIDE + x
}

/// One of the four sideways neighbours of (x, z), or None if it falls
/// outside this chunk. (Grass doesn't spread across chunk edges here; a real
/// server would look into the next chunk.)
fn neighbour(x: usize, z: usize, side: usize) -> Option<(usize, usize)> {
    match side {
        0 if x + 1 < CHUNK_SIDE => Some((x + 1, z)),
        1 if x > 0 => Some((x - 1, z)),
        2 if z + 1 < CHUNK_SIDE => Some((x, z + 1)),
        3 if z > 0 => Some((x, z - 1)),
        _ => None,
    }
}

/// A small, fast random number generator (xorshift, the same idea as the
/// NPC shuffle). Fine for picking voxels; not for anything secret.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A random number from 0 up to, but not including, `n`.
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

struct Chunk {
    blocks: Vec<u16>, // always CHUNK_VOXELS long
    dirty: bool,      // changed since it was last saved
}

/// A chunk of rolling ground: stone, three layers of dirt, grass on top,
/// air above. The surface height varies a little from column to column.
/// Every voxel is written, so Linux hands over the memory now rather than
/// in the middle of a tick.
fn build_chunk(rng: &mut Rng) -> Chunk {
    let mut blocks = vec![AIR; CHUNK_VOXELS];
    for z in 0..CHUNK_SIDE {
        for x in 0..CHUNK_SIDE {
            let surface = 12 + rng.below(8); // somewhere from 12 to 19
            for y in 0..CHUNK_SIDE {
                blocks[voxel_index(x, y, z)] = if y + 3 < surface {
                    STONE
                } else if y < surface {
                    DIRT
                } else if y == surface {
                    GRASS
                } else {
                    AIR
                };
            }
        }
    }
    Chunk { blocks, dirty: false }
}

#[derive(Default, Clone, Copy)]
struct VoxelStats {
    changes: u64,      // voxels that actually changed (edits + rules)
    chunks_saved: u64, // dirty chunks copied to the saver
}

struct VoxelWorld {
    chunks: Vec<Chunk>,
    rng: Rng,
    random_ticks: usize,
    edits: usize,
    // mpsc = "multiple producer, single consumer": a queue between threads.
    // The Sender end puts things in; the saver thread takes them out.
    saver: Option<mpsc::Sender<Vec<u16>>>,
    stats: VoxelStats,
}

impl VoxelWorld {
    fn new(cfg: &Config, saver: Option<mpsc::Sender<Vec<u16>>>) -> VoxelWorld {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15); // any non-zero start works
        let chunks = (0..cfg.voxel_chunks).map(|_| build_chunk(&mut rng)).collect();
        VoxelWorld {
            chunks,
            rng,
            random_ticks: cfg.random_ticks,
            edits: cfg.voxel_edits,
            saver,
            stats: VoxelStats::default(),
        }
    }

    fn voxel_count(&self) -> usize {
        self.chunks.len() * CHUNK_VOXELS
    }

    /// One tick of upkeep: the edits, then random ticks in every chunk.
    fn tick(&mut self) {
        for _ in 0..self.edits {
            self.edit();
        }
        for c in 0..self.chunks.len() {
            for _ in 0..self.random_ticks {
                self.random_tick(c);
            }
        }
    }

    /// Digs out or fills in one random voxel anywhere in the world.
    fn edit(&mut self) {
        let c = self.rng.below(self.chunks.len());
        let i = self.rng.below(CHUNK_VOXELS);
        let chunk = &mut self.chunks[c];
        chunk.blocks[i] = if chunk.blocks[i] == AIR { DIRT } else { AIR };
        chunk.dirty = true;
        self.stats.changes += 1;
    }

    /// Checks one random voxel in chunk `c` against the block rules.
    fn random_tick(&mut self, c: usize) {
        // Pick everything random first, then look at the chunk.
        let x = self.rng.below(CHUNK_SIDE);
        let y = self.rng.below(CHUNK_SIDE - 1); // not the top layer, so "above" exists
        let z = self.rng.below(CHUNK_SIDE);
        let side = self.rng.below(4);

        let chunk = &mut self.chunks[c];
        let here = voxel_index(x, y, z);
        let above = chunk.blocks[voxel_index(x, y + 1, z)];

        let new_block = match chunk.blocks[here] {
            // Grass with something on top of it dies back to dirt.
            GRASS if above != AIR => Some(DIRT),
            // Dirt open to the sky catches grass from a grassy neighbour.
            DIRT if above == AIR => match neighbour(x, z, side) {
                Some((nx, nz)) if chunk.blocks[voxel_index(nx, y, nz)] == GRASS => Some(GRASS),
                _ => None,
            },
            _ => None,
        };

        if let Some(block) = new_block {
            chunk.blocks[here] = block;
            chunk.dirty = true;
            self.stats.changes += 1;
        }
    }

    /// Copies every dirty chunk into the saver's queue and marks it clean.
    fn save_dirty(&mut self) {
        let saver = match &self.saver {
            Some(saver) => saver,
            None => return,
        };
        for chunk in self.chunks.iter_mut() {
            if chunk.dirty {
                // clone() copies all 64 KiB, so the saver works on a snapshot
                // while the tick carries on changing the real chunk.
                if saver.send(chunk.blocks.clone()).is_ok() {
                    self.stats.chunks_saved += 1;
                }
                chunk.dirty = false;
            }
        }
    }
}

#[derive(Default, Clone, Copy)]
struct SaverStats {
    chunks: u64,
    bytes_in: u64,
    bytes_out: u64,
    busy: Duration,
}

/// Starts the saver thread. It compresses each chunk copy it's sent, and
/// stops by itself once the tick thread is done: when every Sender is gone,
/// the `for` loop over the Receiver simply ends.
fn start_saver() -> (mpsc::Sender<Vec<u16>>, thread::JoinHandle<SaverStats>) {
    let (sender, receiver) = mpsc::channel::<Vec<u16>>();
    let handle = thread::spawn(move || {
        let mut stats = SaverStats::default();
        for blocks in receiver {
            let t = Instant::now();
            let packed = compress(&blocks);
            stats.busy += t.elapsed();
            stats.chunks += 1;
            stats.bytes_in += (blocks.len() * 2) as u64;
            stats.bytes_out += packed.len() as u64;
            black_box(&packed);
        }
        stats
    });
    (sender, handle)
}

/// Run-length encoding: each run of identical blocks becomes a pair of
/// numbers, (how many u16, which block u16). Terrain is mostly long runs of
/// stone and air, so it shrinks a lot.
fn compress(blocks: &[u16]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < blocks.len() {
        let block = blocks[i];
        let mut run = 1;
        while i + run < blocks.len() && blocks[i + run] == block && run < u16::MAX as usize {
            run += 1;
        }
        out.extend_from_slice(&(run as u16).to_le_bytes());
        out.extend_from_slice(&block.to_le_bytes());
        i += run;
    }
    out
}

// ---------------------------------------------------------------------------
// Simulated network: server side (runs inside the tick thread)
// ---------------------------------------------------------------------------
//
// Everything is UDP over loopback (127.0.0.1). Packet layouts, all numbers
// little-endian:
//
//   input    (client -> server, input_bytes long):
//            [player id u32][sequence u32][move x f32][padding...]
//
//   snapshot (server -> each client, once per tick):
//            [tick u32][last input sequence seen u32]
//            [player's own x,y,z f32][x,y,z f32 for each of snapshot_npcs NPCs]

struct Player {
    addr: Option<SocketAddr>, // learned from the first packet this player sends
    position: [f32; 3],
    last_seq: u32, // echoed back in snapshots as an acknowledgement
}

// `Default` lets us write ServerStats::default() to get all zeros.
#[derive(Default, Clone, Copy)]
struct ServerStats {
    inputs_received: u64,
    snapshots_sent: u64,
    snapshots_dropped: u64, // the socket's send buffer was full
    errors: u64,
}

struct ServerNet {
    socket: UdpSocket,
    players: Vec<Player>,
    snapshot_npcs: usize,
    recv_buf: Vec<u8>,
    snapshot: Vec<u8>, // reused every send, so no allocation per packet
    stats: ServerStats,
}

// `impl` attaches functions to a struct, like methods on a C# class.
// `&self` / `&mut self` is the object the method was called on (C#'s `this`).
impl ServerNet {
    fn new(players: usize, snapshot_npcs: usize) -> ServerNet {
        // Port 0 means "let the OS pick any free port".
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind server socket");
        // Non-blocking: recv returns a WouldBlock error instead of waiting
        // when nothing has arrived. The tick thread must never wait on the network.
        socket.set_nonblocking(true).expect("set server non-blocking");

        let players = (0..players)
            .map(|_| Player {
                addr: None,
                position: [0.0; 3],
                last_seq: 0,
            })
            .collect();

        ServerNet {
            socket,
            players,
            snapshot_npcs,
            recv_buf: vec![0u8; 2048],
            snapshot: Vec::with_capacity(20 + snapshot_npcs * 12),
            stats: ServerStats::default(),
        }
    }

    fn address(&self) -> SocketAddr {
        self.socket.local_addr().expect("server address")
    }

    /// Drain every input packet that arrived since the last tick and apply it.
    fn receive_inputs(&mut self) {
        loop {
            match self.socket.recv_from(&mut self.recv_buf) {
                Ok((len, from)) => {
                    if len < 12 {
                        self.stats.errors += 1;
                        continue;
                    }
                    let id = read_u32(&self.recv_buf, 0) as usize;
                    let seq = read_u32(&self.recv_buf, 4);
                    let move_x = read_f32(&self.recv_buf, 8);

                    // get_mut returns None for an out-of-range id instead of
                    // crashing, so a bad packet can't take the server down.
                    match self.players.get_mut(id) {
                        Some(player) => {
                            player.addr = Some(from);
                            player.last_seq = seq;
                            player.position[0] += move_x;
                            self.stats.inputs_received += 1;
                        }
                        None => self.stats.errors += 1,
                    }
                }
                // Nothing left to read this tick: done.
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.stats.errors += 1;
                    break;
                }
            }
        }
    }

    /// Send each player a snapshot: their own position plus a slice of NPCs.
    fn send_snapshots(&mut self, tick: u32, world: &[Object], npcs: &[usize]) {
        for (p, player) in self.players.iter().enumerate() {
            // Skip players we haven't heard from yet (no address to send to).
            let addr = match player.addr {
                Some(a) => a,
                None => continue,
            };

            self.snapshot.clear();
            self.snapshot.extend_from_slice(&tick.to_le_bytes());
            self.snapshot.extend_from_slice(&player.last_seq.to_le_bytes());
            for axis in 0..3 {
                self.snapshot
                    .extend_from_slice(&player.position[axis].to_le_bytes());
            }
            if !npcs.is_empty() {
                for k in 0..self.snapshot_npcs {
                    // Each player "sees" a different slice of the NPC list,
                    // so snapshots read from different parts of memory.
                    let idx = npcs[(p * self.snapshot_npcs + k) % npcs.len()];
                    for axis in 0..3 {
                        self.snapshot
                            .extend_from_slice(&world[idx].position[axis].to_le_bytes());
                    }
                }
            }

            match self.socket.send_to(&self.snapshot, addr) {
                Ok(_) => self.stats.snapshots_sent += 1,
                Err(e) if e.kind() == ErrorKind::WouldBlock => self.stats.snapshots_dropped += 1,
                Err(_) => self.stats.errors += 1,
            }
        }
    }
}

// Read a little-endian number out of a byte buffer at a given offset.
// try_into() turns the 4-byte slice into a fixed [u8; 4] array. It can only
// fail if the slice isn't exactly 4 long, which can't happen here, so unwrap().
fn read_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(buf[at..at + 4].try_into().unwrap())
}

fn read_f32(buf: &[u8], at: usize) -> f32 {
    f32::from_le_bytes(buf[at..at + 4].try_into().unwrap())
}

// ---------------------------------------------------------------------------
// Simulated network: client side (one thread plays every player)
// ---------------------------------------------------------------------------

#[derive(Default, Clone, Copy)]
struct ClientStats {
    inputs_sent: u64,
    inputs_failed: u64,
    snapshots_received: u64,
}

/// One thread, one UDP socket per player. Each round, every player sends an
/// input packet and reads whatever snapshots have arrived for it.
fn start_clients(
    cfg: Config,
    server: SocketAddr,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<ClientStats> {
    thread::spawn(move || {
        let mut stats = ClientStats::default();

        let sockets: Vec<UdpSocket> = (0..cfg.players)
            .map(|_| {
                let s = UdpSocket::bind("127.0.0.1:0").expect("bind client socket");
                // connect() on UDP just fixes the destination, so send() needs no address.
                s.connect(server).expect("connect client socket");
                s.set_nonblocking(true).expect("set client non-blocking");
                s
            })
            .collect();

        let interval = Duration::from_secs_f64(1.0 / cfg.input_hz as f64);
        let mut packet = vec![0u8; cfg.input_bytes];
        let mut recv_buf = vec![0u8; 65536];
        let mut seq: u32 = 0;
        let mut next = Instant::now();

        while !stop.load(Ordering::Relaxed) {
            // Wiggle left and right so player positions keep changing.
            let move_x: f32 = if seq % 2 == 0 { 0.1 } else { -0.1 };

            for (id, socket) in sockets.iter().enumerate() {
                packet[0..4].copy_from_slice(&(id as u32).to_le_bytes());
                packet[4..8].copy_from_slice(&seq.to_le_bytes());
                packet[8..12].copy_from_slice(&move_x.to_le_bytes());
                match socket.send(&packet) {
                    Ok(_) => stats.inputs_sent += 1,
                    Err(_) => stats.inputs_failed += 1,
                }
                // Read (and discard) every snapshot waiting for this player.
                while socket.recv(&mut recv_buf).is_ok() {
                    stats.snapshots_received += 1;
                }
            }

            seq = seq.wrapping_add(1);
            next += interval;
            let now = Instant::now();
            if next > now {
                thread::sleep(next - now);
            } else {
                next = now; // fell behind: carry on, don't send a burst
            }
        }
        stats
    })
}

// ---------------------------------------------------------------------------
// Periodic background password hashing (Argon2id)
// ---------------------------------------------------------------------------
//
// Stands in for login bursts: every hash_every_ms, the hasher threads wake up
// and compute hash_burst real Argon2id hashes between them, then go back to
// sleep. Argon2 is "memory-hard": each hash fills hash_mem_kib of RAM and
// reads it back in an unpredictable order. So unlike the `load` threads
// (pure arithmetic), it competes with the tick thread for CPU cache and
// memory bandwidth, the same resources the NPC update depends on.

/// Starts the hasher threads (none if hashing is off). `active` counts how
/// many are in the middle of a burst right now, so the tick thread can tell
/// whether hashing overlapped a tick. Each thread returns how long each of
/// its hashes took.
fn start_hashers(
    cfg: Config,
    active: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
) -> Vec<thread::JoinHandle<Vec<Duration>>> {
    if cfg.hash_every_ms == 0 {
        return Vec::new();
    }
    (0..cfg.hash_threads)
        .map(|w| {
            let active = Arc::clone(&active);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                // Some(32) = a 32-byte hash output. expect() stops the program
                // with a message if the memory/passes/lanes combination is invalid.
                let params = Params::new(cfg.hash_mem_kib, cfg.hash_passes, cfg.hash_lanes, Some(32))
                    .expect("invalid argon2 settings (hash_mem_kib / hash_passes / hash_lanes)");
                let hasher = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

                // Split the burst across threads: 10 hashes on 4 threads = 3,3,2,2.
                let mut share = cfg.hash_burst / cfg.hash_threads;
                if w < cfg.hash_burst % cfg.hash_threads {
                    share += 1;
                }

                let every = Duration::from_millis(cfg.hash_every_ms);
                let salt = b"tick-sim-salt-16"; // 16 bytes; a fixed salt is fine for a benchmark
                let mut out = [0u8; 32];
                let mut times = Vec::new();
                let mut counter: u64 = 0;
                let mut next = Instant::now() + every;

                loop {
                    // Wait for the next burst in naps of at most 100 ms, so a
                    // stop request is noticed quickly even if bursts are far apart.
                    loop {
                        if stop.load(Ordering::Relaxed) {
                            return times;
                        }
                        let now = Instant::now();
                        if now >= next {
                            break;
                        }
                        thread::sleep((next - now).min(Duration::from_millis(100)));
                    }

                    active.fetch_add(1, Ordering::Relaxed);
                    for _ in 0..share {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        counter += 1;
                        let password = counter.to_le_bytes(); // a different "password" each time
                        let t = Instant::now();
                        hasher
                            .hash_password_into(&password, salt, &mut out)
                            .expect("argon2 hash failed");
                        times.push(t.elapsed());
                        black_box(&out);
                    }
                    active.fetch_sub(1, Ordering::Relaxed);

                    // Schedule the next burst. If this burst ran past it, skip ahead.
                    next += every;
                    let now = Instant::now();
                    while next <= now {
                        next += every;
                    }
                }
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The tick loop
// ---------------------------------------------------------------------------

struct Sample {
    late: Duration, // how long after its deadline the tick actually started
    work: Duration,  // total time the tick took (all the parts below)
    npc: Duration,   // NPC update part
    net: Duration,   // receiving inputs + sending snapshots
    voxel: Duration, // voxel edits + random ticks
    save: Duration,  // copying dirty chunks to the saver
    saved: bool,     // a save happened on this tick
    hashing: bool,   // a hashing burst was running at the tick's start or end
}

/// Runs ticks on absolute deadlines: start + period, start + 2*period, ...
/// Returns every tick's sample plus how many deadlines were missed outright.
fn tick_loop(
    cfg: &Config,
    world: &mut [Object],
    npcs: &[usize],
    net: &mut Option<ServerNet>,
    voxels: &mut Option<VoxelWorld>,
    hashing: &AtomicUsize,
) -> (Vec<Sample>, u64) {
    let period = Duration::from_millis(cfg.period_ms);
    let dt = period.as_secs_f32();
    let run_for = Duration::from_secs(cfg.seconds);

    // Reserve all the space up front so the Vec never reallocates mid-run.
    let expected = (run_for.as_millis() / period.as_millis()) as usize + 16;
    let mut samples = Vec::with_capacity(expected);
    let mut missed: u64 = 0;
    let mut tick: u32 = 0;

    // Save every N ticks. 0 means never.
    let save_every_ticks: u32 = if cfg.save_every_ms == 0 {
        0
    } else {
        (cfg.save_every_ms / cfg.period_ms).max(1) as u32
    };

    let start = Instant::now();
    let mut deadline = start + period;

    while deadline - start < run_for {
        // Sleep until the deadline (if it hasn't already passed).
        let now = Instant::now();
        if now < deadline {
            thread::sleep(deadline - now);
        }
        let woke = Instant::now();
        let late = woke.saturating_duration_since(deadline);
        let hashing_at_start = hashing.load(Ordering::Relaxed) > 0;

        // 1. Network in: apply every input that arrived since last tick.
        let mut net_time = Duration::ZERO;
        if let Some(server) = net.as_mut() {
            let t = Instant::now();
            server.receive_inputs();
            net_time += t.elapsed();
        }

        // 2. Simulation: move the NPCs.
        let npc_start = Instant::now();
        update_npcs(world, npcs, dt);
        // black_box tells the optimizer "assume someone looks at this memory",
        // so it can't decide the updates are pointless and delete them.
        let _ = black_box(world.as_ptr());
        let npc_time = npc_start.elapsed();

        // 3. Voxels: edits and random ticks, then a save if one is due.
        let mut voxel_time = Duration::ZERO;
        let mut save_time = Duration::ZERO;
        let mut saved = false;
        if let Some(terrain) = voxels.as_mut() {
            let t = Instant::now();
            terrain.tick();
            voxel_time = t.elapsed();

            if save_every_ticks > 0 && tick > 0 && tick % save_every_ticks == 0 {
                let t = Instant::now();
                terrain.save_dirty();
                save_time = t.elapsed();
                saved = true;
            }
        }

        // 4. Network out: one snapshot per player.
        if let Some(server) = net.as_mut() {
            let t = Instant::now();
            server.send_snapshots(tick, world, npcs);
            net_time += t.elapsed();
        }

        let work = woke.elapsed();
        let hashing_at_end = hashing.load(Ordering::Relaxed) > 0;
        samples.push(Sample {
            late,
            work,
            npc: npc_time,
            net: net_time,
            voxel: voxel_time,
            save: save_time,
            saved,
            hashing: hashing_at_start || hashing_at_end,
        });
        tick = tick.wrapping_add(1);

        // Next deadline. If this tick ran so long that we've already passed
        // it, count it as missed and move on. No catch-up ticks (out of scope).
        deadline += period;
        let finished = Instant::now();
        while deadline <= finished {
            deadline += period;
            missed += 1;
        }
    }
    (samples, missed)
}

// ---------------------------------------------------------------------------
// Background load
// ---------------------------------------------------------------------------

/// Spawns `count` threads that spin doing arithmetic until `stop` is set.
/// Arc = shared ownership across threads (reference counted);
/// AtomicBool = a flag that's safe to read and write from several threads.
fn start_load(count: usize, stop: Arc<AtomicBool>) -> Vec<thread::JoinHandle<()>> {
    (0..count)
        .map(|t| {
            let stop = Arc::clone(&stop); // each thread gets its own handle
            thread::spawn(move || {
                let mut x: u64 = t as u64 + 1;
                while !stop.load(Ordering::Relaxed) {
                    for _ in 0..10_000 {
                        // Rust panics on integer overflow in debug builds;
                        // wrapping_* says "overflow is fine here", like C.
                        x = x
                            .wrapping_mul(6364136223846793005)
                            .wrapping_add(1442695040888963407);
                    }
                    black_box(x);
                }
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

fn print_stats(label: &str, mut values: Vec<f64>, unit: &str) {
    if values.is_empty() {
        println!("{label:<10} no samples (run longer than the warm-up)");
        return;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let avg = values.iter().sum::<f64>() / values.len() as f64;
    // A closure: a small inline function that can read `values`.
    let pct = |p: f64| values[((values.len() - 1) as f64 * p).round() as usize];
    println!(
        "{label:<10} min {:>9.3}  avg {:>9.3}  p50 {:>9.3}  p99 {:>9.3}  max {:>9.3}  ({unit})",
        values[0],
        avg,
        pct(0.50),
        pct(0.99),
        values[values.len() - 1]
    );
}

fn report(
    cfg: &Config,
    npc_count: usize,
    samples: &[Sample],
    missed: u64,
    pinned_ok: bool,
    server_stats: Option<ServerStats>,
    client_stats: Option<ClientStats>,
    hash_times: &[Duration],
    voxel_stats: Option<VoxelStats>,
    saver_stats: Option<SaverStats>,
) {
    let mib = (cfg.objects * std::mem::size_of::<Object>()) as f64 / (1024.0 * 1024.0);
    let order = if cfg.scatter { "shuffled" } else { "in order" };
    let pin = match cfg.pin {
        None => "free".to_string(),
        Some(cpu) => format!("pin CPU {cpu} ({})", if pinned_ok { "ok" } else { "FAILED" }),
    };

    let kept: Vec<&Sample> = samples.iter().skip(WARMUP_TICKS).collect();
    let late_us: Vec<f64> = kept.iter().map(|s| s.late.as_secs_f64() * 1e6).collect();
    let work_ms: Vec<f64> = kept.iter().map(|s| s.work.as_secs_f64() * 1e3).collect();
    let npc_ms: Vec<f64> = kept.iter().map(|s| s.npc.as_secs_f64() * 1e3).collect();
    let net_ms: Vec<f64> = kept.iter().map(|s| s.net.as_secs_f64() * 1e3).collect();

    let period_ms = cfg.period_ms as f64;
    let mut over_budget = 0;
    let mut worst_work: f64 = 0.0;
    for w in &work_ms {
        if *w > period_ms {
            over_budget += 1;
        }
        worst_work = worst_work.max(*w);
    }

    println!();
    println!(
        "=== objects {} ({mib:.0} MiB) | NPCs {npc_count} ({order}) | {pin} | load {} | players {} | {} ms x {} s ===",
        cfg.objects, cfg.load, cfg.players, cfg.period_ms, cfg.seconds
    );
    println!(
        "ticks {}  missed deadlines {missed}  work > period {over_budget}  worst tick used {:.1}% of budget",
        work_ms.len(),
        worst_work / period_ms * 100.0
    );
    print_stats("wake late", late_us, "us");
    print_stats("work time", work_ms, "ms");
    if cfg.players > 0 || cfg.voxel_chunks > 0 {
        print_stats("  npc part", npc_ms, "ms");
    }
    if cfg.players > 0 {
        print_stats("  net part", net_ms, "ms");
    }
    if cfg.voxel_chunks > 0 {
        let voxel_ms: Vec<f64> = kept.iter().map(|s| s.voxel.as_secs_f64() * 1e3).collect();
        // Only the ticks that actually saved; the rest are all zero.
        let save_ms: Vec<f64> = kept
            .iter()
            .filter(|s| s.saved)
            .map(|s| s.save.as_secs_f64() * 1e3)
            .collect();
        print_stats("  voxels", voxel_ms, "ms");
        print_stats("  saving", save_ms, "ms");
    }

    // `if let` with a pair: only runs when both are Some.
    if let (Some(s), Some(c)) = (server_stats, client_stats) {
        println!(
            "packets    inputs sent {} recv {} (send failed {}) | snapshots sent {} recv {} (dropped {}) | errors {}",
            c.inputs_sent,
            s.inputs_received,
            c.inputs_failed,
            s.snapshots_sent,
            c.snapshots_received,
            s.snapshots_dropped,
            s.errors
        );
    }

    if let Some(v) = voxel_stats {
        let voxels = cfg.voxel_chunks * CHUNK_VOXELS;
        let mib = (cfg.voxel_chunks * CHUNK_VOXELS * 2) as f64 / (1024.0 * 1024.0);
        println!(
            "voxels     {} chunks = {:.1}M voxels ({mib:.0} MiB) | random ticks {}/chunk | edits {}/tick | changes {:.1}/tick | chunks saved {}",
            cfg.voxel_chunks,
            voxels as f64 / 1e6,
            cfg.random_ticks,
            cfg.voxel_edits,
            v.changes as f64 / samples.len().max(1) as f64,
            v.chunks_saved
        );
    }
    if let Some(s) = saver_stats {
        println!(
            "saver      compressed {} chunks, {:.1} MiB -> {:.1} MiB, busy {:.0} ms in total",
            s.chunks,
            s.bytes_in as f64 / (1024.0 * 1024.0),
            s.bytes_out as f64 / (1024.0 * 1024.0),
            s.busy.as_secs_f64() * 1e3
        );
    }

    if cfg.hash_every_ms > 0 {
        println!(
            "hashing    every {} ms: {} hash(es) on {} thread(s), Argon2id {} KiB x {} passes x {} lane(s)",
            cfg.hash_every_ms,
            cfg.hash_burst,
            cfg.hash_threads,
            cfg.hash_mem_kib,
            cfg.hash_passes,
            cfg.hash_lanes
        );

        // Split the ticks into those that overlapped a burst and those that didn't.
        // partition() sorts items into two Vecs by a true/false test.
        let (busy, quiet): (Vec<&Sample>, Vec<&Sample>) =
            kept.iter().copied().partition(|s| s.hashing);
        let mut busy_over = 0;
        for s in &busy {
            if s.work.as_secs_f64() * 1e3 > period_ms {
                busy_over += 1;
            }
        }
        println!(
            "ticks      during hashing {} (over budget {busy_over})  quiet {}  hashes done {}",
            busy.len(),
            quiet.len(),
            hash_times.len()
        );
        print_stats("late quiet", quiet.iter().map(|s| s.late.as_secs_f64() * 1e6).collect(), "us");
        print_stats("late hash", busy.iter().map(|s| s.late.as_secs_f64() * 1e6).collect(), "us");
        print_stats("work quiet", quiet.iter().map(|s| s.work.as_secs_f64() * 1e3).collect(), "ms");
        print_stats("work hash", busy.iter().map(|s| s.work.as_secs_f64() * 1e3).collect(), "ms");
        print_stats("hash time", hash_times.iter().map(|d| d.as_secs_f64() * 1e3).collect(), "ms");
    }
}

// ---------------------------------------------------------------------------

fn main() {
    let cfg = parse_args();

    eprintln!("building world of {} objects...", cfg.objects);
    let mut world = build_world(cfg.objects);
    let npcs = pick_npcs(cfg.objects, cfg.npc_every, cfg.scatter);
    let npc_count = npcs.len();

    let stop = Arc::new(AtomicBool::new(false));
    let load_threads = start_load(cfg.load, Arc::clone(&stop));

    // Background hashing: `hashing_active` is how many hasher threads are
    // mid-burst right now; the tick thread reads it to label each tick.
    let hashing_active = Arc::new(AtomicUsize::new(0));
    let hash_threads = start_hashers(cfg, Arc::clone(&hashing_active), Arc::clone(&stop));

    // Network: the server socket goes to the tick thread; the simulated
    // players get a thread of their own.
    let mut net: Option<ServerNet> = None;
    let mut client_thread = None;
    if cfg.players > 0 {
        let server = ServerNet::new(cfg.players, cfg.snapshot_npcs);
        client_thread = Some(start_clients(cfg, server.address(), Arc::clone(&stop)));
        net = Some(server);
    }

    // Voxels: the world lives on the tick thread; saving gets its own thread.
    let mut voxels: Option<VoxelWorld> = None;
    let mut saver_thread = None;
    if cfg.voxel_chunks > 0 {
        eprintln!("building voxel world of {} chunks...", cfg.voxel_chunks);
        let sender = if cfg.save_every_ms > 0 {
            let (sender, handle) = start_saver();
            saver_thread = Some(handle);
            Some(sender)
        } else {
            None
        };
        let terrain = VoxelWorld::new(&cfg, sender);
        eprintln!("  {:.1}M voxels", terrain.voxel_count() as f64 / 1e6);
        voxels = Some(terrain);
    }

    // `move` hands ownership of world, npcs, net, voxels and cfg to the new
    // thread. Main can't touch them after this; the compiler enforces it.
    let tick_thread = thread::spawn(move || {
        let pinned_ok = match cfg.pin {
            Some(cpu) => core_affinity::set_for_current(core_affinity::CoreId { id: cpu }),
            None => false,
        };
        let (samples, missed) =
            tick_loop(&cfg, &mut world, &npcs, &mut net, &mut voxels, &hashing_active);
        // Option::map: if there's a server, pull its stats out; if not, None.
        let server_stats = net.map(|server| server.stats);
        // Same for the voxel world. This also drops the world, and the saver's
        // Sender with it, which is what tells the saver thread to finish.
        let voxel_stats = voxels.map(|terrain| terrain.stats);
        (samples, missed, pinned_ok, server_stats, voxel_stats)
    });

    // join() waits for the thread to finish and hands back what it returned.
    let (samples, missed, pinned_ok, server_stats, voxel_stats) =
        tick_thread.join().expect("tick thread panicked");
    let saver_stats = saver_thread.map(|handle| handle.join().expect("saver thread panicked"));

    stop.store(true, Ordering::Relaxed);
    for handle in load_threads {
        handle.join().expect("load thread panicked");
    }
    let client_stats = client_thread.map(|handle| handle.join().expect("client thread panicked"));
    let mut hash_times: Vec<Duration> = Vec::new();
    for handle in hash_threads {
        hash_times.extend(handle.join().expect("hasher thread panicked"));
    }

    report(
        &cfg,
        npc_count,
        &samples,
        missed,
        pinned_ok,
        server_stats,
        client_stats,
        &hash_times,
        voxel_stats,
        saver_stats,
    );
}