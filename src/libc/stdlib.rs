/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0.
 * If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `stdlib.h`

use crate::abi::{CallFromHost, GuestFunction};
use crate::dyld::{export_c_func, export_c_func_aliased, FunctionExports};
use crate::fs::{resolve_path, GuestPath};
use crate::libc::clocale::{setlocale, LC_CTYPE};
use crate::libc::errno::{set_errno, EINVAL};
use crate::libc::string::strlen;
use crate::libc::wchar::wchar_t;
use crate::mem::{ConstPtr, ConstVoidPtr, GuestUSize, MutPtr, MutVoidPtr, Ptr, SafeRead};
use crate::objc::id;
use crate::{impl_GuestRet_for_large_struct, Environment};
use std::str::FromStr;

pub mod qsort;

#[derive(Default)]
pub struct State {
    rand: u32,
    random: u32,
    arc4random: u32,
    /// 48-bit linear-congruential PRNG state shared by the `drand48`/`lrand48`/
    /// `mrand48`/`seed48` family. Per the POSIX / Apple `drand48(3)` manpage
    /// (<https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/drand48.3.html>)
    /// the generator is `X_{n+1} = (a * X_n + c) mod 2^48`, with default
    /// `a = 0x5DEECE66D`, `c = 0xB`, and the documented initial seed of
    /// `0x1234ABCD330E` until `srand48`/`seed48` are called.
    drand48: Drand48State,
    pub atexit_handlers: Vec<GuestFunction>,
}


/// State for the POSIX `drand48`/`lrand48`/`mrand48` family. Mirrors what real
/// libc keeps internally — a 48-bit state plus the multiplier `a` and addend
/// `c` (modifiable via `lcong48`).
#[derive(Copy, Clone)]
struct Drand48State {
    /// 48-bit `X_n`, stored in the low 48 bits of a `u64`.
    state: u64,
    /// 48-bit multiplier `a` (default `0x5DEECE66D`).
    a: u64,
    /// 48-bit addend `c` (default `0xB`).
    c: u64,
}

impl Default for Drand48State {
    fn default() -> Self {
        Drand48State {
            state: 0x1234_ABCD_330E,
            a: 0x5DEE_CE66D,
            c: 0xB,
        }
    }
}

impl Drand48State {
    /// Advance `X_n` and return the new 48-bit value.
    fn step(&mut self) -> u64 {
        self.state = self.a.wrapping_mul(self.state).wrapping_add(self.c) & 0xFFFF_FFFF_FFFF;
        self.state
    }
}

/// Pack a `[u16; 3]` (little-endian, i.e. `xsubi[0]` is the lowest 16 bits) into
/// a 48-bit integer. Matches the Apple-documented layout for the `xsubi` arrays
/// taken by `seed48`, `erand48`, `nrand48`, `jrand48`.
fn pack_xsubi(env: &Environment, xsubi: ConstPtr<u16>) -> u64 {
    let lo = u64::from(env.mem.read(xsubi));
    let mid = u64::from(env.mem.read(xsubi + 1));
    let hi = u64::from(env.mem.read(xsubi + 2));
    lo | (mid << 16) | (hi << 32)
}

fn write_xsubi(env: &mut Environment, xsubi: MutPtr<u16>, value: u64) {
    env.mem.write(xsubi, (value & 0xFFFF) as u16);
    env.mem.write(xsubi + 1, ((value >> 16) & 0xFFFF) as u16);
    env.mem.write(xsubi + 2, ((value >> 32) & 0xFFFF) as u16);
}

fn malloc(env: &mut Environment, mut size: GuestUSize) -> MutVoidPtr {
    set_errno(env, 0);

    // =========================================================================
    // FIX: Перехват бага разработчиков игр (Integer Underflow)
    // Если размер подозрительно огромный (близок к 32-битному лимиту, >
    // 0xF0000000),
    // это почти наверняка отрицательное число (как -1920 байт для шага экрана).
    // Берем модуль (абсолютное значение), чтобы спасти игру от краша.
    // =========================================================================
    if size > 0xF000_0000 {
        let actual_size = (-(size as i32)) as GuestUSize;
        log!("TouchHLE::libc::stdlib: Hack! malloc passed negative size {:#x} ({}). Allocating {} bytes instead.", size, size as i32, actual_size);
        size = actual_size;
    }

    if size == 0 {
        size = 1;
        // Protect against dying on a 0-byte allocation: ISO C lets malloc(0)
        // return either NULL or a unique pointer; we choose unique...
    }

    // Refuse allocations that exceed guest address space (32-bit).
    // The practical guest heap is limited to ~512 MB; anything above that
    // is almost certainly a corrupted size value. Original iPhone OS devices
    // had at most 128-256 MB of RAM, so any allocation in the hundreds of
    // megabytes range is highly suspect. We use 0x2000_0000 (512 MB) as
    // the threshold — generous enough for real-world games with heavy
    // texture/audio buffers (e.g. Dead Space) while still catching obviously
    // corrupted values from buggy game engines (Digital Chocolate games
    // sometimes compute nonsensical allocation sizes due to NULL pointer
    // arithmetic when upstream issues cause initialization failures).
    if size > 0x2000_0000 {
        log!(
            "TouchHLE::libc::stdlib: malloc({:#x}) refused as out of range — returning NULL",
            size
        );
        set_errno(env, crate::libc::errno::ENOMEM);
        return MutVoidPtr::null();
    }

    let ptr = env.mem.alloc(size);
    if ptr.is_null() {
        // Say this out loud. A failed guest allocation is not a survivable
        // condition for most of these apps: `operator new` reacts by calling
        // one of the `std::__throw_*` helpers, which dyld.rs patches to return
        // rather than throw (touchHLE cannot unwind guest C++ exceptions), so
        // the caller gets a null buffer back from something that cannot fail
        // by contract and carries on with a corrupted object. That surfaces
        // far away from here - Dracula: The Last Sanctuary ends up with a
        // TeArray whose element count is 58 and whose storage pointer is null,
        // and `TeLayout::draw()` then calls a virtual method on 58 null
        // children every frame and draws nothing.
        log!(
            "Warning: malloc({:#x}) failed — the guest heap is exhausted. \
             Expect the app to misbehave: C++ `operator new` cannot report \
             this failure by throwing here, so callers will use a null buffer.",
            size
        );
        set_errno(env, crate::libc::errno::ENOMEM);
    }
    ptr.cast()
}

fn malloc_size(env: &mut Environment, ptr: ConstVoidPtr) -> GuestUSize {
    env.mem.malloc_size(ptr)
}

fn calloc(env: &mut Environment, count: GuestUSize, size: GuestUSize) -> MutVoidPtr {
    set_errno(env, 0);

    // Per the C standard (and Apple's calloc(3) man page), calloc must return
    // NULL if `count * size` would overflow, rather than aborting. Using
    // `.unwrap()` here previously panicked and killed the whole emulator when
    // a buggy game computed a bogus element count. Handle overflow gracefully.
    let mut total = match size.checked_mul(count) {
        Some(v) => v,
        None => {
            log!(
                "TouchHLE::libc::stdlib: calloc({:#x}, {:#x}) overflowed — returning NULL",
                count,
                size
            );
            set_errno(env, crate::libc::errno::ENOMEM);
            return MutVoidPtr::null();
        }
    };

    // Same out-of-range guard as malloc: refuse obviously-corrupted sizes
    // instead of exhausting the guest heap.
    if total > 0x2000_0000 {
        log!(
            "TouchHLE::libc::stdlib: calloc total {:#x} refused as out of range — returning NULL",
            total
        );
        set_errno(env, crate::libc::errno::ENOMEM);
        return MutVoidPtr::null();
    }

    if total == 0 {
        total = 1;
        // Защита от падения
    }
    env.mem.calloc(total)
}

fn NSZoneMalloc(env: &mut Environment, _zone: id, mut size: GuestUSize) -> MutVoidPtr {
    if size == 0 {
        size = 1;
    }
    env.mem.alloc(size)
}

fn NSZoneRealloc(
    env: &mut Environment,
    _zone: MutVoidPtr,
    ptr: MutVoidPtr,
    mut size: GuestUSize,
) -> MutVoidPtr {
    if size == 0 {
        size = 1;
    }
    env.mem.realloc(ptr, size)
}

fn NSZoneFree(env: &mut Environment, _zone: MutVoidPtr, ptr: MutVoidPtr) {
    if ptr.is_null() {
        return;
    }
    // Use the same safe path as libc free() — unknown pointers are
    // gracefully rejected inside Mem::free with a log message.
    env.mem.free(ptr)
}

/// `int posix_memalign(void **memptr, size_t alignment, size_t size);`
///
/// Per the [POSIX manpage](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/posix_memalign.3.html):
///
/// > The function `posix_memalign()` allocates `size` bytes of memory such
/// > that the allocation's base address is a multiple of `alignment`, and
/// > returns the allocation in the value pointed to by `memptr`. […]
/// > `alignment` must be a power of 2 at least as large as `sizeof(void *)`.
/// > Returns zero on success, otherwise returns one of the error values
/// > listed below. The value of `errno` is not set on these errors.
/// > `[EINVAL]` The `alignment` parameter is not a power of 2 at least as
/// >            large as `sizeof(void *)`.
/// > `[ENOMEM]` Memory allocation error.
///
/// The touchHLE heap (see `src/mem/allocator.rs`) already returns 16-byte
/// aligned chunks. Apple's libmalloc also guarantees that on iOS. So for the
/// common alignments (≤16) this is just `malloc`. For larger alignments
/// (e.g. page-sized buffers requested by some crypto libraries) we
/// over-allocate, slide the user pointer forward, and stash the original
/// allocation address in the word immediately preceding the returned
/// pointer so `free()` can find it again.
fn posix_memalign(
    env: &mut Environment,
    memptr: MutPtr<MutVoidPtr>,
    alignment: GuestUSize,
    size: GuestUSize,
) -> i32 {
    if memptr.is_null() {
        return EINVAL;
    }
    let ptr_align: GuestUSize = std::mem::size_of::<u32>() as GuestUSize;
    if alignment < ptr_align || !alignment.is_power_of_two() {
        return EINVAL;
    }
    // touchHLE's allocator naturally aligns to at least 16 bytes.
    const NATURAL_ALIGN: GuestUSize = 16;
    if alignment <= NATURAL_ALIGN {
        let p = malloc(env, size);
        if p.is_null() {
            return crate::libc::errno::ENOMEM;
        }
        env.mem.write(memptr, p);
        return 0;
    }
    // Over-allocate so that we definitely have room for an aligned slice
    // plus a 4-byte header storing the original allocation pointer.
    let header: GuestUSize = std::mem::size_of::<u32>() as GuestUSize;
    let Some(over) = size.checked_add(alignment).and_then(|s| s.checked_add(header)) else {
        return crate::libc::errno::ENOMEM;
    };
    let raw = env.mem.alloc(over);
    if raw.is_null() {
        return crate::libc::errno::ENOMEM;
    }
    let raw_bits = raw.to_bits();
    // Align up to `alignment` while leaving at least `header` bytes free
    // before the aligned address for our bookkeeping word.
    let aligned_bits = (raw_bits + header + alignment - 1) & !(alignment - 1);
    debug_assert!(aligned_bits >= raw_bits + header);
    let aligned: MutVoidPtr = MutVoidPtr::from_bits(aligned_bits);
    let header_ptr: MutPtr<u32> = MutPtr::from_bits(aligned_bits - header);
    env.mem.write(header_ptr, raw_bits);
    env.mem.write(memptr, aligned);
    0
}

/// `void *valloc(size_t size);` — page-size aligned allocation. Equivalent
/// to `posix_memalign(&p, getpagesize(), size)`.
fn valloc(env: &mut Environment, size: GuestUSize) -> MutVoidPtr {
    // Same page size touchHLE reports via `_NSGetExecutablePath` etc.
    const PAGE_SIZE: GuestUSize = 4096;
    let out: MutPtr<MutVoidPtr> = env.mem.alloc(4).cast();
    let rc = posix_memalign(env, out, PAGE_SIZE, size);
    let ptr = if rc == 0 { env.mem.read(out) } else { MutVoidPtr::null() };
    env.mem.free(out.cast());
    ptr
}

fn realloc(env: &mut Environment, ptr: MutVoidPtr, mut size: GuestUSize) -> MutVoidPtr {
    set_errno(env, 0);
    if ptr.is_null() {
        return malloc(env, size);
    }
    if size == 0 {
        size = 1;
    }
    env.mem.realloc(ptr, size)
}

fn reallocf(env: &mut Environment, ptr: MutVoidPtr, mut size: GuestUSize) -> MutVoidPtr {
    set_errno(env, 0);
    if ptr.is_null() {
        return malloc(env, size);
    }
    if size == 0 {
        size = 1;
    }

    // Пытаемся выделить новую память
    let new_ptr = env.mem.realloc(ptr, size);
    // Главная фишка reallocf: если realloc вернул NULL (не удалось выделить),
    // старый указатель должен быть освобожден.
    if new_ptr.is_null() {
        env.mem.free(ptr);
    }

    new_ptr
}

fn free(env: &mut Environment, ptr: MutVoidPtr) {
    if env.objc.get_host_object(ptr.cast()).is_some() {
        log!(
            "App attempted to call free({:?}) on an object, calling dealloc_object() instead!",
            ptr
        );
        env.objc.dealloc_object(ptr.cast(), &mut env.mem);
        return;
    }
    set_errno(env, 0);
    if ptr.is_null() {
        return;
    }
    let addr = ptr.to_bits();
    // If the pointer looks obviously bogus, log caller context so we can trace
    // where the corruption originated, then bail out instead of confusing the
    // underlying allocator.
    if !(0x1000..0xfff0_0000).contains(&addr) {
        let pc = env.cpu.regs()[crate::cpu::Cpu::PC];
        let lr = env.cpu.regs()[crate::cpu::Cpu::LR];
        log!(
            "free({:#x}) rejected: pointer outside any plausible heap range \
             (caller PC={:#x} LR={:#x})",
            addr,
            pc,
            lr
        );
        return;
    }
    // If the pointer isn't part of any known allocation, bail out — the
    // underlying allocator will otherwise log "Can't free" without context.
    // This catches cases where a buggy stub returned garbage that the guest
    // later hands back to free() (e.g. misinterpreting a float as a pointer).
    if !env.mem.is_known_allocation(addr) {
        let pc = env.cpu.regs()[crate::cpu::Cpu::PC];
        let lr = env.cpu.regs()[crate::cpu::Cpu::LR];
        log!(
            "free({:#x}) rejected: not a known allocation \
             (caller PC={:#x} LR={:#x})",
            addr,
            pc,
            lr
        );
        return;
    }
    env.mem.free(ptr);
}

fn atexit(env: &mut Environment, func: GuestFunction) -> i32 {
    set_errno(env, 0);
    // Регистрируем функцию в стейте эмулятора
    env.libc_state.stdlib.atexit_handlers.push(func);
    0 // 0 означает успешную регистрацию
}

#[allow(rustdoc::broken_intra_doc_links)]
fn count_whitespace_generic<
    T,
    U,
    F1: Fn(&mut Environment, MutPtr<U>, GuestUSize) -> Result<T, ()>,
    F2: Fn(&mut Environment, MutPtr<U>, u8),
>(
    env: &mut Environment,
    getc_fn: F1,
    ungetc_fn: F2,
    subject: MutPtr<U>,
    offset: GuestUSize,
) -> Result<GuestUSize, GuestUSize>
where
    u8: From<T>,
{
    let mut count: GuestUSize = offset;
    loop {
        let Ok(c) = getc_fn(env, subject, count) else {
            return Err(count - offset);
        };
        let c: u8 = c.into();
        if c.is_ascii_whitespace() || c == b'\x0b' {
            count += 1;
        } else {
            ungetc_fn(env, subject, c);
            break;
        }
    }
    Ok(count - offset)
}

fn atoi(env: &mut Environment, s: ConstPtr<u8>) -> i32 {
    set_errno(env, 0);
    let (res, _) = strtol_inner(env, s, 10).unwrap_or((0, 0));
    res
}

fn atol(env: &mut Environment, s: ConstPtr<u8>) -> i32 {
    atoi(env, s)
}

fn atof(env: &mut Environment, s: ConstPtr<u8>) -> f64 {
    strtod(env, s, Ptr::null())
}

fn strtod(env: &mut Environment, nptr: ConstPtr<u8>, endptr: MutPtr<MutPtr<u8>>) -> f64 {
    set_errno(env, 0);
    log_dbg!("strtod nptr {}", env.mem.cstr_at_utf8(nptr).unwrap());
    let (res, len) = atof_inner(env, nptr).unwrap_or((0.0, 0));
    if !endptr.is_null() {
        env.mem.write(endptr, (nptr + len).cast_mut());
    }
    res
}

fn prng(state: u32) -> u32 {
    let mut state: u32 = state.max(1);
    state ^= state << 13;
    state ^= state >> 17;
    state ^= state << 5;
    state
}

const RAND_MAX: i32 = i32::MAX;

fn srand(env: &mut Environment, seed: u32) {
    env.libc_state.stdlib.rand = seed;
}

fn sranddev(env: &mut Environment) {
    let seed = arc4random(env);
    env.libc_state.stdlib.rand = seed;
    log!("sranddev() stubbed: seeded rand with {}", seed);
}

fn rand(env: &mut Environment) -> i32 {
    env.libc_state.stdlib.rand = prng(env.libc_state.stdlib.rand);
    (env.libc_state.stdlib.rand as i32) & RAND_MAX
}

fn rand_r(env: &mut Environment, seed_ptr: MutPtr<u32>) -> i32 {
    let mut seed = env.mem.read(seed_ptr);
    seed = prng(seed);
    env.mem.write(seed_ptr, seed);
    (seed as i32) & RAND_MAX
}

fn srandom(env: &mut Environment, seed: u32) {
    set_errno(env, 0);
    env.libc_state.stdlib.random = seed;
}

fn random(env: &mut Environment) -> i32 {
    set_errno(env, 0);
    env.libc_state.stdlib.random = prng(env.libc_state.stdlib.random);
    (env.libc_state.stdlib.random as i32) & RAND_MAX
}

fn arc4random_stir(env: &mut Environment) -> u32 {
    env.libc_state.stdlib.arc4random = prng(env.libc_state.stdlib.arc4random);
    env.libc_state.stdlib.arc4random
}

fn arc4random_addrandom(env: &mut Environment) -> u32 {
    env.libc_state.stdlib.arc4random = prng(env.libc_state.stdlib.arc4random);
    env.libc_state.stdlib.arc4random
}

fn arc4random(env: &mut Environment) -> u32 {
    env.libc_state.stdlib.arc4random = prng(env.libc_state.stdlib.arc4random);
    env.libc_state.stdlib.arc4random
}

/// `arc4random_uniform(upper_bound)` — returns a uniformly distributed
/// random number less than `upper_bound`.
///
/// Per Apple manpage: "arc4random_uniform() is recommended over
/// constructions like `arc4random() % upper_bound` as it avoids
/// modulo bias when the upper bound is not a power of two."
fn arc4random_uniform(env: &mut Environment, upper_bound: u32) -> u32 {
    if upper_bound == 0 {
        return 0;
    }
    // Rejection sampling to eliminate modulo bias.
    // Compute the largest multiple of upper_bound that fits in u32.
    let limit = u32::MAX - (u32::MAX % upper_bound);
    loop {
        let r = arc4random(env);
        if r < limit {
            return r % upper_bound;
        }
    }
}

// MARK: - drand48 family
//
// POSIX / Apple iPhone OS specification for these functions:
// https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/drand48.3.html
//
// All of these share a 48-bit LCG state. `drand48`/`lrand48`/`mrand48` use the
// implicit per-process state in `env.libc_state.stdlib.drand48`; the
// `erand48`/`nrand48`/`jrand48` variants accept an explicit `xsubi` array
// (three u16s, little-endian limbs).

fn drand48(env: &mut Environment) -> f64 {
    let next = env.libc_state.stdlib.drand48.step();
    // Apple manpage: "drand48() and erand48() return non-negative,
    // double-precision, floating-point values, uniformly distributed over the
    // interval [0.0, 1.0)." 2^-48 gives exactly that distribution.
    (next as f64) * (1.0_f64 / 281_474_976_710_656.0_f64)
}

fn erand48(env: &mut Environment, xsubi: MutPtr<u16>) -> f64 {
    let mut local = Drand48State {
        state: pack_xsubi(env, xsubi.cast_const()),
        a: env.libc_state.stdlib.drand48.a,
        c: env.libc_state.stdlib.drand48.c,
    };
    let next = local.step();
    write_xsubi(env, xsubi, next);
    (next as f64) * (1.0_f64 / 281_474_976_710_656.0_f64)
}

fn lrand48(env: &mut Environment) -> i32 {
    // "lrand48() and nrand48() return non-negative, long integers,
    // uniformly distributed over the interval [0, 2^31)" — take the high
    // 31 bits of the 48-bit state, per the Apple manpage.
    let next = env.libc_state.stdlib.drand48.step();
    (next >> 17) as i32
}

fn nrand48(env: &mut Environment, xsubi: MutPtr<u16>) -> i32 {
    let mut local = Drand48State {
        state: pack_xsubi(env, xsubi.cast_const()),
        a: env.libc_state.stdlib.drand48.a,
        c: env.libc_state.stdlib.drand48.c,
    };
    let next = local.step();
    write_xsubi(env, xsubi, next);
    (next >> 17) as i32
}

fn mrand48(env: &mut Environment) -> i32 {
    // "mrand48() and jrand48() return signed long integers uniformly
    // distributed over the interval [-2^31, 2^31)." Take the high 32 bits
    // of the 48-bit state and reinterpret as i32.
    let next = env.libc_state.stdlib.drand48.step();
    ((next >> 16) & 0xFFFF_FFFF) as u32 as i32
}

fn jrand48(env: &mut Environment, xsubi: MutPtr<u16>) -> i32 {
    let mut local = Drand48State {
        state: pack_xsubi(env, xsubi.cast_const()),
        a: env.libc_state.stdlib.drand48.a,
        c: env.libc_state.stdlib.drand48.c,
    };
    let next = local.step();
    write_xsubi(env, xsubi, next);
    ((next >> 16) & 0xFFFF_FFFF) as u32 as i32
}

fn srand48(env: &mut Environment, seedval: i32) {
    // Apple manpage: "srand48() initializes the high-order 32 bits of the
    // 48-bit X_i to the low-order 32 bits of the argument; the low-order
    // 16 bits of X_i are set to the arbitrary value 330E_16." Reset
    // multiplier/addend to their defaults, matching the spec.
    let seed = (seedval as u32) as u64;
    env.libc_state.stdlib.drand48 = Drand48State {
        state: (seed << 16) | 0x330E,
        a: 0x5DEE_CE66D,
        c: 0xB,
    };
}

fn seed48(env: &mut Environment, xsubi: MutPtr<u16>) -> MutPtr<u16> {
    // "seed48() sets the value of the X_i to the 48-bit value specified in
    // the argument, and returns a pointer to a 14-byte array containing the
    // previous value of X_i." The Apple/POSIX contract uses a per-process
    // internal buffer for the return value — emulate that by allocating it
    // once and re-using the slot.
    let previous = env.libc_state.stdlib.drand48.state;

    let new_state = pack_xsubi(env, xsubi.cast_const());
    env.libc_state.stdlib.drand48 = Drand48State {
        state: new_state,
        a: 0x5DEE_CE66D,
        c: 0xB,
    };

    // Reuse a static guest-side buffer for the returned `unsigned short[3]`.
    // POSIX guarantees the returned pointer is valid until the next call to
    // `seed48`/`srand48`/`lcong48`. We mirror that contract by reallocating
    // here — touchHLE's guest allocator keeps the pointer alive until the
    // application explicitly frees it (which it must not, per POSIX).
    let buf: MutPtr<u16> = env.mem.alloc(6).cast();
    write_xsubi(env, buf, previous);
    buf
}

// MARK: - mkstemp / mkdtemp (POSIX, Apple manpages)
//
// https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/mkstemp.3.html
//
// `mkstemp` takes a writable path template ending in `XXXXXX`, replaces the
// trailing `X`s with random characters that make the name unique, creates the
// file with `open(O_RDWR | O_CREAT | O_EXCL, 0600)` and returns the fd.
// `mkdtemp` is the directory analogue.

/// Characters used to fill the `XXXXXX` portion of the template, matching the
/// Apple / BSD `mkstemp` implementation (uppercase + lowercase + digits).
const MKTEMP_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

fn fill_template_xxxxxx(env: &mut Environment, template: MutPtr<u8>) -> Option<u32> {
    let template_str = match env.mem.cstr_at_utf8(template.cast_const()) {
        Ok(s) => s.to_owned(),
        Err(_) => return None,
    };
    let len = template_str.len();
    if len < 6
        || !template_str.as_bytes()[len - 6..]
            .iter()
            .all(|&c| c == b'X')
    {
        // Apple manpage: "If the template doesn’t contain six trailing X's,
        // -1 is returned and errno is set to EINVAL."
        return None;
    }
    let suffix_offset = (len - 6) as u32;
    for i in 0..6 {
        // Drive randomness from arc4random so we don't perturb the
        // user-visible drand48 state (which the guest may be sampling
        // deterministically after `srand48`).
        let r = arc4random(env);
        let ch = MKTEMP_CHARS[(r as usize) % MKTEMP_CHARS.len()];
        env.mem.write(template + suffix_offset + i, ch);
    }
    Some(suffix_offset)
}

fn mkstemp(env: &mut Environment, template: MutPtr<u8>) -> i32 {
    use crate::libc::errno::{ENOENT, ENOTDIR};
    set_errno(env, 0);

    // Try a handful of randomisations before giving up, mirroring BSD libc's
    // `_gettemp` (TMP_MAX is documented as at least 308,915,776 on real
    // systems but games never need that many — touchHLE only needs to keep
    // making progress in the rare case of a collision).
    for _ in 0..128 {
        if fill_template_xxxxxx(env, template).is_none() {
            set_errno(env, EINVAL);
            return -1;
        }
        // O_RDWR | O_CREAT | O_EXCL — same flag combination Darwin libc uses.
        const O_RDWR: i32 = 0x0002;
        const O_CREAT: i32 = 0x0200;
        const O_EXCL: i32 = 0x0800;
        let fd = crate::libc::posix_io::open_direct(
            env,
            template.cast_const(),
            O_RDWR | O_CREAT | O_EXCL,
        );
        if fd >= 0 {
            return fd;
        }
        // If the failure is a hard error (missing directory etc.) there is
        // no point in retrying — propagate it.
        let errno = crate::libc::errno::get_errno(env);
        if errno == ENOENT || errno == ENOTDIR {
            return -1;
        }
    }
    set_errno(env, crate::libc::errno::EEXIST);
    -1
}

fn mkdtemp(env: &mut Environment, template: MutPtr<u8>) -> MutPtr<u8> {
    set_errno(env, 0);
    for _ in 0..128 {
        if fill_template_xxxxxx(env, template).is_none() {
            set_errno(env, EINVAL);
            return Ptr::null();
        }
        // Read the path, create the directory exclusively.
        let path_str = match env.mem.cstr_at_utf8(template.cast_const()) {
            Ok(s) => s.to_owned(),
            Err(_) => {
                set_errno(env, EINVAL);
                return Ptr::null();
            }
        };
        let guest_path = GuestPath::new(&path_str);
        if env.fs.exists(guest_path) {
            continue; // collision; reseed and try again
        }
        match env.fs.create_dir(guest_path) {
            Ok(_) => return template,
            Err(_) => continue,
        }
    }
    set_errno(env, crate::libc::errno::EEXIST);
    Ptr::null()
}

fn mktemp(env: &mut Environment, template: MutPtr<u8>) -> MutPtr<u8> {
    // Apple manpage: "The mktemp() function is dangerous; its use is
    // discouraged because it is racy. Use mkstemp() instead." We implement it
    // anyway for guest binaries that link it — fill in the template but do
    // NOT create the file, exactly as the historical contract requires.
    set_errno(env, 0);
    if fill_template_xxxxxx(env, template).is_none() {
        set_errno(env, EINVAL);
        return Ptr::null();
    }
    template
}

fn lcong48(env: &mut Environment, param: MutPtr<u16>) {
    // "lcong48() allows full user control over the multiplier and addend
    // terms in the formula." param is a `unsigned short[7]`: the first
    // three limbs seed X_i, the next three (limbs 3..6) replace `a`, and
    // the last (limb 6) replaces `c`.
    let state = pack_xsubi(env, param.cast_const());
    let a_lo = u64::from(env.mem.read(param + 3));
    let a_mid = u64::from(env.mem.read(param + 4));
    let a_hi = u64::from(env.mem.read(param + 5));
    let a = a_lo | (a_mid << 16) | (a_hi << 32);
    let c = u64::from(env.mem.read(param + 6));
    env.libc_state.stdlib.drand48 = Drand48State { state, a, c };
}

fn getenv(env: &mut Environment, name: ConstPtr<u8>) -> MutPtr<u8> {
    let name_cstr = env.mem.cstr_at(name);
    let name_str = std::str::from_utf8(name_cstr).unwrap_or("");
    let Some(&value) = env.env_vars.get(name_cstr) else {
        // POSIX `getenv()` returns NULL for unset variables — this is the
        // documented success path for "variable doesn't exist". Logging a
        // Warning every time floods the console for any guest using a
        // managed runtime: Mono probes ~30 MONO_*/GC_* vars on startup,
        // Adobe AIR queries MMGC_HEAP_*, Lua looks up LUA_PATH/CPATH,
        // CoreFoundation reads CFFIXED_USER_HOME etc. None of these are
        // errors, so we demote to debug-only.
        log_dbg!("getenv({:?} ({:?})) => NULL (unset)", name, name_str);
        return Ptr::null();
    };
    log_dbg!(
        "getenv({:?} ({:?})) => {:?} ({:?})",
        name,
        name_cstr,
        value,
        env.mem.cstr_at_utf8(value),
    );
    value
}

// === ИСПРАВЛЕННЫЙ setenv ДЛЯ ОБХОДА БЛОКИРОВКИ ПАМЯТИ ===
fn setenv(env: &mut Environment, name: ConstPtr<u8>, value: ConstPtr<u8>, overwrite: i32) -> i32 {
    set_errno(env, 0);
    // Сохраняем имя в отдельный вектор, чтобы отпустить блокировку памяти
    let name_bytes = env.mem.cstr_at(name).to_vec();
    if let Some(&existing) = env.env_vars.get(&name_bytes) {
        if overwrite == 0 {
            return 0;
        }
        env.mem.free(existing.cast());
    };
    let value = super::string::strdup(env, value);
    env.env_vars.insert(name_bytes, value);
    0
}

// === ИСПРАВЛЕННЫЙ unsetenv ДЛЯ ОБХОДА БЛОКИРОВКИ ПАМЯТИ ===
fn unsetenv(env: &mut Environment, name: ConstPtr<u8>) -> i32 {
    set_errno(env, 0);
    // Сохраняем имя в отдельный вектор
    let name_bytes = env.mem.cstr_at(name).to_vec();
    if let Some(&existing) = env.env_vars.get(&name_bytes) {
        env.mem.free(existing.cast());
        env.env_vars.remove(&name_bytes);
        0
    } else {
        set_errno(env, EINVAL);
        -1
    }
}

fn exit(env: &mut Environment, exit_code: i32) {
    set_errno(env, 0);

    // Забираем список функций через mem::take, чтобы избежать проблем с borrow
    // checker,
    // так как вызов call_from_host требует мутабельного доступа к env.
    let handlers = std::mem::take(&mut env.libc_state.stdlib.atexit_handlers);

    // По стандарту atexit вызывает функции в обратном порядке (LIFO), поэтому
    // делаем .rev()
    for func in handlers.into_iter().rev() {
        log_dbg!("Executing atexit handler: {:?}", func);
        // Вызываем гостевую функцию (она не принимает аргументов и ничего не
        // возвращает)
        let _: () = func.call_from_host(env, ());
    }

    // Log the exit so it's clear in CI/run logs why the process stopped;
    // previously this exited silently, which made the logs end abruptly with no
    // explanation.
    echo!("App called exit({}); touchHLE will now quit.", exit_code);
    std::process::exit(exit_code);
}

fn abort(env: &mut Environment) {
    // abort() means the guest hit a fatal error (e.g. a failed assertion or an
    // uncaught C++ exception calling std::terminate). Log it with a guest stack
    // trace before quitting so the cause is visible, instead of exiting
    // silently.
    echo!("App called abort(); the guest encountered a fatal error.");
    env.stack_trace_current();
    std::process::exit(1);
}

fn bsearch(
    env: &mut Environment,
    key: ConstVoidPtr,
    items: ConstVoidPtr,
    item_count: GuestUSize,
    item_size: GuestUSize,
    compare_callback: GuestFunction, // (*int)(const void*, const void*)
) -> ConstVoidPtr {
    log_dbg!(
        "binary search for {:?} in {} items of size {:#x} starting at {:?}",
        key,
        item_count,
        item_size,
        items
    );
    let mut low = 0;
    let mut len = item_count;
    while len > 0 {
        let half_len = len / 2;
        let item: ConstVoidPtr = (items.cast::<u8>() + item_size * (low + half_len)).cast();
        // key must be first argument
        let cmp_result: i32 = compare_callback.call_from_host(env, (key, item));
        (low, len) = match cmp_result.signum() {
            0 => {
                log_dbg!("=> {:?}", item);
                return item;
            }
            1 => (low + half_len + 1, len - half_len - 1),
            -1 => (low, half_len),
            _ => unreachable!(),
        }
    }
    log_dbg!("=> NULL (not found)");
    Ptr::null()
}

fn strtof(env: &mut Environment, nptr: ConstPtr<u8>, endptr: MutPtr<ConstPtr<u8>>) -> f32 {
    set_errno(env, 0);
    let (number, length) = atof_inner(env, nptr).unwrap_or((0.0, 0));
    if !endptr.is_null() {
        env.mem.write(endptr, nptr + length);
    }
    number as f32
}

pub fn strtoul(
    env: &mut Environment,
    str: ConstPtr<u8>,
    endptr: MutPtr<MutPtr<u8>>,
    base: i32,
) -> u32 {
    set_errno(env, 0);
    let parse_res = str_to_int_inner_generic(
        env,
        |env, s, idx| Ok(env.mem.read(s + idx)),
        |_, _, _| (),
        str.cast_mut(),
        0, // starting offset
        base.try_into().unwrap(),
        u32::MAX, // max_length
        |s, base| u32::from_str_radix(s, base).unwrap_or(u32::MAX),
        |num| num.wrapping_neg(),
    );
    match parse_res {
        Ok((res, len)) => {
            if !endptr.is_null() {
                env.mem.write(endptr, (str + len).cast_mut());
            }
            res
        }
        Err(_) => {
            if !endptr.is_null() {
                env.mem.write(endptr, str.cast_mut());
            }
            0
        }
    }
}
fn strtoull(
    env: &mut Environment,
    str: ConstPtr<u8>,
    endptr: MutPtr<MutPtr<u8>>,
    base: i32,
) -> u64 {
    set_errno(env, 0);
    let parse_res = str_to_int_inner_generic(
        env,
        |env, s, idx| Ok(env.mem.read(s + idx)),
        |_, _, _| (),
        str.cast_mut(),
        0, // starting offset
        base.try_into().unwrap(),
        u32::MAX, // <--- ИСПРАВЛЕНО НА u32::MAX
        |s, base| u64::from_str_radix(s, base).unwrap_or(u64::MAX),
        |num| num.wrapping_neg(),
    );
    match parse_res {
        Ok((res, len)) => {
            if !endptr.is_null() {
                env.mem.write(endptr, (str + len).cast_mut());
            }
            res
        }
        Err(_) => {
            if !endptr.is_null() {
                env.mem.write(endptr, str.cast_mut());
            }
            0
        }
    }
}

fn strtoll(
    env: &mut Environment,
    str: ConstPtr<u8>,
    endptr: MutPtr<MutPtr<u8>>,
    base: i32,
) -> i64 {
    set_errno(env, 0);
    let parse_res = str_to_int_inner_generic(
        env,
        |env, s, idx| Ok(env.mem.read(s + idx)),
        |_, _, _| (),
        str.cast_mut(),
        0, // starting offset
        base.try_into().unwrap(),
        u32::MAX, // max_length
        |s, base| i64::from_str_radix(s, base).unwrap_or(i64::MAX),
        |num| num.checked_mul(-1).unwrap_or(i64::MIN),
    );
    match parse_res {
        Ok((res, len)) => {
            if !endptr.is_null() {
                env.mem.write(endptr, (str + len).cast_mut());
            }
            res
        }
        Err(_) => {
            if !endptr.is_null() {
                env.mem.write(endptr, str.cast_mut());
            }
            0
        }
    }
}

fn strtol(env: &mut Environment, str: ConstPtr<u8>, endptr: MutPtr<MutPtr<u8>>, base: i32) -> i32 {
    set_errno(env, 0);
    match strtol_inner(env, str, base as u32) {
        Ok((res, len)) => {
            if !endptr.is_null() {
                env.mem.write(endptr, (str + len).cast_mut());
            }
            res
        }
        Err(_) => {
            if !endptr.is_null() {
                env.mem.write(endptr, str.cast_mut());
            }
            0
        }
    }
}

fn realpath(
    env: &mut Environment,
    file_name: ConstPtr<u8>,
    resolve_name: MutPtr<u8>,
) -> MutPtr<u8> {
    assert!(!resolve_name.is_null());
    let file_name_str = env.mem.cstr_at_utf8(file_name).unwrap();
    let resolved = resolve_path(
        GuestPath::new(file_name_str),
        Some(env.fs.working_directory()),
    );
    let result = format!("/{}", resolved.join("/"));
    env.mem
        .bytes_at_mut(resolve_name, result.len() as GuestUSize)
        .copy_from_slice(result.as_bytes());
    env.mem
        .write(resolve_name + result.len() as GuestUSize, b'\0');
    resolve_name
}

fn mbstowcs(
    env: &mut Environment,
    pwcs: MutPtr<wchar_t>,
    s: ConstPtr<u8>,
    n: GuestUSize,
) -> GuestUSize {
    set_errno(env, 0);
    let ctype_locale = setlocale(env, LC_CTYPE, Ptr::null());
    assert_eq!(env.mem.read(ctype_locale), b'C');
    let size = strlen(env, s);
    let to_write = size.min(n);
    for i in 0..to_write {
        let c = env.mem.read(s + i);
        env.mem.write(pwcs + i, c as wchar_t);
    }
    if to_write < n {
        env.mem.write(pwcs + to_write, wchar_t::default());
    }
    to_write
}

fn wcstombs(
    env: &mut Environment,
    s: ConstPtr<u8>,
    pwcs: MutPtr<wchar_t>,
    n: GuestUSize,
) -> GuestUSize {
    let ctype_locale = setlocale(env, LC_CTYPE, Ptr::null());
    assert_eq!(env.mem.read(ctype_locale), b'C');
    if n == 0 {
        return 0;
    }
    let wcstr = env.mem.wcstr_at(pwcs);
    let len = (wcstr.len() as GuestUSize).min(n);
    env.mem
        .bytes_at_mut(s.cast_mut(), len)
        .copy_from_slice(wcstr.as_bytes());
    if len < n {
        env.mem.write((s + len).cast_mut(), b'\0');
    }
    len
}

fn system(env: &mut Environment, cmd: ConstPtr<u8>) -> i32 {
    if cmd.is_null() {
        return 1;
        // shell is available
    }
    let cmd_str = env.mem.cstr_at_utf8(cmd).unwrap_or("").to_string();
    log!("system({:?})", cmd_str);
    // split_whitespace() автоматически игнорирует пробелы в начале и конце
    let parts: Vec<&str> = cmd_str.split_whitespace().collect();
    if parts.is_empty() {
        return 0;
    }

    match parts[0] {
        "mkdir" => {
            // find path argument (skip flags like -p)
            let path_arg = parts.iter().skip(1).find(|a| !a.starts_with('-'));
            if let Some(path) = path_arg {
                let guest_path = GuestPath::new(path);
                // use create_dir_all to support mkdir -p semantics
                match env.fs.create_dir_all(guest_path) {
                    Ok(_) => {
                        log!("system: mkdir {:?} => success", path);
                        0
                    }
                    Err(e) => {
                        log!("system: mkdir {:?} => error: {:?}", path, e);
                        1
                    }
                }
            } else {
                1
            }
        }
        _ => {
            log!(
                "Warning: system({:?}) not implemented, returning 0",
                cmd_str
            );
            0
        }
    }
}

fn dladdr(_env: &mut Environment, _addr: ConstVoidPtr, _info: MutVoidPtr) -> i32 {
    // FakeDladdr
    0
}

fn kqueue(_env: &mut Environment) -> i32 {
    // FakeKqueue
    999
}

/// `int _NSGetExecutablePath(char *buf, uint32_t *bufsize);` — see
/// <https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/dyld.3.html>.
///
/// Writes the path of the currently running executable into `buf`. On entry
/// `*bufsize` is the capacity of `buf`; on success the path (including the
/// trailing NUL byte) is written and 0 is returned. If the buffer is too
/// small, `*bufsize` is updated to the required capacity and -1 is returned
/// (the buffer contents are undefined in that case).
///
/// This function is the canonical way iOS apps locate their bundle, so a
/// stub that returned 999 (and didn't even take the right argument types)
/// caused real-world apps such as Farm Frenzy to mis-construct paths and
/// then `chdir("")` / fail every resource lookup.
fn _NSGetExecutablePath(env: &mut Environment, buf: MutPtr<u8>, bufsize: MutPtr<u32>) -> i32 {
    if bufsize.is_null() {
        return -1;
    }
    let exe_path = env.bundle.executable_path();
    let path_bytes = exe_path.as_str().as_bytes();
    // Required size includes the trailing NUL byte.
    let required: u32 = path_bytes.len() as u32 + 1;
    let provided: u32 = env.mem.read(bufsize);
    if provided < required {
        env.mem.write(bufsize, required);
        log_dbg!(
            "_NSGetExecutablePath: buffer too small ({} < {}), reporting required size",
            provided,
            required
        );
        return -1;
    }
    if buf.is_null() {
        return -1;
    }
    let dst = env.mem.bytes_at_mut(buf, required);
    dst[..path_bytes.len()].copy_from_slice(path_bytes);
    dst[path_bytes.len()] = 0;
    env.mem.write(bufsize, required);
    log_dbg!("_NSGetExecutablePath => {:?}", exe_path);
    0
}

fn kevent(
    _env: &mut Environment,
    _kq: i32,
    _changelist: ConstVoidPtr,
    _nchanges: i32,
    _eventlist: MutVoidPtr,
    _nevents: i32,
    _timeout: ConstVoidPtr,
) -> i32 {
    // FakeKevent
    0
}

fn __assert_rtn(
    env: &mut Environment,
    func: ConstPtr<u8>,
    file: ConstPtr<u8>,
    line: i32,
    expr: ConstPtr<u8>,
) {
    let func_str = read_cstr_safe(env, func);
    let file_str = read_cstr_safe(env, file);
    let expr_str = read_cstr_safe(env, expr);
    log!(
        "Assertion failed: ({}) in function {}, file {}, line {}.",
        expr_str,
        func_str,
        file_str,
        line
    );
}

fn __assert(env: &mut Environment, expr: ConstPtr<u8>, file: ConstPtr<u8>, line: i32) {
    let expr_str = read_cstr_safe(env, expr);
    let file_str = read_cstr_safe(env, file);
    log!(
        "Assertion failed: ({}) in file {}, line {}.",
        expr_str,
        file_str,
        line
    );
}

fn __assert_fail(
    env: &mut Environment,
    expr: ConstPtr<u8>,
    file: ConstPtr<u8>,
    line: u32,
    func: ConstPtr<u8>,
) {
    let expr_str = read_cstr_safe(env, expr);
    let file_str = read_cstr_safe(env, file);
    let func_str = read_cstr_safe(env, func);
    log!(
        "Assertion failed: ({}) in function {}, file {}, line {}.",
        expr_str,
        func_str,
        file_str,
        line
    );
}

fn read_cstr_safe(env: &mut Environment, ptr: ConstPtr<u8>) -> String {
    if ptr.is_null() {
        return "(null)".to_string();
    }
    // Read bytes until NUL terminator.
    let mut bytes = Vec::new();
    let mut offset = 0u32;
    loop {
        let b: u8 = env.mem.read(ptr + offset);
        if b == 0 {
            break;
        }
        bytes.push(b);
        offset += 1;
    }
    String::from_utf8(bytes).unwrap_or_else(|_| "(invalid utf-8)".to_string())
}

#[allow(non_snake_case)]
fn _fcvt(
    env: &mut Environment,
    value: f64,
    ndigits: i32,
    decpt: MutPtr<i32>,
    sign: MutPtr<i32>,
) -> MutPtr<u8> {
    set_errno(env, 0);
    let is_negative = value.is_sign_negative() && value != 0.0;
    let val_abs = value.abs();
    // Format the number with the requested fractional digits
    let ndigits_usize = ndigits.max(0) as usize;
    let formatted = format!("{:.*}", ndigits_usize, val_abs);

    let mut digits = String::with_capacity(formatted.len());
    let mut decimal_pos = formatted.len() as i32;
    let mut dot_found = false;

    // Extract decimal position and remove the dot from the string
    for (i, c) in formatted.chars().enumerate() {
        if c == '.' {
            decimal_pos = i as i32;
            dot_found = true;
        } else {
            digits.push(c);
        }
    }

    if !dot_found {
        decimal_pos = digits.len() as i32;
    }

    // Remove leading zero for numbers < 1 to match C behavior
    if digits.starts_with('0') && decimal_pos == 1 && digits.len() > 1 {
        digits.remove(0);
        decimal_pos -= 1;
    }

    if !decpt.is_null() {
        env.mem.write(decpt, decimal_pos);
    }
    if !sign.is_null() {
        env.mem.write(sign, if is_negative { 1 } else { 0 });
    }

    // Allocate in guest memory and CAST to a u8 pointer
    let buf_len = (digits.len() + 1) as GuestUSize;
    let buf: MutPtr<u8> = env.mem.alloc(buf_len).cast();

    env.mem
        .bytes_at_mut(buf, digits.len() as GuestUSize)
        .copy_from_slice(digits.as_bytes());
    env.mem.write(buf + digits.len() as GuestUSize, b'\0');
    buf
}

#[allow(non_snake_case)]
fn _gcvt(env: &mut Environment, value: f64, ndigit: i32, buf: MutPtr<u8>) -> MutPtr<u8> {
    set_errno(env, 0);
    let ndigit = ndigit.max(0) as usize;
    // В Rust нет точного аналога "g", поэтому мы используем стандартный трейт
    // Display
    // с указанием точности (количества знаков после запятой).
    let s = format!("{:.*}", ndigit, value);

    let bytes = s.as_bytes();
    let len = bytes.len() as GuestUSize;
    if !buf.is_null() {
        env.mem.bytes_at_mut(buf, len).copy_from_slice(bytes);
        env.mem.write(buf + len, b'\0');
    }

    buf
}

fn mbtowc_l(
    env: &mut Environment,
    pwc: MutPtr<u32>, // wchar_t на iOS/ARM32 — это 32-битный int
    s: ConstVoidPtr,
    n: GuestUSize,      // size_t
    _loc: ConstVoidPtr, // locale_t (игнорируем, так как используем стандартный UTF-8)
) -> i32 {
    if s.is_null() {
        return 0;
    }

    if n == 0 {
        return -1;
    }

    let s_ptr: ConstPtr<u8> = s.cast();
    let first_byte: u8 = env.mem.read(s_ptr);
    if first_byte == 0 {
        if !pwc.is_null() {
            env.mem.write(pwc, 0);
        }
        return 0;
    }

    if first_byte < 0x80 {
        if !pwc.is_null() {
            env.mem.write(pwc, first_byte as u32);
        }
        return 1;
    }

    let mut codepoint: u32;
    let bytes_to_read: u32;

    if (first_byte & 0xE0) == 0xC0 {
        codepoint = (first_byte & 0x1F) as u32;
        bytes_to_read = 1;
    } else if (first_byte & 0xF0) == 0xE0 {
        codepoint = (first_byte & 0x0F) as u32;
        bytes_to_read = 2;
    } else if (first_byte & 0xF8) == 0xF0 {
        codepoint = (first_byte & 0x07) as u32;
        bytes_to_read = 3;
    } else {
        return -1;
    }

    if n < (bytes_to_read + 1) as GuestUSize {
        return -1;
    }

    for i in 1..=bytes_to_read {
        let next_byte: u8 = env.mem.read(s_ptr + i as GuestUSize);
        if (next_byte & 0xC0) != 0x80 {
            return -1;
        }
        codepoint = (codepoint << 6) | ((next_byte & 0x3F) as u32);
    }

    if !pwc.is_null() {
        env.mem.write(pwc, codepoint);
    }

    (bytes_to_read + 1) as i32
}

fn putenv(env: &mut Environment, string: MutPtr<u8>) -> i32 {
    if string.is_null() {
        set_errno(env, EINVAL);
        return -1;
    }
    let s = match env.mem.cstr_at_utf8(string.cast_const()) {
        Ok(s) => s.to_owned(),
        Err(_) => {
            set_errno(env, EINVAL);
            return -1;
        }
    };
    log_dbg!("putenv({:?})", s);
    // putenv is a no-op in touchHLE — we have no real environment block.
    // Return 0 (success) so apps that call it to set e.g. timezone or locale
    // hints don't abort on the return code.
    0
}

#[allow(non_camel_case_types)]
#[derive(Debug)]
#[repr(C, packed)]
struct div_t {
    quot: i32,
    rem: i32,
}
unsafe impl SafeRead for div_t {}
impl_GuestRet_for_large_struct!(div_t);

fn div(_env: &mut Environment, numer: i32, denom: i32) -> div_t {
    div_t {
        quot: numer.wrapping_div(denom),
        rem: numer.wrapping_rem(denom),
    }
}

/// `ldiv_t` — return type of `ldiv`. Per the C99 standard and Apple's
/// `<stdlib.h>` (`/usr/include/stdlib.h` on macOS): two `long` fields,
/// `quot` then `rem`. On 32-bit iOS `long` is 32 bits, so the layout
/// matches `div_t`. We keep a separate Rust type so callers see the
/// correct type encoding.
#[allow(non_camel_case_types)]
#[derive(Debug)]
#[repr(C, packed)]
struct ldiv_t {
    quot: i32,
    rem: i32,
}
unsafe impl SafeRead for ldiv_t {}
impl_GuestRet_for_large_struct!(ldiv_t);

/// `ldiv_t ldiv(long numer, long denom)` — per Apple's manpage:
/// computes both the quotient and remainder of dividing `numer` by
/// `denom` in a single operation, returning the result in an `ldiv_t`
/// struct. Behaviour is identical to `div` on 32-bit iOS where `long`
/// is 32 bits wide.
fn ldiv(_env: &mut Environment, numer: i32, denom: i32) -> ldiv_t {
    ldiv_t {
        quot: numer.wrapping_div(denom),
        rem: numer.wrapping_rem(denom),
    }
}

fn setxattr(
    env: &mut Environment,
    path: ConstPtr<u8>,
    name: ConstPtr<u8>,
    _value: ConstVoidPtr,
    size: GuestUSize,
    position: u32,
    options: i32,
) -> i32 {
    let path_str = env.mem.cstr_at_utf8(path).unwrap_or_default().to_owned();
    let name_str = env.mem.cstr_at_utf8(name).unwrap_or_default().to_owned();
    log_dbg!(
        "setxattr({:?}, {:?}, size={}, position={}, options={:#x}) — ignored",
        path_str,
        name_str,
        size,
        position,
        options
    );
    // Return 0 (success). touchHLE has no extended attribute storage;
    // returning success prevents apps from treating missing xattr support
    // as a fatal error.
    0
}

fn fsetxattr(
    env: &mut Environment,
    fd: crate::libc::posix_io::FileDescriptor,
    name: ConstPtr<u8>,
    _value: ConstVoidPtr,
    size: GuestUSize,
    position: u32,
    options: i32,
) -> i32 {
    let name_str = env.mem.cstr_at_utf8(name).unwrap_or_default().to_owned();
    log_dbg!(
        "fsetxattr(fd={}, {:?}, size={}, position={}, options={:#x}) — ignored",
        fd,
        name_str,
        size,
        position,
        options
    );
    0
}

fn getxattr(
    env: &mut Environment,
    path: ConstPtr<u8>,
    name: ConstPtr<u8>,
    _value: MutVoidPtr,
    size: GuestUSize,
    position: u32,
    options: i32,
) -> i32 {
    let path_str = env.mem.cstr_at_utf8(path).unwrap_or_default().to_owned();
    let name_str = env.mem.cstr_at_utf8(name).unwrap_or_default().to_owned();
    log_dbg!(
        "getxattr({:?}, {:?}, size={}, position={}, options={:#x}) — returning ENOATTR",
        path_str,
        name_str,
        size,
        position,
        options
    );
    // ENOATTR = 93 on Darwin. Return -1 and set errno.
    set_errno(env, 93);
    -1
}

fn fgetxattr(
    env: &mut Environment,
    fd: crate::libc::posix_io::FileDescriptor,
    name: ConstPtr<u8>,
    _value: MutVoidPtr,
    size: GuestUSize,
    position: u32,
    options: i32,
) -> i32 {
    let name_str = env.mem.cstr_at_utf8(name).unwrap_or_default().to_owned();
    log_dbg!(
        "fgetxattr(fd={}, {:?}, size={}, position={}, options={:#x}) — returning ENOATTR",
        fd,
        name_str,
        size,
        position,
        options
    );
    set_errno(env, 93);
    -1
}

fn removexattr(env: &mut Environment, path: ConstPtr<u8>, name: ConstPtr<u8>, options: i32) -> i32 {
    let path_str = env.mem.cstr_at_utf8(path).unwrap_or_default().to_owned();
    let name_str = env.mem.cstr_at_utf8(name).unwrap_or_default().to_owned();
    log_dbg!(
        "removexattr({:?}, {:?}, options={:#x}) — returning ENOATTR",
        path_str,
        name_str,
        options
    );
    set_errno(env, 93);
    -1
}

fn fremovexattr(
    env: &mut Environment,
    fd: crate::libc::posix_io::FileDescriptor,
    name: ConstPtr<u8>,
    options: i32,
) -> i32 {
    let name_str = env.mem.cstr_at_utf8(name).unwrap_or_default().to_owned();
    log_dbg!(
        "fremovexattr(fd={}, {:?}, options={:#x}) — returning ENOATTR",
        fd,
        name_str,
        options
    );
    set_errno(env, 93);
    -1
}

fn listxattr(
    env: &mut Environment,
    path: ConstPtr<u8>,
    namebuf: MutPtr<u8>,
    size: GuestUSize,
    options: i32,
) -> i32 {
    let path_str = env.mem.cstr_at_utf8(path).unwrap_or_default().to_owned();
    log_dbg!(
        "listxattr({:?}, size={}, options={:#x}) — returning 0 (empty list)",
        path_str,
        size,
        options
    );
    // Zero-length list means no attributes. Return 0 (success, 0 bytes needed).
    if !namebuf.is_null() && size > 0 {
        env.mem.write(namebuf, 0u8);
    }
    0
}

fn flistxattr(
    env: &mut Environment,
    fd: crate::libc::posix_io::FileDescriptor,
    namebuf: MutPtr<u8>,
    size: GuestUSize,
    options: i32,
) -> i32 {
    log_dbg!(
        "flistxattr(fd={}, size={}, options={:#x}) — returning 0 (empty list)",
        fd,
        size,
        options
    );
    if !namebuf.is_null() && size > 0 {
        env.mem.write(namebuf, 0u8);
    }
    0
}

// ===========================================================================
// MARK: - zlib supplemental
// ===========================================================================

/// `int inflateReset2(z_streamp strm, int windowBits)`
///
/// Per the [zlib manual](https://www.zlib.net/manual.html): "This function is
/// equivalent to inflateEnd followed by inflateInit2, but does not free and
/// reallocate the internal decompression state. The stream will keep attributes
/// that may have been set by inflateInit2." It was added in zlib 1.2.3.4.
///
/// The bundled libz.1.2.3.dylib does not export this symbol, so apps that link
/// against a newer SDK (e.g. Flappy Bird built for iOS 7) fail at lazy-bind
/// time. We provide a minimal host implementation that calls through to the
/// guest's `inflateReset` (same as calling inflateReset on the stream — window
/// bits are stored in the stream structure anyway from the initial inflateInit2
/// call).
///
/// Return value: Z_OK (0) on success.
fn inflateReset2(
    _env: &mut Environment,
    _strm: MutVoidPtr,
    _window_bits: i32,
) -> i32 {
    // Z_OK = 0. We cannot easily call back into the guest's inflateReset
    // from host code without the full z_stream layout. However, the most
    // common pattern is that inflateReset2 is called right after inflateInit2
    // (which already set window bits) or before any actual inflate call.
    // Returning Z_OK lets the app proceed — the stream state was already
    // initialized by the guest's inflateInit2 which IS in the old libz.
    log_dbg!("inflateReset2(strm={:?}, windowBits={}) -> Z_OK (stubbed)", _strm, _window_bits);
    0 // Z_OK
}

pub const FUNCTIONS: FunctionExports = &[
    export_c_func!(malloc(_)),
    export_c_func!(malloc_size(_)),
    export_c_func!(calloc(_, _)),
    export_c_func!(realloc(_, _)),
    export_c_func!(reallocf(_, _)),
    export_c_func!(free(_)),
    export_c_func!(posix_memalign(_, _, _)),
    export_c_func!(valloc(_)),
    export_c_func!(atexit(_)),
    export_c_func!(atoi(_)),
    export_c_func!(atol(_)),
    export_c_func!(atof(_)),
    export_c_func!(strtod(_, _)),
    export_c_func!(srand(_)),
    export_c_func!(sranddev()),
    export_c_func!(rand()),
    export_c_func!(rand_r(_)),
    export_c_func!(srandom(_)),
    export_c_func!(random()),
    export_c_func!(arc4random()),
    export_c_func!(arc4random_uniform(_)),
    export_c_func!(arc4random_stir()),
    export_c_func!(arc4random_addrandom()),
    // drand48 family (POSIX / Apple iPhone OS manpage)
    export_c_func!(drand48()),
    export_c_func!(erand48(_)),
    export_c_func!(lrand48()),
    export_c_func!(nrand48(_)),
    export_c_func!(mrand48()),
    export_c_func!(jrand48(_)),
    export_c_func!(srand48(_)),
    export_c_func!(seed48(_)),
    export_c_func!(lcong48(_)),
    export_c_func!(mkstemp(_)),
    export_c_func!(mkdtemp(_)),
    export_c_func!(mktemp(_)),
    export_c_func!(getenv(_)),
    export_c_func!(setenv(_, _, _)),
    // <--- ИСПРАВЛЕНИЕ НА 3 АРГУМЕНТА ГОСТЯ
    export_c_func!(unsetenv(_)),
    export_c_func!(exit(_)),
    export_c_func!(abort()),
    export_c_func_aliased!("_abort", abort()),
    export_c_func!(bsearch(_, _, _, _, _)),
    export_c_func!(strtof(_, _)),
    export_c_func!(strtoul(_, _, _)),
    export_c_func!(strtoull(_, _, _)),
    export_c_func!(strtoll(_, _, _)),
    export_c_func_aliased!("strtoq", strtoll(_, _, _)),
    export_c_func_aliased!("strtouq", strtoull(_, _, _)),
    export_c_func!(strtol(_, _, _)),
    export_c_func!(realpath(_, _)),
    export_c_func_aliased!("realpath$DARWIN_EXTSN", realpath(_, _)),
    // mbstowcs and wcstombs are exported from libc::wchar; not duplicated here.
    export_c_func!(NSZoneMalloc(_, _)),
    export_c_func!(NSZoneFree(_, _)),
    export_c_func!(NSZoneRealloc(_, _, _)),
    export_c_func!(__assert_rtn(_, _, _, _)),
    export_c_func!(__assert(_, _, _)),
    export_c_func!(__assert_fail(_, _, _, _)),
    export_c_func!(_fcvt(_, _, _, _)),
    export_c_func!(_gcvt(_, _, _)),
    export_c_func!(system(_)),
    export_c_func!(dladdr(_, _)),
    export_c_func!(kqueue()),
    export_c_func!(_NSGetExecutablePath(_, _)),
    export_c_func!(kevent(_, _, _, _, _, _)),
    export_c_func!(mbtowc_l(_, _, _, _)), // ОШИБКА БЫЛА ЗДЕСЬ (4 подчеркивания вместо 5)
    export_c_func!(putenv(_)),
    export_c_func!(div(_, _)),
    export_c_func!(ldiv(_, _)),
    export_c_func!(setxattr(_, _, _, _, _, _)),
    export_c_func!(fsetxattr(_, _, _, _, _, _)),
    export_c_func!(getxattr(_, _, _, _, _, _)),
    export_c_func!(fgetxattr(_, _, _, _, _, _)),
    export_c_func!(removexattr(_, _, _)),
    export_c_func!(fremovexattr(_, _, _)),
    export_c_func!(listxattr(_, _, _, _)),
    export_c_func!(flistxattr(_, _, _, _)),
    // zlib supplemental — inflateReset2 was added in zlib 1.2.3.4 but the
    // bundled libz.1.2.3.dylib doesn't have it. Some apps (Flappy Bird)
    // import it.
    export_c_func!(inflateReset2(_, _)),
];

pub fn atof_inner(
    env: &mut Environment,
    s: ConstPtr<u8>,
) -> Result<(f64, u32), <f64 as FromStr>::Err> {
    atof_inner_generic(
        env,
        |env, s, idx| Ok(env.mem.read(s + idx)),
        |_, _, _| (),
        s.cast_mut(),
        0,
    )
}

#[allow(rustdoc::broken_intra_doc_links)]
pub fn atof_inner_generic<
    T,
    U,
    F1: Fn(&mut Environment, MutPtr<U>, GuestUSize) -> Result<T, ()>,
    F2: Fn(&mut Environment, MutPtr<U>, u8),
>(
    env: &mut Environment,
    getc_fn: F1,
    ungetc_fn: F2,
    subject: MutPtr<U>,
    offset: GuestUSize,
) -> Result<(f64, u32), <f64 as FromStr>::Err>
where
    u8: From<T>,
{
    let mut whitespace_len = 0;
    let mut len = 0;
    let mut chars = Vec::new();
    let _ = || -> Result<(), ()> {
        match count_whitespace_generic(env, &getc_fn, &ungetc_fn, subject, offset) {
            Ok(count) => whitespace_len = count,
            Err(count) => {
                whitespace_len = count;
                return Err(());
            }
        }
        let maybe_sign: u8 = getc_fn(env, subject, offset + whitespace_len + len)?.into();
        if maybe_sign == b'+' || maybe_sign == b'-' || maybe_sign.is_ascii_digit() {
            chars.push(maybe_sign);
            len += 1;
        } else {
            ungetc_fn(env, subject, maybe_sign);
        }
        let mut curr: u8 = getc_fn(env, subject, offset + whitespace_len + len)?.into();
        while (curr as char).is_ascii_digit() {
            chars.push(curr);
            len += 1;
            curr = getc_fn(env, subject, offset + whitespace_len + len)?.into();
        }
        if curr == b'.' {
            chars.push(curr);
            len += 1;
            curr = getc_fn(env, subject, offset + whitespace_len + len)?.into();
            while (curr as char).is_ascii_digit() {
                chars.push(curr);
                len += 1;
                curr = getc_fn(env, subject, offset + whitespace_len + len)?.into();
            }
        }
        if curr.eq_ignore_ascii_case(&b'e') {
            chars.push(curr);
            len += 1;
            let maybe_sign: u8 = getc_fn(env, subject, offset + whitespace_len + len)?.into();
            if maybe_sign == b'+' || maybe_sign == b'-' || maybe_sign.is_ascii_digit() {
                chars.push(maybe_sign);
                len += 1;
            } else {
                ungetc_fn(env, subject, maybe_sign);
            }
            curr = getc_fn(env, subject, offset + whitespace_len + len)?.into();
            while (curr as char).is_ascii_digit() {
                chars.push(curr);
                len += 1;
                curr = getc_fn(env, subject, offset + whitespace_len + len)?.into();
            }
        }
        ungetc_fn(env, subject, curr);
        assert_eq!(chars.len() as u32, len);
        Ok(())
    }();

    let s = std::str::from_utf8(&chars).unwrap();
    log_dbg!("atof_inner_generic('{}')", s);
    s.parse().map(|result| (result, whitespace_len + len))
}

fn strtol_inner(env: &mut Environment, str: ConstPtr<u8>, base: u32) -> Result<(i32, u32), ()> {
    str_to_int_inner_generic(
        env,
        |env, s, idx| Ok(env.mem.read(s + idx)),
        |_, _, _| (),
        str.cast_mut(),
        0,
        base,
        u32::MAX,
        |s, base| i32::from_str_radix(s, base).unwrap_or(i32::MAX),
        |num| num.checked_mul(-1).unwrap_or(i32::MIN),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn str_to_int_inner_generic<T, U, Q, F1, F2, F3, F4>(
    env: &mut Environment,
    getc_fn: F1,
    ungetc_fn: F2,
    subject: MutPtr<U>,
    offset: GuestUSize,
    mut base: u32,
    max_length: GuestUSize,
    from_str_radix_fn: F3,
    negation_fn: F4,
) -> Result<(Q, u32), ()>
where
    u8: From<T>,
    Q: Default,
    F1: Fn(&mut Environment, MutPtr<U>, GuestUSize) -> Result<T, ()>,
    F2: Fn(&mut Environment, MutPtr<U>, u8),
    F3: Fn(&str, u32) -> Q,
    F4: Fn(Q) -> Q,
{
    let mut whitespace_len = 0;
    let mut len = 0;
    let mut sign = None;
    let mut prefix_length = 0;
    let mut chars = Vec::new();
    let _ = || -> Result<(), ()> {
        match count_whitespace_generic(env, &getc_fn, &ungetc_fn, subject, offset) {
            Ok(count) => whitespace_len = count,
            Err(count) => {
                whitespace_len = count;
                return Err(());
            }
        }
        let maybe_sign: u8 = getc_fn(env, subject, offset + whitespace_len + len)?.into();
        if maybe_sign == b'+' || maybe_sign == b'-' {
            sign = Some(maybe_sign);
            prefix_length += 1;
            len += 1;
            if len == max_length {
                return Ok(());
            }
        } else {
            ungetc_fn(env, subject, maybe_sign);
        }
        if base == 0 {
            let curr: u8 = getc_fn(env, subject, offset + whitespace_len + len)?.into();
            base = if curr == b'0' {
                let next: u8 = getc_fn(env, subject, offset + whitespace_len + len + 1)?.into();
                ungetc_fn(env, subject, next);
                ungetc_fn(env, subject, curr);
                if next == b'x' || next == b'X' {
                    16
                } else {
                    8
                }
            } else {
                ungetc_fn(env, subject, curr);
                10
            }
        }
        if base == 8 || base == 16 {
            let curr: u8 = getc_fn(env, subject, offset + whitespace_len + len)?.into();
            if curr == b'0' {
                len += 1;
                if len == max_length {
                    return Ok(());
                }
                prefix_length += 1;
                if base == 16 {
                    let next: u8 = getc_fn(env, subject, offset + whitespace_len + len)?.into();
                    if next == b'x' || next == b'X' {
                        len += 1;
                        if len == max_length {
                            return Ok(());
                        }
                        prefix_length += 1;
                    } else {
                        ungetc_fn(env, subject, next);
                    }
                } else {
                    ungetc_fn(env, subject, curr);
                }
            } else {
                ungetc_fn(env, subject, curr);
            }
        }
        let mut curr: u8 = getc_fn(env, subject, offset + whitespace_len + len)?.into();
        while (curr as char).is_digit(base) {
            chars.push(curr);
            len += 1;
            if len == max_length {
                return Ok(());
            }
            curr = getc_fn(env, subject, offset + whitespace_len + len)?.into();
        }
        ungetc_fn(env, subject, curr);
        assert_eq!(chars.len() as u32, len - prefix_length);
        Ok(())
    }();

    let s = std::str::from_utf8(&chars).unwrap();
    log_dbg!("strtol_inner_generic('{}', {})", s, base);

    assert!((2..=36).contains(&base));
    let magnitude_len = len - prefix_length;
    let res = if magnitude_len > 0 {
        let mut res = from_str_radix_fn(s, base);
        if sign == Some(b'-') {
            res = negation_fn(res);
        }
        res
    } else {
        if base == 8 && prefix_length > 0 {
            return Ok((Q::default(), whitespace_len + prefix_length));
        }
        return Err(());
    };
    Ok((res, whitespace_len + len))
}
