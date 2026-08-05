/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! The core of the emulator: management of state, execution, threading.
//!
//! Unlike its siblings, this module should be considered private and only used
//! via the re-exports one level up.

pub mod app_picker;
mod mutex;
mod nullable_box;

use crate::abi::{CallFromHost, GuestFunction};
use crate::audio::openal::OpenALManager;
use crate::cpu::Cpu;
use crate::libc::semaphore::sem_t;
use crate::mem::{GuestUSize, MutPtr, MutVoidPtr};
use crate::{
    abi, bundle, cpu, dyld, frameworks, fs, gdb, image, libc, mach_o, mem, objc, options, stack,
    window,
};
use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::TcpListener;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use crate::libc::pthread::cond::pthread_cond_t;
use crate::window::DeviceFamily;
use corosensei::{Coroutine, Yielder};
pub use mutex::{MutexId, MutexType, PTHREAD_MUTEX_DEFAULT};
use nullable_box::NullableBox;

/// Index into the [Vec] of threads. Thread 0 is always the main thread.
pub type ThreadId = usize;

pub type HostContext = Coroutine<Environment, Environment, Environment>;

/// Bookkeeping for a thread.
pub struct Thread {
    /// Once a thread finishes, this is set to false.
    pub active: bool,
    /// If this is not [ThreadBlock::NotBlocked], the thread is not executing
    /// until a certain condition is fufilled.
    pub blocked_by: ThreadBlock,
    /// Container for thread local state of various child modules
    pub thread_local_framework_state: frameworks::ThreadLocalState,
    /// After a secondary thread finishes, this is set to the returned value.
    return_value: Option<MutVoidPtr>,
    /// Context object containing the CPU state for this thread.
    ///
    /// There should always be `(threads.len() - 1)` contexts in existence.
    /// When a thread is currently executing, its state is stored directly in
    /// the CPU, rather than in a context object. In that case, this field is
    /// None. See also: [std::mem::take] and [cpu::Cpu::swap_context].
    pub guest_context: Option<Box<cpu::CpuContext>>,
    /// The coroutine associated with this thread.
    ///
    /// In more typical rust, this is equivalent to to a [std::future::Future].
    /// Like a [std::future::Future], it holds the call stack so the inner
    /// function can (cooperatively) suspend execution and be resumed at a
    /// later time. Unlike a [std::future::Future], the call stack is actually
    /// stored as a stack, and not as an anonymous, compiler generated,
    /// (typically heap allocated) object.
    host_context: Option<HostContext>,
    /// Address range of this thread's stack, used to check if addresses are in
    /// range while producing a stack trace.
    pub stack: Option<std::ops::RangeInclusive<u32>>,
}

impl Thread {
    fn is_blocked(&self) -> bool {
        !matches!(self.blocked_by, ThreadBlock::NotBlocked)
    }
}

impl std::fmt::Debug for Thread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Thread {{ active: {:?}, blocked_by: {:?}, return_value: {:?} }}",
            self.active, self.blocked_by, self.return_value
        )
    }
}

/// The struct containing the entire emulator state. Methods are provided for
/// execution and management of threads.
pub struct Environment {
    /// Reference point for various timing functions.
    pub startup_time: Instant,
    pub bundle: NullableBox<bundle::Bundle>,
    pub fs: NullableBox<fs::Fs>,
    /// The window is only absent when running in headless mode.
    pub window: Option<Box<window::Window>>,
    pub openal_manager: NullableBox<OpenALManager>,
    pub mem: NullableBox<mem::Mem>,
    /// Loaded binaries. Index `0` is always the app binary, other entries are
    /// dynamic libraries.
    pub bins: Vec<mach_o::MachO>,
    pub objc: NullableBox<objc::ObjC>,
    pub dyld: NullableBox<dyld::Dyld>,
    pub cpu: NullableBox<cpu::Cpu>,
    pub current_thread: ThreadId,
    pub threads: Vec<Thread>,
    pub libc_state: NullableBox<libc::State>,
    pub framework_state: NullableBox<frameworks::State>,
    pub mutex_state: NullableBox<mutex::MutexState>,
    pub options: NullableBox<options::Options>,
    gdb_server: Option<Box<gdb::GdbServer>>,
    pub env_vars: HashMap<Vec<u8>, MutPtr<u8>>,
    /// Set to [true] when created using [Environment::new_without_app].
    pub dump_file: Option<std::fs::File>,
    pub is_app_picker: bool,
    yielder: *const Yielder<Environment, Environment>,
    // The amount of ticks to run for Some(value), or single-stepping for None.
    // Sadly, setting ticks to 1 does not step properly, so Option is required.
    remaining_ticks: Option<u64>,
    panic_cell: Rc<Cell<Option<Environment>>>,
    /// Tracks repeated UndefinedInstruction bypasses. See `debug_cpu_error`.
    udf_bypass_last: Option<(u32, u32)>,
    udf_bypass_count: u32,
}

/// What to do next when executing this thread.
enum ThreadNextAction {
    /// Continue CPU emulation.
    Continue,
    /// Return to host.
    ReturnToHost,
    /// Debug the current CPU error.
    DebugCpuError(cpu::CpuError),
}

/// If/what a thread is blocked by.
#[derive(Debug, Clone, PartialEq)]
pub enum ThreadBlock {
    // Default state. (thread is not blocked)
    NotBlocked,
    // Thread is sleeping. (until Instant)
    Sleeping(Instant),
    // Thread is waiting for a mutex to unlock.
    Mutex(MutexId),
    // Thread is waiting on a semaphore.
    Semaphore(MutPtr<sem_t>),
    // Thread is waiting on a condition variable
    Condition(MutPtr<pthread_cond_t>, Option<Duration>),
    // Thread is waiting for another thread to finish (joining).
    Joining(ThreadId, MutPtr<MutVoidPtr>),
    // Thread has hit a cpu error, and is waiting to be debugged.
    WaitingForDebugger(Option<cpu::CpuError>),
    // Thread is suspended. We keep a suspend count and a previous thread state
    // (boxed to avoid cyclic dependency), which would be restored upon
    // resuming.
    #[allow(dead_code)]
    Suspended(usize, Box<ThreadBlock>),
}

struct BinaryDependencyNode {
    name: String,
    dependencies: Vec<String>,
}

/// Topologically sorts the binary dylibs using Kahn's algorithm
/// and returns the sorted list of indices
fn generate_binary_load_order(graph: &[BinaryDependencyNode]) -> Result<Vec<usize>, String> {
    let node_to_index: HashMap<_, _> = graph
        .iter()
        .enumerate()
        .map(|(idx, node)| (node.name.as_str(), idx))
        .collect();
    let mut node_dependents = HashMap::new();
    let mut node_in_degrees: HashMap<_, _> = node_to_index.values().map(|&idx| (idx, 0)).collect();

    for node in graph {
        let &bin_index = node_to_index
            .get(node.name.as_str())
            .ok_or_else(|| format!("Failed to find {:?} name mapping", &node.name))?;

        // Bin names dont include prefix while dynamic lib paths do
        for dependency in node
            .dependencies
            .iter()
            .map(|path| path.strip_prefix("/usr/lib/").unwrap_or(path.as_str()))
        {
            // Ignore dependencies that are not included in packaged dylibs
            let Some(&dylib_index) = node_to_index.get(dependency) else {
                continue;
            };

            node_dependents
                .entry(dylib_index)
                .or_insert_with(Vec::new)
                .push(bin_index);

            node_in_degrees
                .entry(bin_index)
                .and_modify(|in_degree| *in_degree += 1);
        }
    }

    let mut leaf_nodes: VecDeque<_> = node_in_degrees
        .iter()
        .filter(|(_, &in_degree)| in_degree == 0)
        .map(|(&node, _)| node)
        .collect();

    let mut sorted_indices = Vec::new();

    while let Some(node) = leaf_nodes.pop_front() {
        sorted_indices.push(node);

        let Some(dependents) = node_dependents.get(&node) else {
            continue;
        };

        for &dependant in dependents {
            let Some(in_degree) = node_in_degrees.get_mut(&dependant) else {
                continue;
            };
            *in_degree -= 1;

            if *in_degree == 0 {
                leaf_nodes.push_back(dependant);
            }
        }
    }

    if let Some((&index, _)) = node_in_degrees.iter().find(|(_, &in_degree)| in_degree > 0) {
        return Err(format!(
            "Failed to sort nodes, cycle with {:?}",
            graph.get(index).unwrap().name
        ));
    }

    log!(
        "Found sorted order {:?}",
        sorted_indices
            .iter()
            .map(|&index| graph.get(index).unwrap().name.as_str())
            .collect::<Vec<_>>()
    );

    Ok(sorted_indices)
}

/// Enforces the one (real) Environment limit. See
/// [Environment::with_yielder] for why this is needed.
static ENVIRONMENT_INSTANCE_EXISTS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

impl Environment {
    /// Loads the binary and sets up the emulator.
    pub fn new(
        bundle: bundle::Bundle,
        fs: fs::Fs,
        mut options: options::Options,
        app_args: Vec<String>,
    ) -> Result<Environment, String> {
        let startup_time = Instant::now();
        let launched_bundle_id = bundle.bundle_identifier().to_owned();

        if launched_bundle_id == "at.source.potato.full" {
            log!(
        "Applying PotatoGold compatibility profile: disable present rotation, remap touch location to landscape, fake network success, and use silent OpenAL fallback."
    );

            // SAFETY: Environment::new runs during startup before guest worker threads
            // are created. These env vars are read by compatibility shims inside this
            // same process.
            unsafe {
                std::env::set_var("TOUCHHLE_DISABLE_PRESENT_ROTATION", "1");
                std::env::set_var("TOUCHHLE_TOUCH_LOCATION_PORTRAIT_TO_LANDSCAPE", "1");
                std::env::set_var("TOUCHHLE_FAKE_NETWORK_SUCCESS", "1");

                // PotatoGold's audio path was crashing on some Linux setups unless
                // OpenAL Soft used the null backend. This keeps the app playable even
                // if sound is silent.
            }
        }
        // Enforces the one (real) Environment limit. See `with_yielder` for
        // why this is needed.
        if ENVIRONMENT_INSTANCE_EXISTS.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Err("Only one (real) Environment can exist at a time!".to_string());
        }

        // Certain apps need to launch in a non-portrait orientation, and this
        // should be handled before creating the window because handling of
        // window rotation after-the-fact is somewhat glitchy.
        // This also ensures the splash screen is correctly oriented.
        //
        // Only force a non-portrait orientation when the app explicitly
        // does NOT advertise portrait support. Storyboard apps (and any
        // other modern UIKit binary) routinely declare every orientation
        // they can run in via `UISupportedInterfaceOrientations`, and
        // picking the first non-portrait entry would force them into
        // landscape even when portrait is perfectly fine. Apple's own
        // launch logic uses portrait by default whenever it's listed, so
        // mirror that.
        let portrait_supported = bundle
            .supported_interface_orientations()
            .contains(&"UIInterfaceOrientationPortrait");
        if options.initial_orientation == window::DeviceOrientation::Portrait && !portrait_supported
        {
            if let Some(&non_portrait_orientation) = bundle
                .supported_interface_orientations()
                .iter()
                .find(|&&o| o != "UIInterfaceOrientationPortrait")
            {
                // TODO: Overwriting the options might not be ideal; do we need
                //       to distinguish this kind of orientation change from
                //       others?
                options.initial_orientation = match non_portrait_orientation {
                    // UIInterfaceOrientation values are flipped relative to
                    // (UI)DeviceOrientation values (content has to rotate in
                    // the opposite direction to how the device rotates).
                    "UIInterfaceOrientationLandscapeLeft" => {
                        window::DeviceOrientation::LandscapeRight
                    }
                    "UIInterfaceOrientationLandscapeRight" => {
                        window::DeviceOrientation::LandscapeLeft
                    }
                    // This appears to be an older way set the orientation.
                    // From testing, it seems to correspond to left.
                    "UIInterfaceOrientationLandscape" => window::DeviceOrientation::LandscapeLeft,

                    // ДОБАВЛЯЕМ СЮДА ПРИВЯЗКУ К ОБЫЧНОМУ ПОРТРЕТУ:
                    "UIInterfaceOrientationPortraitUpsideDown" => {
                        window::DeviceOrientation::Portrait
                    }

                    other => {
                        log!(
                            "Warning: Unsupported startup orientation: {:?}; defaulting to Portrait.",
                            other
                        );
                        window::DeviceOrientation::Portrait
                    }
                };
                log!("App needs non-portrait user interface orientation {:?}, applying device orientation {:?}.", non_portrait_orientation, options.initial_orientation);
            }
        }

        let device_family_override = options.device_family;
        // `--device-family=auto`: when the user hasn't pinned a specific family,
        // probe the host display and pick the closest-matching emulated device.
        // This is treated exactly like an explicit override below, so it still
        // respects what the app bundle actually supports.
        let device_family_override = if device_family_override.is_none()
            && options.auto_device_family
            && !options.headless
        {
            match window::host_screen_size() {
                Some((w, h)) => {
                    let picked = DeviceFamily::pick_for_screen(w, h);
                    if options.host_screen_size.is_none() {
                        options.host_screen_size = Some((w, h));
                    }
                    log!(
                        "Auto device family: host screen is {}x{} px, exposing the same resolution to the app and picking closest match {:?}.",
                        w,
                        h,
                        picked
                    );
                    Some(picked)
                }
                None => {
                    log!("Auto device family: couldn't determine host screen size; leaving choice to the app bundle.");
                    None
                }
            }
        } else {
            device_family_override
        };
        let device_family_array = bundle.device_family_array();
        // The bundle only declares generic device *classes* (iPhone == phone
        // family, iPad == tablet family). A user override may now name a
        // specific model (e.g. iPhone 4s, iPad mini 2). We accept the override
        // when its class matches one the bundle supports, and otherwise fall
        // back to a sensible default model for a supported class.
        let bundle_supports_ipad = device_family_array.iter().any(|f| f.is_ipad());
        let bundle_supports_phone = device_family_array.iter().any(|f| !f.is_ipad());
        // Default model picked for each class when the user hasn't chosen one.
        // iPhone 3GS (iPhone2,1) is the historical touchHLE phone default
        // (320x480, GLES2-capable); iPad 2 (iPad2,1) is the tablet default.
        let default_phone = DeviceFamily::iPhone3GS;
        let default_ipad = DeviceFamily::iPad2;

        let device_family = if let Some(dfo) = device_family_override {
            let override_is_ipad = dfo.is_ipad();
            if override_is_ipad && bundle_supports_ipad {
                dfo
            } else if !override_is_ipad && bundle_supports_phone {
                dfo
            } else {
                log!(
                    "Warning: User-defined {:?} device family override is not supported by the app (supported: {:?}); ignoring.",
                    dfo,
                    device_family_array
                );
                if bundle_supports_phone {
                    default_phone
                } else if bundle_supports_ipad {
                    default_ipad
                } else {
                    default_phone
                }
            }
        } else if bundle_supports_phone {
            // Prefer the phone family when the bundle supports it, matching the
            // previous behaviour for universal (iPhone + iPad) bundles.
            default_phone
        } else if bundle_supports_ipad {
            default_ipad
        } else {
            log!(
                "Warning: bundle declares no recognised supported device families ({:?}); falling back to iPhone.",
                device_family_array
            );
            default_phone
        };
        log!("{:?} device family is chosen.", device_family);
        options.device_family = Some(device_family);

        let window = if options.headless {
            None
        } else {
            let icon = bundle.load_icon(&fs);
            if let Err(ref e) = icon {
                log!("Warning: {}", e);
            }

            let launch_image_path = bundle.launch_image_path(&fs, device_family);
            let launch_image = if fs.is_file(&launch_image_path) {
                let res = fs
                    .read(launch_image_path)
                    .map_err(|_| "Could not read launch image file".to_string())
                    .and_then(|bytes| {
                        image::Image::from_bytes(&bytes)
                            .map_err(|e| format!("Could not parse launch image: {e}"))
                    });
                if let Err(ref e) = res {
                    log!("Warning: {}", e);
                };
                res.ok()
            } else {
                None
            };
            Some(Box::new(window::Window::new(
                &format!(
                    "{} (touchHLE {}{}{})",
                    bundle.display_name(),
                    super::branding(),
                    if super::branding().is_empty() {
                        ""
                    } else {
                        " "
                    },
                    super::VERSION
                ),
                icon.ok(),
                launch_image.map(|image| (image, false)),
                &options,
            )))
        };

        let mut mem = mem::Mem::new();

        let is_spore = bundle.bundle_identifier().starts_with("com.ea.spore");
        let is_critter_crunch = bundle
            .bundle_identifier()
            .starts_with("com.capybaragames.CritterCrunch")
            || bundle
                .bundle_identifier()
                .starts_with("com.go.starwave.CritterCrunch");
        // We always reset this flag depending on which game is launched.
        mem.zero_memory_on_free = !is_spore && !is_critter_crunch;
        if is_spore {
            log!("Applying game-specific hack for Spore Origins: zeroing memory on alloc instead of free.");
        }
        if is_critter_crunch {
            // Without this hack, every time a critter 'explodes',
            // the game crashes with a null page access error.
            log!("Applying game-specific hack for Critter Crunch: zeroing memory on alloc instead of free.");
        }
        let executable = mach_o::MachO::load_from_file(
            bundle.executable_path(),
            &fs,
            &mut mem,
            /* slide: */ 0,
        )
        .map_err(|e| format!("Could not load executable: {e}"))?;

        let mut dylibs = Vec::new();
        for dylib in &executable.dynamic_libraries {
            // There are some Free Software libraries bundled with touchHLE and
            // exposed via the guest file system (see Fs::new()).
            let dylib_path = fs::GuestPath::new(dylib);
            if fs.is_file(dylib_path) {
                // We use hardcoded slide values for libgcc and libstdc++
                // based on base addresses of those dylibs prior to iOS 3.1
                // TODO: implement some kind of ASLR instead of hardcoding
                assert!(dylib_path.as_str().starts_with("/usr/lib/"));

                let name = dylib_path.file_name().unwrap();
                let dylib_slide = match name {
                    "libstdc++.6.dylib" | "libstdc++.6.0.9.dylib" => 0x3748a000,

                    // ДОБАВИТЬ ЭТО: Честный базовый адрес для libc++ (iOS 5.0+)
                    "libc++.1.dylib" => 0x38000000,
                    // На случай, если игра также потянет за собой libc++abi
                    "libc++abi.dylib" => 0x38100000,
                    "libiconv.2.dylib" => 0x32000000,

                    "libgcc_s.1.dylib" => 0x30000000,
                    "libz.1.dylib" | "libz.1.2.3.dylib" | "libz.dylib" | "libz.1.1.3.dylib" => {
                        // We build `libz` from sources with our OSS toolchain,
                        // the base address is already set and sliding is not
                        // needed.
                        0
                    }
                    "libsqlite3.dylib" | "libsqlite3.0.dylib" => {
                        // We build `libsqlite3` from sources with our OSS
                        // toolchain, the base address is already set and
                        // sliding is not needed.
                        0
                    }
                    _ => {
                        log!(
                            "Warning: unknown binary slide for {:?}; loading at slide 0. App may fail to bind some symbols.",
                            name
                        );
                        0
                    }
                };

                let dylib = mach_o::MachO::load_from_file(
                    fs::GuestPath::new(dylib),
                    &fs,
                    &mut mem,
                    dylib_slide,
                )
                .map_err(|e| format!("Could not load bundled dylib: {e}"))?;

                dylibs.push(dylib);
            // Otherwise, look for it in our host implementations.
            } else if !crate::dyld::DYLIB_LIST
                .iter()
                .any(|d| d.path == dylib || d.aliases.contains(&dylib.as_str()))
            {
                log!(
                    "Warning: app binary depends on unimplemented or missing dylib \"{}\"",
                    dylib
                );
            }
        }

        let entry_point_addr = executable
            .entry_point_pc
            .ok_or_else(|| {
                "Mach-O file does not specify an entry point PC, perhaps it is not an executable?"
                    .to_string()
            })
            .unwrap();

        let entry_point_is_lc_main = executable.entry_point_is_lc_main;

        let entry_point_addr = abi::GuestFunction::from_addr_with_thumb_bit(entry_point_addr);

        log_dbg!("Address of start function: {:?}", entry_point_addr);

        let mut bins = dylibs;
        bins.insert(0, executable);

        let mut objc = objc::ObjC::new();

        let mut dyld = dyld::Dyld::new();
        dyld.do_initial_linking(&bundle, &bins, &mut mem, &mut objc);

        let cpu = cpu::Cpu::new(match options.direct_memory_access {
            true => Some(&mut mem),
            false => None,
        });

        let main_thread_init_routine = Coroutine::new(move |yielder, mut env: Environment| {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                env.with_yielder(yielder, move |env| {
                    echo!("CPU emulation begins now.");
                    // Some apps use the stack inside the static initializer.
                    // While properly behaving apps should be fine, some app
                    // will try to poke the top of the stack, so we'll give
                    // it some room.
                    env.cpu.regs_mut()[Cpu::SP] = 0xFFFFF000;

                    // Call `+load` method on classes where it's defined.
                    // TODO: `+load` methods from our image should take priority
                    // over frameworks ones.
                    // TODO: a category `+load` method should be called after
                    // the class's own +load method.
                    // Note: `+load` is sent without triggering `+initialize`,
                    // matching the runtime's guarantee that `+load` runs first.
                    let mut to_be_loaded = Vec::new();
                    let mut processed = HashSet::new();
                    let load_sel: objc::SEL = env
                        .objc
                        .register_host_selector("load".to_string(), &mut env.mem);
                    for (class_name, &class) in env.objc.all_classes() {
                        if processed.contains(&class) {
                            continue;
                        }
                        if env.objc.is_unimplemented_class(class) || env.objc.is_fake_class(class) {
                            continue;
                        }
                        if env
                            .objc
                            .object_has_uninherited_method(&env.mem, class, load_sel)
                        {
                            log_dbg!("Calling +load on inheritance chain of {} class", class_name);
                            let mut inherited = Vec::new();
                            let mut curr_class = class;
                            while curr_class != objc::nil
                                && !env.objc.is_unimplemented_class(curr_class)
                                && !env.objc.is_fake_class(curr_class)
                            {
                                if !processed.contains(&curr_class)
                                    && env.objc.object_has_uninherited_method(
                                        &env.mem, curr_class, load_sel,
                                    )
                                {
                                    inherited.push(curr_class);
                                    processed.insert(curr_class);
                                }
                                curr_class = env.objc.get_superclass(curr_class);
                            }
                            to_be_loaded.extend(inherited.into_iter().rev());
                        }
                    }
                    for &class in &to_be_loaded {
                        () = objc::msg_send_no_initialize(env, (class, load_sel));
                    }

                    // Static initializers for libraries must be run before
                    // the initializer in the app binary.
                    for bin_idx in env.get_sorted_bin_indices().unwrap() {
                        let Some(bin) = env.bins.get(bin_idx) else {
                            continue;
                        };
                        let Some(section) =
                            bin.get_section(mach_o::SectionType::ModInitFuncPointers)
                        else {
                            continue;
                        };

                        log_dbg!("Calling static initializers for {:?}", bin.name);
                        assert!(section.size % 4 == 0);

                        let base: mem::ConstPtr<abi::GuestFunction> =
                            mem::Ptr::from_bits(section.addr);

                        let count = section.size / 4;
                        for i in 0..count {
                            let func = env.mem.read(base + i);

                            log_dbg!(
                                "Calling static initializer at {:?} from {:?}",
                                func,
                                (base + i)
                            );

                            () = func.call_from_host(env, ());
                        }
                        log_dbg!("Static initialization done");
                    }

                    {
                        let bin_path = env.bundle.executable_path();

                        let envp_list: Vec<String> = env
                            .env_vars
                            .clone()
                            .iter_mut()
                            .map(|tuple| {
                                [
                                    std::str::from_utf8(tuple.0).unwrap(),
                                    "=",
                                    env.mem.cstr_at_utf8(*tuple.1).unwrap(),
                                ]
                                .concat()
                            })
                            .collect();

                        let envp_ref_list: Vec<&str> =
                            envp_list.iter().map(|keyvalue| keyvalue.as_str()).collect();

                        let bin_path_apple_key = format!("executable_path={}", bin_path.as_str());

                        let argv = Vec::from_iter(
                            std::iter::once(bin_path.as_str())
                                .chain(app_args.iter().map(|s| s.as_str())),
                        );

                        let envp = envp_ref_list.as_slice();
                        let apple = &[bin_path_apple_key.as_str()];
                        stack::prep_stack_for_start(
                            &mut env.mem,
                            &mut env.cpu,
                            &argv,
                            envp,
                            apple,
                            entry_point_is_lc_main,
                        );
                    }

                    // Manually call here, since running call_from_host pushes
                    // a stack frame and disrupts abi for _start.
                    env.cpu
                        .branch_with_link(entry_point_addr, env.dyld.thread_exit_routine());

                    env.run_call();

                    panic!("Main function exited unexpectedly!");
                })
            }));

            if let Err(e) = res {
                let panic_cell = env.panic_cell.clone();
                panic_cell.set(Some(env));
                std::panic::resume_unwind(e);
            }
            env
        });

        let main_thread = Thread {
            active: true,
            blocked_by: ThreadBlock::NotBlocked,
            return_value: None,
            guest_context: None,
            host_context: Some(main_thread_init_routine),
            stack: Some(mem::Mem::MAIN_THREAD_STACK_LOW_END..=0u32.wrapping_sub(1)),
            thread_local_framework_state: Default::default(),
        };

        let mut env = Environment {
            startup_time,
            bundle: NullableBox::new(bundle),
            fs: NullableBox::new(fs),
            window,
            openal_manager: NullableBox::new(OpenALManager::new()?),
            mem: NullableBox::new(mem),
            bins,
            objc: NullableBox::new(objc),
            dyld: NullableBox::new(dyld),
            cpu: NullableBox::new(cpu),
            current_thread: 0,
            threads: vec![main_thread],
            libc_state: Default::default(),
            mutex_state: Default::default(),
            framework_state: Default::default(),
            options: NullableBox::new(options),
            gdb_server: None,
            env_vars: Default::default(),
            dump_file: None,
            is_app_picker: false,
            yielder: std::ptr::null(),
            remaining_ticks: None,
            panic_cell: Rc::new(Cell::new(None)),
            udf_bypass_last: None,
            udf_bypass_count: 0,
        };

        if env.options.dumping_options.any() {
            env.dump_file =
                Some(std::fs::File::create(&env.options.dumping_file).map_err(|e| e.to_string())?);
        }

        env.set_up_initial_env_vars();
        dyld::Dyld::do_late_linking(&mut env);

        env.cpu.set_cpsr(cpu::Cpu::CPSR_USER_MODE);

        if let Some(addrs) = env.options.gdb_listen_addrs.take() {
            let listener = TcpListener::bind(addrs.as_slice())
                .map_err(|e| format!("Could not bind to {addrs:?}: {e}"))?;

            echo!(
                "Waiting for debugger connection on {}...",
                addrs
                    .into_iter()
                    .map(|a| format!("{a}"))
                    .collect::<Vec<String>>()
                    .join(", ")
            );

            let (client, client_addr) = listener
                .accept()
                .map_err(|e| format!("Could not accept connection: {e}"))?;

            echo!("Debugger client connected on {}.", client_addr);
            let mut gdb_server = gdb::GdbServer::new(client);
            let step = gdb_server.wait_for_debugger(None, &mut env.cpu, &mut env.mem);

            assert!(!step, "Can't step right now!"); // TODO?
            env.gdb_server = Some(Box::new(gdb_server));
        }

        if env.options.dumping_options.linking_info {
            let file = env.dump_file.as_mut().unwrap();

            env.objc.dump_classes(file).unwrap();
            env.dyld.dump_lazy_symbols(&env.bins, file).unwrap();
            env.objc
                .dump_selectors(&env.bins[0], &env.mem, file)
                .unwrap();
        }

        env.cpu.branch(entry_point_addr);
        Ok(env)
    }

    /// Set up the emulator environment without loading an app binary.
    ///
    /// This is a special mode that only exists to support the app picker, which
    /// uses the emulated environment to draw its UI and process input. Filling
    /// some of the fields with fake data is a hack, but it means the frameworks
    /// do not need to be aware of the app picker's peculiarities, so it is
    /// cleaner than the alternative!
    pub fn new_without_app(
        options: options::Options,
        icon: image::Image,
    ) -> Result<Environment, String> {
        // Enforces a one (real) Environment limit. See `with_yielder` for
        // why this is needed.
        if ENVIRONMENT_INSTANCE_EXISTS.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("Only one (real) Environment can exist at a time!".to_string());
        }
        ENVIRONMENT_INSTANCE_EXISTS.store(true, std::sync::atomic::Ordering::Relaxed);
        let bundle = bundle::Bundle::new_fake_bundle();
        let fs = fs::Fs::new_fake_fs();

        let startup_time = Instant::now();

        let launch_image = None;

        assert!(!options.headless);
        let window = Some(Box::new(window::Window::new(
            &format!(
                "touchHLE {}{}{}",
                super::branding(),
                if super::branding().is_empty() {
                    ""
                } else {
                    " "
                },
                super::VERSION
            ),
            Some(icon),
            launch_image,
            &options,
        )));

        let mut mem = mem::Mem::new();

        let bins = Vec::new();

        let mut objc = objc::ObjC::new();

        let mut dyld = dyld::Dyld::new();

        dyld.do_initial_linking_with_no_bins(&mut mem, &mut objc);

        let cpu = cpu::Cpu::new(match options.direct_memory_access {
            true => Some(&mut mem),
            false => None,
        });

        let main_thread = Thread {
            active: true,
            blocked_by: ThreadBlock::NotBlocked,
            return_value: None,
            guest_context: None,
            host_context: None,
            stack: Some(mem::Mem::MAIN_THREAD_STACK_LOW_END..=0u32.wrapping_sub(1)),
            thread_local_framework_state: Default::default(),
        };

        let mut env = Environment {
            startup_time,
            bundle: NullableBox::new(bundle),
            fs: NullableBox::new(fs),
            window,
            openal_manager: NullableBox::new(OpenALManager::new()?),
            mem: NullableBox::new(mem),
            bins,
            objc: NullableBox::new(objc),
            dyld: NullableBox::new(dyld),
            cpu: NullableBox::new(cpu),
            current_thread: 0,
            threads: vec![main_thread],
            libc_state: Default::default(),
            mutex_state: Default::default(),
            framework_state: Default::default(),
            options: NullableBox::new(options),
            gdb_server: None,
            env_vars: Default::default(),
            dump_file: None,
            is_app_picker: true,
            yielder: std::ptr::null(),
            remaining_ticks: None,
            panic_cell: Rc::new(Cell::new(None)),
            udf_bypass_last: None,
            udf_bypass_count: 0,
        };

        env.set_up_initial_env_vars();

        // Dyld::do_late_linking() would be called here, but it doesn't do
        // anything relevant here, so it's skipped.

        {
            let argv = &[];
            let envp = &[];
            let apple = &[];
            stack::prep_stack_for_start(&mut env.mem, &mut env.cpu, argv, envp, apple, false);
        }

        env.cpu.set_cpsr(cpu::Cpu::CPSR_USER_MODE);

        // GDB server setup would be done here, but there's no need for it.

        // "CPU emulation begins now" would happen here, but there's nothing
        // to emulate. :)

        Ok(env)
    }

    /// Create a new Environment to swap with.
    ///
    /// SAFETY: You must *NEVER, IN ANY CIRCUMSTANCE* dereference any fields or
    /// call any methods on the environment. This means that you must *NEVER,
    /// IN ANY CIRCUMSTANCE* leak this to safe code. You *MUST* make sure this
    /// includes panic safety - do not allow a panic to accidentally smuggle
    /// out this environment to safe code!
    ///
    /// Admittedly, even if this is leaked, it's very unlikely it would lead to
    /// any real problems, just a null pointer deref.
    unsafe fn new_fake() -> Self {
        Self {
            startup_time: Instant::now(),
            bundle: NullableBox::null(),
            fs: NullableBox::null(),
            window: None,
            openal_manager: NullableBox::null(),
            mem: NullableBox::null(),
            bins: Vec::new(),
            objc: NullableBox::null(),
            dyld: NullableBox::null(),
            cpu: NullableBox::null(),
            current_thread: 0,
            threads: Vec::new(),
            libc_state: NullableBox::null(),
            framework_state: NullableBox::null(),
            mutex_state: NullableBox::null(),
            options: NullableBox::null(),
            gdb_server: None,
            env_vars: HashMap::new(),
            dump_file: None,
            is_app_picker: true,
            yielder: std::ptr::null(),
            remaining_ticks: None,
            panic_cell: Rc::new(Cell::new(None)),
            udf_bypass_last: None,
            udf_bypass_count: 0,
        }
    }

    /// Add a [corosensei::Yielder] so that it can be used by the passed
    /// Environment in the passed function. This exists to avoid
    /// reannotating all code with an additional lifetime on every use of
    /// Environment.
    ///
    /// The design _would_ be unsound if it wasn't for the one real
    /// Environment limit.
    ///
    /// Theoretically (even if this is extremely unlikely), this could
    /// happen:
    ///      - New thread(coroutine) is created and calls [Self::with_yielder]
    ///      - Coroutine swaps the provided env with another env.
    ///      - Coroutine moves the env to the executor.
    ///      - Coroutine ends (and ends the yielder).
    ///      - Executor uses yielder - oh no, UAF!
    ///  (While this is pretty theoretical, prudent readers will in fact
    ///  notice this is the exact same way we share Environments across
    ///  threads anyways! - just with [Self::new_fake] instead of a "real"
    ///  Environment.)
    ///
    ///  There is however, (seemingly) no way to (safely) move out behind a
    ///  &mut T without another T to replace it - so it is safe as long there
    ///  is only ever one Environment exposed to safe code (this is part of
    ///  the [Self::new_fake] safety requirements).
    pub fn with_yielder<F, T>(&mut self, yielder: &Yielder<Environment, Environment>, block: F) -> T
    where
        F: FnOnce(&mut Environment) -> T + 'static,
        T: 'static,
    {
        assert!(self.yielder.is_null());

        self.yielder = yielder;
        // We need to ensure panic safety here, so make sure to reset the
        // yielder if the inner function panics.
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| block(self)));
        self.yielder = std::ptr::null();
        match res {
            Ok(ret) => ret,
            Err(e) => {
                std::panic::resume_unwind(e);
            }
        }
    }

    /// Get a shared reference to the window. Panics if touchHLE is running in
    /// headless mode.
    pub fn window(&self) -> &window::Window {
        self.window.as_ref().expect(
            "Tried to do something that needs a window, but touchHLE is running in headless mode!",
        )
    }

    /// Get a mutable reference to the window. Panics if touchHLE is running
    /// in headless mode.
    pub fn window_mut(&mut self) -> &mut window::Window {
        self.window.as_mut().expect(
            "Tried to do something that needs a window, but touchHLE is running in headless mode!",
        )
    }

    pub fn stack_for_longjmp(&self, mut lr: u32, fp: u32) -> Vec<u32> {
        let stack_range = self.threads[self.current_thread].stack.clone().unwrap();

        let mut frames = Vec::new();
        let mut fp: mem::ConstPtr<u8> = mem::Ptr::from_bits(fp);
        let return_to_host_routine_addr = self.dyld.return_to_host_routine().addr_with_thumb_bit();

        while stack_range.contains(&fp.to_bits()) && lr != return_to_host_routine_addr {
            frames.push(lr);

            lr = self.mem.read((fp + 4).cast());
            fp = self.mem.read(fp.cast());
        }
        frames
    }

    fn dump_all_regs(&self) {
        echo_no_panic!(
            "Dumping registers for current thread (#{})",
            self.current_thread
        );
        self.cpu.dump_regs();
        for (tid, thread) in self.threads.iter().enumerate() {
            if thread.active && tid != self.current_thread {
                echo_no_panic!(
                    "Dumping registers for thread #{} (blocked by {:?})",
                    tid,
                    thread.blocked_by
                );
                let Some(ctx) = thread.guest_context.as_ref() else {
                    echo_no_panic!("Could not get registers for thread {}!", tid);
                    return;
                };
                cpu::Cpu::echo_regs(&ctx.regs);
            }
        }
    }

    pub(crate) fn stack_trace_current(&self) {
        if self.current_thread == 0 {
            echo_no_panic!("Attempting to produce stack trace for main thread:");
        } else {
            echo_no_panic!(
                "Attempting to produce stack trace for thread {}:",
                self.current_thread
            );
        }
        self.stack_trace_for_thread(self.current_thread);
    }

    fn stack_trace_all(&self) {
        echo_no_panic!(
            "Attempting to produce stack trace for current thread (#{}):",
            self.current_thread
        );
        self.stack_trace_for_thread(self.current_thread);
        for tid in 0..self.threads.len() {
            if self.threads[tid].active && tid != self.current_thread {
                echo_no_panic!("Attempting to produce stack trace for thread #{}:", tid);
                self.stack_trace_for_thread(tid);
            }
        }
    }

    fn stack_trace_for_thread(&self, tid: usize) {
        if tid >= self.threads.len() {
            echo_no_panic!(
                "Thread {} is too large ({} threads exist)!",
                tid,
                self.threads.len()
            );
        }
        let Some(stack_range) = self.threads[tid].stack.clone() else {
            echo_no_panic!("Failed to get stack trace!");
            return;
        };
        let (regs, cpsr) = if self.current_thread == tid {
            // Current thread is not stored in context since it is used by cpu,
            // get it from cpu.
            (*self.cpu.regs(), self.cpu.cpsr())
        } else {
            let Some(ctx) = self.threads[tid].guest_context.as_ref() else {
                echo_no_panic!("Failed to get registers for thread {}!", tid);
                return;
            };
            (ctx.regs, ctx.cpsr)
        };
        let pc_nothumb = regs[cpu::Cpu::PC];
        let thumb = (cpsr & cpu::Cpu::CPSR_THUMB) == cpu::Cpu::CPSR_THUMB;
        let pc = GuestFunction::from_addr_and_thumb_flag(pc_nothumb, thumb);
        echo_no_panic!(" 0. {:#x} (PC)", pc.addr_with_thumb_bit());

        let mut lr = regs[cpu::Cpu::LR];
        let return_to_host_routine_addr = self.dyld.return_to_host_routine().addr_with_thumb_bit();
        let thread_exit_routine_addr = self.dyld.thread_exit_routine().addr_with_thumb_bit();

        if lr == return_to_host_routine_addr {
            echo_no_panic!(" 1. [host function] (LR)");
        } else if lr == thread_exit_routine_addr {
            echo_no_panic!(" 1. [thread exit] (LR)");
            return;
        } else {
            echo_no_panic!(" 1. {:#x} (LR)", lr);
        }
        let mut i = 2;
        let mut fp: mem::ConstPtr<u8> = mem::Ptr::from_bits(regs[abi::FRAME_POINTER]);
        loop {
            if !stack_range.contains(&fp.to_bits()) {
                echo_no_panic!("Next FP ({:?}) is outside the stack.", fp);
                break;
            }
            lr = self.mem.read((fp + 4).cast());
            fp = self.mem.read(fp.cast());
            if lr == return_to_host_routine_addr {
                echo_no_panic!("{:2}. [host function]", i);
            } else if lr == thread_exit_routine_addr {
                echo_no_panic!("{:2}. [thread exit]", i);
                return;
            } else {
                echo_no_panic!("{:2}. {:#x}", i, lr);
            }
            i += 1;
        }
    }

    /// Create a new thread and return its ID. The `start_routine` and
    /// `user_data` arguments have the same meaning as the last two arguments to
    /// `pthread_create`.
    pub fn new_thread(
        &mut self,
        start_routine: abi::GuestFunction,
        user_data: mem::MutVoidPtr,
        stack_size: GuestUSize,
    ) -> ThreadId {
        let stack_alloc = self.mem.alloc(stack_size);
        let stack_high_addr = stack_alloc.to_bits() + stack_size;
        assert!(stack_high_addr.is_multiple_of(4));

        let thread_routine = Coroutine::new(move |yielder, mut env: Environment| {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                env.with_yielder(yielder, move |env| {
                    let regs = env.cpu.regs_mut();
                    regs[cpu::Cpu::LR] = env.dyld.thread_exit_routine().addr_with_thumb_bit();
                    regs[cpu::Cpu::SP] = stack_high_addr;
                    regs[0] = user_data.to_bits();

                    env.cpu.set_cpsr(
                        cpu::Cpu::CPSR_USER_MODE
                            | ((start_routine.is_thumb() as u32) * cpu::Cpu::CPSR_THUMB),
                    );
                    let return_value: mem::MutVoidPtr =
                        start_routine.call_from_host(env, (user_data,));
                    let curr_thread = &mut env.threads[env.current_thread];
                    curr_thread.return_value = Some(return_value);
                    curr_thread.active = false;
                });
            }));
            if let Err(e) = res {
                let panic_cell = env.panic_cell.clone();
                panic_cell.set(Some(env));
                std::panic::resume_unwind(e);
            }
            env
        });

        self.threads.push(Thread {
            active: true,
            blocked_by: ThreadBlock::NotBlocked,
            return_value: None,
            guest_context: Some(Box::new(cpu::CpuContext::new())),
            host_context: Some(thread_routine),
            stack: Some(stack_alloc.to_bits()..=(stack_high_addr - 1)),
            thread_local_framework_state: Default::default(),
        });

        let new_thread_id = self.threads.len() - 1;

        log_dbg!("Created new thread {} with stack {:#x}–{:#x}, will execute function {:?} with data {:?}", new_thread_id, stack_alloc.to_bits(), (stack_high_addr - 1), start_routine, user_data);

        new_thread_id
    }

    #[allow(unused)]
    pub fn get_tl_framework_state(&mut self) -> &mut frameworks::ThreadLocalState {
        &mut self.threads[self.current_thread].thread_local_framework_state
    }

    /// Record that the guest has made observable forward progress, clearing the
    /// repeated-`UndefinedInstruction` bypass counter in `debug_cpu_error`.
    ///
    /// That counter exists to stop an app that is stuck re-trapping on the same
    /// bad address from hanging the emulator forever. Presenting a frame is
    /// proof that the app is *not* stuck, so a game that draws while tolerating
    /// a bogus call every frame should be allowed to keep running instead of
    /// being aborted once the count happens to reach the limit.
    pub fn note_forward_progress(&mut self) {
        self.udf_bypass_last = None;
        self.udf_bypass_count = 0;
    }

    /// Put the current thread to sleep for some duration, running other threads
    /// in the meantime as appropriate. Functions that call sleep right before
    /// they return back to the main run loop ([Environment::run]) should set
    /// `tail_call`.
    pub fn sleep(&mut self, duration: Duration) {
        log_dbg!(
            "Thread {} is going to sleep for {:?}.",
            self.current_thread,
            duration
        );
        let until = Instant::now().checked_add(duration).unwrap();
        self.yield_thread(ThreadBlock::Sleeping(until));
    }

    #[allow(dead_code)]
    pub fn suspend_thread(&mut self, thread: ThreadId) {
        match &mut self.threads[thread].blocked_by {
            ThreadBlock::Suspended(count, _) => {
                *count += 1;
            }
            _ => {
                let previous_thread_state = std::mem::replace(
                    &mut self.threads[thread].blocked_by,
                    ThreadBlock::NotBlocked,
                );
                log_dbg!("Suspend thread {} from {:?}", thread, previous_thread_state);
                let new_state = ThreadBlock::Suspended(1, Box::new(previous_thread_state));
                if thread == self.current_thread {
                    self.yield_thread(new_state);
                } else {
                    self.threads[thread].blocked_by = new_state;
                }
            }
        }
    }

    #[allow(dead_code)]
    pub fn resume_thread(&mut self, thread: ThreadId) {
        let old = std::mem::replace(
            &mut self.threads[thread].blocked_by,
            ThreadBlock::NotBlocked,
        );
        match old {
            ThreadBlock::Suspended(count, previous_thread_state) => {
                assert!(count > 0);
                if count > 1 {
                    self.threads[thread].blocked_by =
                        ThreadBlock::Suspended(count - 1, previous_thread_state);
                } else {
                    log_dbg!("Resume thread {} to {:?}", thread, previous_thread_state);
                    self.threads[thread].blocked_by = *previous_thread_state;
                }
            }
            other => {
                // The caller asked to resume a thread that is not currently
                // suspended. Restore whatever state it was in (already
                // overwritten with NotBlocked above) and log a warning
                // instead of crashing the host.
                log!(
                    "Warning: resume_thread({}) called on a thread that was not Suspended (was {:?}); leaving thread NotBlocked.",
                    thread,
                    other
                );
                self.threads[thread].blocked_by = other;
            }
        }
    }

    /// Block the current thread until the given mutex unlocks.
    ///
    /// Other threads also blocking on this mutex may get access first.
    /// Like all other thread blocking functions, this will suspend
    /// execution of the current host thread.
    pub fn block_on_mutex(&mut self, mutex_id: MutexId) {
        log_dbg!(
            "Thread {} blocking on mutex #{}.",
            self.current_thread,
            mutex_id
        );
        self.yield_thread(ThreadBlock::Mutex(mutex_id));
    }

    /// Locks a semaphore (decrements value of a semaphore and blocks
    /// if necessary).
    ///
    /// Like all other thread blocking functions, this will suspend
    /// execution of the current host thread (if the semaphore is
    /// currently at 0).
    pub fn sem_decrement(&mut self, sem: MutPtr<sem_t>, wait_on_lock: bool) -> bool {
        let host_sem_rc: &mut _ = self
            .libc_state
            .semaphore
            .open_semaphores
            .get_mut(&sem)
            .unwrap();
        let mut host_sem = (*host_sem_rc).borrow_mut();

        if host_sem.value > 0 {
            log_dbg!(
                "sem_decrement: semaphore {:?} is now {}",
                sem,
                host_sem.value
            );
            host_sem.value -= 1;
            return true;
        }

        if !wait_on_lock {
            log_dbg!(
                "sem_decrement: semaphore {:?} attempted decrement without waiting, failed",
                sem,
            );
            return false;
        }
        log_dbg!(
            "Thread {} is blocking on semaphore {:?}",
            self.current_thread,
            sem
        );
        host_sem.waiting.insert(self.current_thread);
        std::mem::drop(host_sem);
        // The scheduler will decrement the semaphore value when it unblocks.
        self.yield_thread(ThreadBlock::Semaphore(sem));
        true
    }

    /// Unlock a semaphore (increments value of a semaphore).
    pub fn sem_increment(&mut self, sem: MutPtr<sem_t>) {
        let host_sem_rc: &mut _ = self
            .libc_state
            .semaphore
            .open_semaphores
            .get_mut(&sem)
            .unwrap();
        let mut host_sem = (*host_sem_rc).borrow_mut();

        host_sem.value += 1;
        log_dbg!(
            "sem_increment: semaphore {:?} is now {}",
            sem,
            host_sem.value
        );
    }

    /// Blocks the current thread until the thread given finishes, writing its
    /// return value to ptr (if non-null).
    ///
    /// Note that there are no protections against joining with a detached
    /// thread, joining a thread with itself, or deadlocking joins. Callers
    /// should ensure these do not occur!
    ///
    /// Like all other thread blocking functions, this will suspend
    /// execution of the current host thread.
    pub fn join_with_thread(&mut self, joinee_thread: ThreadId, ptr: MutPtr<MutVoidPtr>) {
        log_dbg!(
            "Thread {} waiting for thread {} to finish.",
            self.current_thread,
            joinee_thread
        );
        self.yield_thread(ThreadBlock::Joining(joinee_thread, ptr));
    }

    pub fn run_app_picker<F, R>(mut self, f: F) -> R
    where
        F: FnOnce(&mut Environment) -> R + 'static,
        R: 'static,
    {
        let panic_cell = Rc::new(Cell::new(None));
        let mut app_picker_coroutine = Coroutine::new(move |yielder, mut env: Environment| {
            env.panic_cell = panic_cell.clone();
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                env.with_yielder(yielder, f)
            }));
            match res {
                // We want the environment to be dropped outside of the
                // coroutine, so send it back when we return.
                Ok(r) => (r, env),
                Err(e) => {
                    let panic_cell = env.panic_cell.clone();
                    panic_cell.set(Some(env));
                    std::panic::resume_unwind(e);
                }
            }
        });

        loop {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                app_picker_coroutine.resume(self)
            }));
            self = match res {
                Ok(ret) => match ret {
                    corosensei::CoroutineResult::Yield(env) => env,
                    corosensei::CoroutineResult::Return((ret_val, _env)) => {
                        return ret_val;
                    }
                },
                Err(e) => {
                    log_no_panic!("Crash in app picker!");
                    // No need to get the environment back - It's local to this
                    // function anyways.
                    std::panic::resume_unwind(e);
                }
            };
            self.window
                .as_mut()
                .unwrap()
                .poll_for_events(self.options.as_ref());
            assert!(self.threads.len() == 1);
            match self.threads[0].blocked_by {
                ThreadBlock::NotBlocked => {}
                ThreadBlock::Sleeping(until) => {
                    let duration = until.duration_since(Instant::now());
                    std::thread::sleep(duration);
                }
                ref other => {
                    log!(
                        "Warning: Unexpected ThreadBlock in app picker: {:?}; clearing block.",
                        other
                    );
                }
            }
            self.threads[0].blocked_by = ThreadBlock::NotBlocked;
        }
    }

    /// Run the emulator. This is the main loop and won't return until app exit.
    /// Only `main.rs` should call this.
    pub fn run(mut self) {
        let mut curr_host_context = self.threads[0].host_context.take().unwrap();
        let panic_cell = self.panic_cell.clone();
        let mut stepping = false;
        loop {
            #[cfg(target_os = "ios")]
            if crate::take_host_exit_request() {
                echo!("Returning to the iOS host library.");
                let thread = &mut self.threads[self.current_thread];
                assert!(thread.host_context.is_none());
                thread.host_context = Some(curr_host_context);
                return;
            }

            if stepping {
                self.remaining_ticks = None;
            } else {
                // 100,000 ticks is an arbitrary number. It needs to be
                // reasonably large so we aren't jumping in and out of dynarmic
                // or trying to poll for events too often. At the same time,
                // very large values are bad for responsiveness.
                self.remaining_ticks = Some(100_000);
            }
            let mut kill_current_thread = false;
            if let Some(w) = self.window.as_mut() {
                w.on_main_stack = false;
            }
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                curr_host_context.resume(self)
            }));
            self = match res {
                Ok(ret) => match ret {
                    corosensei::CoroutineResult::Yield(env) => env,
                    corosensei::CoroutineResult::Return(env) => {
                        kill_current_thread = true;
                        env
                    }
                },
                Err(e) => {
                    let Some(mut env) = panic_cell.take() else {
                        log_no_panic!("Did not recieve env from coroutine unwind, must abort!");
                        std::process::exit(-1)
                    };
                    if let Some(window) = env.window.as_mut() {
                        window.on_main_stack = true;
                    };

                    echo!("Register state immediately after panic:");
                    env.dump_all_regs();
                    env.stack_trace_all();

                    if env.options.popup_errors {
                        let error_string = if let Some(s) = e.downcast_ref::<&str>() {
                            s
                        } else if let Some(s) = e.downcast_ref::<String>() {
                            s
                        } else {
                            "(non-string payload)"
                        };
                        let message = match crate::last_panic_location() {
                            Some(location) => format!("{error_string} (at {location})"),
                            None => error_string.to_string(),
                        };
                        window::show_error_messagebox(env.window.as_deref(), &message);
                    }
                    // Put the host context back before resuming, the env will
                    // clean it up on drop.
                    let Some(thread) = env.threads.get_mut(env.current_thread) else {
                        log_no_panic!("Bad current_thread, must abort!");
                        std::process::exit(-1)
                    };
                    thread.host_context = Some(curr_host_context);
                    std::panic::resume_unwind(e);
                }
            };
            let mut old_context = if kill_current_thread {
                log_dbg!("Killing thread {}", self.current_thread);
                panic_cell.set(Some(self));
                std::mem::drop(curr_host_context);
                let Some(env) = panic_cell.take() else {
                    log_no_panic!("Did not get env back from coroutine after drop, must abort!");
                    std::process::exit(-1)
                };
                self = env;
                let stack = self.threads[self.current_thread].stack.take().unwrap();
                let stack: mem::MutVoidPtr = mem::Ptr::from_bits(*stack.start());
                log_dbg!("Freeing thread {} stack {:?}", self.current_thread, stack);
                self.mem.free(stack);
                None
            } else {
                Some(curr_host_context)
            };
            if let Some(w) = self.window.as_mut() {
                w.on_main_stack = true;
            }

            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // To maintain responsiveness when moving the window and so on,
                // we need to poll for events occasionally, even if the app
                // isn't actively processing them.
                // Polling for events can be quite expensive, so we shouldn't do
                // this until after we've done some amount of work on the guest
                // thread, lest every single callback call pay this cost.
                if let Some(ref mut window) = self.window {
                    window.poll_for_events(&self.options);
                }
                let curr_thread_block = self.threads[self.current_thread].blocked_by.clone();
                if stepping || matches!(curr_thread_block, ThreadBlock::WaitingForDebugger(_)) {
                    if old_context.is_none() {
                        let old_thread = self.current_thread;
                        let next_thread = self.schedule_next_thread();
                        self.switch_thread(&mut old_context, next_thread);
                        echo!(
                            "\nGDB WARNING ------- Thread {} has exited - switched thread to {}",
                            old_thread,
                            next_thread
                        );
                    }
                    match self.threads[self.current_thread].blocked_by {
                        ThreadBlock::NotBlocked | ThreadBlock::WaitingForDebugger(_) => {}
                        _ => {
                            let old_thread = self.current_thread;
                            let next_thread = self.schedule_next_thread();
                            self.switch_thread(&mut old_context, next_thread);
                            let block = &self.threads[old_thread].blocked_by;
                            echo!(
                                "\nGDB WARNING ------- Thread {} is blocked by {:?} - switched thread to {}",
                                old_thread,
                                block,
                                next_thread
                            );
                        }
                    }
                    let reason = if let ThreadBlock::WaitingForDebugger(reason) = curr_thread_block
                    {
                        self.threads[self.current_thread].blocked_by = ThreadBlock::NotBlocked;
                        reason.clone()
                    } else {
                        None
                    };
                    let will_step = self.gdb_server.as_deref_mut().unwrap().wait_for_debugger(
                        reason.clone(),
                        self.cpu.as_mut(),
                        self.mem.as_mut(),
                    );
                    if will_step {
                        stepping = true;
                    }
                }

                // Don't switch threads if stepping.
                if stepping {
                    assert!(old_context.is_some());
                    return;
                }

                stepping = false;
                let next_thread = self.schedule_next_thread();
                if next_thread != self.current_thread {
                    self.switch_thread(&mut old_context, next_thread);
                }
                assert!(old_context.is_some());
            }));

            match res {
                Ok(_) => {}
                Err(e) => {
                    if let Some(window) = self.window.as_mut() {
                        window.on_main_stack = true;
                    };
                    if self.options.popup_errors {
                        let error_string = if let Some(s) = e.downcast_ref::<&str>() {
                            s
                        } else if let Some(s) = e.downcast_ref::<String>() {
                            s
                        } else {
                            "(non-string payload)"
                        };
                        let message = match crate::last_panic_location() {
                            Some(location) => format!("{error_string} (at {location})"),
                            None => error_string.to_string(),
                        };
                        window::show_error_messagebox(self.window.as_deref(), &message);
                    }
                    echo!("Register state immediately after panic:");
                    self.dump_all_regs();
                    self.stack_trace_all();

                    // Clean up the used host context. The ones inside the env
                    // are cleaned up by the drop handler.
                    let panic_cell = self.panic_cell.clone();
                    if let Some(ctx) = old_context {
                        panic_cell.set(Some(self));
                        std::mem::drop(ctx);
                        self = panic_cell.take().unwrap_or_else(|| {
                            log_no_panic!(
                            "Did not recieve env from coroutine unwind during drop, must abort!"
                            );
                            std::process::exit(-1)
                        });
                        std::mem::drop(self);
                    };
                    std::panic::resume_unwind(e);
                }
            }
            curr_host_context = old_context.unwrap();
        }
    }

    /// Run the emulator until the app returns control to the host. This is for
    /// host-to-guest function calls (see [abi::CallFromHost::call_from_host]).
    ///
    /// Note that this might execute code from other threads while waiting for
    /// the app to return control on the original thread!
    pub fn run_call(&mut self) {
        let old_thread = self.current_thread;
        self.run_inner();
        assert!(self.current_thread == old_thread);
    }

    /// Switch the current thread, putting the old host context (if it exists)
    /// back into its thread, and the new host context where the old one was.
    ///
    /// This also internally switches the currently used guest context.
    fn switch_thread(&mut self, old_context: &mut Option<HostContext>, new_thread: ThreadId) {
        assert!(new_thread != self.current_thread);
        assert!(self.threads[new_thread].active);
        log_dbg!(
            "Switching thread: {} => {}",
            self.current_thread,
            new_thread
        );
        let mut guest_ctx = self.threads[new_thread].guest_context.take().unwrap();
        self.cpu.swap_context(&mut guest_ctx);
        assert!(self.threads[self.current_thread].guest_context.is_none());
        assert!(old_context.is_some() || !self.threads[self.current_thread].active);
        self.threads[self.current_thread].guest_context = Some(guest_ctx);

        let new_host_ctx = self.threads[new_thread].host_context.take().unwrap();
        self.threads[self.current_thread].host_context = old_context.take();
        *old_context = Some(new_host_ctx);
        self.current_thread = new_thread;
    }

    #[cold]
    /// Let the debugger handle a CPU error, or panic if there's no debugger
    /// connected. Returns [true] if the CPU should step and then resume
    /// debugging, or [false] if it should resume normal execution.
    fn debug_cpu_error(&mut self, error: cpu::CpuError) {
        let instruction_len = if (self.cpu.cpsr() & cpu::Cpu::CPSR_THUMB) != 0 {
            2
        } else {
            4
        };

        if matches!(error, cpu::CpuError::UndefinedInstruction)
            || matches!(error, cpu::CpuError::Breakpoint)
        {
            // Rewind the PC so that it's at the instruction where the error
            // occurred, rather than the next instruction. This is necessary for
            // GDB to detect its software breakpoints. For some reason this
            // isn't correct for memory errors however.
            self.cpu.regs_mut()[cpu::Cpu::PC] -= instruction_len;
        }

        if self.gdb_server.is_none() {
            // Bypass crashes without implementing stubs for every framework.
            // Games often trigger UndefinedInstruction (abort/__builtin_trap)
            // when an API returns nil or an otherwise unexpected value.
            // Fake a function return to LR to keep execution going.
            //
            // However: if we hit the SAME (PC,LR) pair too many times in a row
            // this indicates we are looping forever (LR itself points back
            // through an infinite chain of UDF instructions). In that case
            // panic with a clear message instead of wedging the emulator.
            if matches!(error, cpu::CpuError::UndefinedInstruction) {
                let pc = self.cpu.regs()[cpu::Cpu::PC];
                let lr = self.cpu.regs()[cpu::Cpu::LR];

                // Potato Story Android hard fallback:
                //
                // The generic decoder did not match on-device, but Android
                // repeatedly reports UDF at these exact Thumb-2 sites while
                // desktop runs through them. Force the known constant-load
                // results and advance PC like the desktop path effectively does.
                if cfg!(target_os = "android") && (self.cpu.cpsr() & cpu::Cpu::CPSR_THUMB) != 0 {
                    match pc {
                        // 0x9ec2: MOVW r0, #0xa136
                        // 0x9ec6: MOVT r0, #0x0030
                        0x9ec6 => {
                            let old = self.cpu.regs()[0];
                            let new_value = (old & 0x0000_ffff) | 0x0030_0000;
                            log_no_panic!(
                                "Potato Story Android hard fallback: MOVT r0 at 0x9ec6: {:#x} -> {:#x}; PC=0x9eca",
                                old,
                                new_value
                            );
                            self.cpu.regs_mut()[0] = new_value;
                            self.cpu.regs_mut()[cpu::Cpu::PC] = 0x9eca;
                            self.udf_bypass_last = None;
                            self.udf_bypass_count = 0;
                            return;
                        }

                        // 0xabce: MOVW r1, #0xa0ea
                        0xabce => {
                            log_no_panic!(
                                "Potato Story Android hard fallback: MOVW r1 at 0xabce -> 0xa0ea; PC=0xabd2"
                            );
                            self.cpu.regs_mut()[1] = 0x0000_a0ea;
                            self.cpu.regs_mut()[cpu::Cpu::PC] = 0xabd2;
                            self.udf_bypass_last = None;
                            self.udf_bypass_count = 0;
                            return;
                        }

                        // 0xacd4: MOVT r12, #0x0030
                        // Dynarmic reports/logs the second halfword at 0xacd6.
                        0xacd6 => {
                            let old = self.cpu.regs()[12];
                            let new_value = (old & 0x0000_ffff) | 0x0030_0000;
                            log_no_panic!(
                                "Potato Story Android hard fallback: MOVT r12 at 0xacd4/0xacd6: {:#x} -> {:#x}; PC=0xacd8",
                                old,
                                new_value
                            );
                            self.cpu.regs_mut()[12] = new_value;
                            self.cpu.regs_mut()[cpu::Cpu::PC] = 0xacd8;
                            self.udf_bypass_last = None;
                            self.udf_bypass_count = 0;
                            return;
                        }

                        // 0xadae is another one-off Android trap in the same
                        // startup cluster. Advance past the 32-bit Thumb-2
                        // instruction instead of fake-returning to LR.
                        0xadae => {
                            log_no_panic!(
                                "Potato Story Android hard fallback: skipping trapped Thumb-2 instruction at 0xadae; PC=0xadb2"
                            );
                            self.cpu.regs_mut()[cpu::Cpu::PC] = 0xadb2;
                            self.udf_bypass_last = None;
                            self.udf_bypass_count = 0;
                            return;
                        }

                        _ => {}
                    }
                }

                // Android/Dynarmic workaround:
                //
                // Potato Story hits UndefinedInstruction on valid Thumb-2
                // MOVW/MOVT constant-load instructions on Android, while the
                // same code runs on desktop. Do what the desktop path does:
                // materialize the immediate into the destination register and
                // advance past the 32-bit Thumb-2 instruction instead of
                // fake-returning to LR and looping forever.
                //
                // The PC we log here has already been rewound by the generic
                // Thumb path above, which assumes 2-byte Thumb instructions.
                // Thumb-2 instructions are 4 bytes, so try both PC and PC-2.
                if cfg!(target_os = "android") && (self.cpu.cpsr() & cpu::Cpu::CPSR_THUMB) != 0 {
                    // Android/Dynarmic sometimes reports the fault PC a few
                    // bytes before/after the real 32-bit Thumb-2 instruction.
                    // Scan nearby even halfword starts instead of only pc/pc-2.
                    for delta in [-8i32, -6, -4, -2, 0, 2, 4, 6, 8] {
                        let start = if delta < 0 {
                            pc.wrapping_sub((-delta) as u32)
                        } else {
                            pc.wrapping_add(delta as u32)
                        };

                        if start & 1 != 0 {
                            continue;
                        }

                        let hw1: u16 = self.mem.read(mem::ConstPtr::<u16>::from_bits(start));
                        let hw2: u16 = self
                            .mem
                            .read(mem::ConstPtr::<u16>::from_bits(start.wrapping_add(2)));

                        // Potato Story on Android also trips on Thumb-2
                        // VFP/coprocessor-looking instructions immediately
                        // after the constant-load clusters. Do not fake-return
                        // from the whole function; advance past the trapped
                        // 32-bit instruction and let scene setup continue.
                        if std::env::var_os("TOUCHHLE_POTATO_ANDROID_THUMB2_COMPAT").is_some()
                            && matches!(hw1 & 0xfe00, 0xec00 | 0xee00)
                        {
                            log_no_panic!(
                                "Potato Story Android Thumb-2 compat: skipping coprocessor/VFP-looking instruction at {:#x} (reported PC {:#x}, hw1={:#06x}, hw2={:#06x}); advancing to {:#x}",
                                start,
                                pc,
                                hw1,
                                hw2,
                                start.wrapping_add(4)
                            );

                            self.cpu.regs_mut()[cpu::Cpu::PC] = start.wrapping_add(4);
                            self.udf_bypass_last = None;
                            self.udf_bypass_count = 0;
                            return;
                        }

                        // Thumb-2 MOVW/MOVT immediate encodings. The mask
                        // keeps the opcode bits and ignores immediate bits.
                        // Examples from Potato Story:
                        //   bytes 4a f2 36 10 => hw1=f24a, hw2=1036, MOVW
                        //   bytes c0 f2 30 00 => hw1=f2c0, hw2=0030, MOVT
                        //   bytes 4a f6 ea 01 => hw1=f64a, hw2=01ea, MOVW
                        let is_movw = (hw1 & 0xfbf0) == 0xf240 && (hw2 & 0x8000) == 0;
                        let is_movt = (hw1 & 0xfbf0) == 0xf2c0 && (hw2 & 0x8000) == 0;

                        if is_movw || is_movt {
                            let imm4 = (hw1 & 0x000f) as u32;
                            let i = ((hw1 >> 10) & 1) as u32;
                            let imm3 = ((hw2 >> 12) & 0x7) as u32;
                            let rd = ((hw2 >> 8) & 0xf) as usize;
                            let imm8 = (hw2 & 0x00ff) as u32;
                            let imm16 = (imm4 << 12) | (i << 11) | (imm3 << 8) | imm8;

                            // MOVW/MOVT to PC is not a normal case here; if it
                            // ever appears, let the existing error path handle
                            // it instead of inventing branch semantics.
                            if rd == cpu::Cpu::PC {
                                continue;
                            }

                            let old = self.cpu.regs()[rd];
                            let new_value = if is_movt {
                                (old & 0x0000_ffff) | (imm16 << 16)
                            } else {
                                imm16
                            };

                            log_no_panic!(
                                "Android Thumb-2 compat: emulated {} at {:#x}: r{} {:#x} -> {:#x}; advancing to {:#x}",
                                if is_movt { "MOVT" } else { "MOVW" },
                                start,
                                rd,
                                old,
                                new_value,
                                start.wrapping_add(4)
                            );

                            self.cpu.regs_mut()[rd] = new_value;
                            self.cpu.regs_mut()[cpu::Cpu::PC] = start.wrapping_add(4);
                            self.udf_bypass_last = None;
                            self.udf_bypass_count = 0;
                            return;
                        }
                    }
                }

                // Track repeated occurrences of the same bypass site.
                //
                // The counter is cleared whenever the guest presents a frame
                // (see `note_forward_progress`), so the limit is really "this
                // many identical bogus calls within a single frame". An app
                // that keeps drawing is not hung, whatever it is re-trapping
                // on: Dracula: The Last Sanctuary calls one bad function
                // pointer a couple of times per frame, survives the faked
                // return, and used to be killed at 61 FPS by a guard whose
                // whole purpose is to catch apps that have stopped making
                // progress.
                const BYPASS_LIMIT: u32 = 256;
                const LOG_RATE: u32 = 32;
                let key = (pc, lr);
                let count = if self.udf_bypass_last == Some(key) {
                    self.udf_bypass_count = self.udf_bypass_count.saturating_add(1);
                    self.udf_bypass_count
                } else {
                    self.udf_bypass_last = Some(key);
                    self.udf_bypass_count = 1;
                    1
                };

                if count == 1 || count % LOG_RATE == 0 {
                    log_no_panic!(
                        "Warning: Ignored UndefinedInstruction at {:#x}. \
                         Faking function return to LR ({:#x}) to bypass crash! \
                         cpsr={:#x} thumb={} instruction_len={} \
                         (occurrence {} of at most {})",
                        pc,
                        lr,
                        self.cpu.cpsr(),
                        (self.cpu.cpsr() & cpu::Cpu::CPSR_THUMB) != 0,
                        instruction_len,
                        count,
                        BYPASS_LIMIT
                    );
                }

                if count >= BYPASS_LIMIT {
                    panic!(
                        "UndefinedInstruction at {:#x} looped {} times with \
                         LR={:#x}; giving up to avoid hanging. This usually \
                         means a framework stub returned bogus data that the \
                         guest keeps re-trapping on.",
                        pc, count, lr
                    );
                }

                // Pathological self-loop: when LR (with Thumb bit cleared)
                // points right back at the UDF we just trapped on, branching
                // to LR would re-enter the trap and burn through BYPASS_LIMIT
                // until the emulator panics. This shape shows up when a
                // framework stub returns the address of its own UDF as the
                // return target (e.g. PC=0x5cc86, LR=0x5cc87). Skip past the
                // faulting instruction instead so the guest makes forward
                // progress, and clear the bypass counter since we're no
                // longer bypassing the same site.
                if (lr & !1) == pc {
                    log_no_panic!(
                        "Warning: UndefinedInstruction self-loop at {:#x} \
                         (LR={:#x} re-enters the same UDF). Advancing past \
                         the faulting instruction instead of branching to LR.",
                        pc,
                        lr
                    );
                    self.cpu.regs_mut()[cpu::Cpu::PC] = pc.wrapping_add(instruction_len);
                    self.udf_bypass_last = None;
                    self.udf_bypass_count = 0;
                    return;
                }

                // Instead of skipping forward through garbage data, pretend the
                // faulting function returned to its caller.
                self.cpu.branch(GuestFunction::from_addr_with_thumb_bit(lr));
                return;
            }

            panic!("Error during CPU execution: {error:?}");
        }

        echo!("Debuggable error during CPU execution: {:?}.", error);
        self.enter_debugger(Some(error))
    }

    /// Used to check whether a debugger is connected, and therefore whether
    /// [Environment::enter_debugger] will do something.
    pub fn is_debugging_enabled(&self) -> bool {
        self.gdb_server.is_some()
    }

    /// Suspend execution and hand control to the connected debugger.
    /// You should precede this call with a log message that explains why the
    /// debugger is being invoked. The return value is the same as
    /// [gdb::GdbServer::wait_for_debugger]'s.
    ///
    /// Note that this also yields the thread - take care!
    pub fn enter_debugger(&mut self, reason: Option<cpu::CpuError>) {
        // GDB doesn't seem to manage to produce a useful stack trace, so
        // let's print our own.
        self.stack_trace_current();

        self.yield_thread(ThreadBlock::WaitingForDebugger(reason));
    }

    #[inline(always)]
    /// Respond to the new CPU state (do nothing, execute an SVC or enter
    /// debugging) and decide what to do next.
    fn handle_cpu_state(&mut self, state: cpu::CpuState) -> ThreadNextAction {
        match state {
            cpu::CpuState::Normal => ThreadNextAction::Continue,
            cpu::CpuState::Svc(svc) => {
                // The program counter is pointing at the
                // instruction after the SVC, but we want the
                // address of the SVC itself.
                let svc_pc = self.cpu.regs()[cpu::Cpu::PC] - 4;
                match svc {
                    dyld::Dyld::SVC_RETURN_TO_HOST => {
                        assert!(
                            svc_pc == self.dyld.return_to_host_routine().addr_without_thumb_bit()
                        );
                        // Normal return from host-to-guest call.
                        ThreadNextAction::ReturnToHost
                    }
                    dyld::Dyld::SVC_LAZY_LINK
                    | dyld::Dyld::SVC_LAZY_LINK_RET_FLAG
                    | dyld::Dyld::SVC_LINKED_FUNCTIONS_BASE.. => {
                        if let Some(f) = self.dyld.get_svc_handler(
                            &self.bins,
                            &mut self.mem,
                            &mut self.cpu,
                            svc_pc,
                            svc,
                        ) {
                            f.call_from_guest(self);

                            // ORIGINAL LOGIC MERGED: Stack zeroing
                            if svc & dyld::Dyld::SVC_LAZY_LINK_RET_FLAG == 0 {
                                if let Some(len) = self.options.zero_stack_after_guest_to_host_call
                                {
                                    log_once!(
                                        "Applying zeroing of stack after guest to host call."
                                    );
                                    let start = self.cpu.regs()[cpu::Cpu::SP] - len;
                                    self.mem
                                        .bytes_at_mut(mem::Ptr::from_bits(start), len)
                                        .fill(0);
                                }
                            }

                            // On entry_size 4 return here since there's
                            // no space to add a ret after the svc call
                            if svc & dyld::Dyld::SVC_LAZY_LINK_RET_FLAG != 0 {
                                let lr = self.cpu.regs()[cpu::Cpu::LR];
                                self.cpu.branch(GuestFunction::from_addr_with_thumb_bit(lr));
                            }
                            ThreadNextAction::Continue
                        } else {
                            self.cpu.regs_mut()[cpu::Cpu::PC] = svc_pc;
                            ThreadNextAction::Continue
                        }
                    }
                    dyld::Dyld::SVC_THREAD_EXIT => {
                        if self.current_thread == 0 {
                            log_no_panic!("Main thread exited normally (or crashed early). Returning to host.");
                            ThreadNextAction::ReturnToHost
                        } else {
                            log_dbg!(
                                "Thread {} has completed execution via SVC_THREAD_EXIT. Returning to host.",
                                self.current_thread
                            );

                            // Important: do NOT Continue here.
                            //
                            // The thread-exit routine is an SVC followed by a trap/undefined
                            // instruction. If we continue guest execution after handling the SVC,
                            // PC falls through into that trap and loops forever:
                            //   UndefinedInstruction at 0x3000a014 with LR=0x3000a010
                            //
                            // Returning to host lets the coroutine that called into guest code
                            // finish normally. The secondary-thread coroutine will then store the
                            // return value and mark the thread inactive in the existing normal path.
                            ThreadNextAction::ReturnToHost
                        }
                    }
                }
            }
            cpu::CpuState::Error(e) => ThreadNextAction::DebugCpuError(e),
        }
    }

    fn run_inner(&mut self) {
        let initial_thread = self.current_thread;
        if !self.threads[initial_thread].active {
            log_no_panic!(
                "Warning: run_inner called on inactive thread {}. Returning early.",
                initial_thread
            );
            return;
        }
        if self.threads[initial_thread].guest_context.is_some() {
            // This can happen when an app re-enters the run loop from within
            // a callback (e.g. Pocket Army spawns a worker thread whose
            // completion handler tries to resume the main run loop before the
            // previous invocation has returned). Instead of panicking the
            // whole emulator, log and bail — the outer run_inner is still
            // executing and will pick up from where it left off.
            log_no_panic!(
                "Warning: run_inner called on thread {} which already has a \
                 guest_context (re-entrant run loop?). Returning early to \
                 avoid assertion failure.",
                initial_thread
            );
            return;
        }

        loop {
            while self
                .remaining_ticks
                .is_none_or(|remaining_ticks| remaining_ticks > 0)
            {
                let state = self
                    .cpu
                    .run_or_step(&mut self.mem, self.remaining_ticks.as_mut());
                match self.handle_cpu_state(state) {
                    ThreadNextAction::Continue => {}
                    ThreadNextAction::ReturnToHost => return,
                    ThreadNextAction::DebugCpuError(e) => {
                        self.debug_cpu_error(e);
                    }
                }
                if self.remaining_ticks.is_none() {
                    break;
                }
            }
            self.yield_thread(ThreadBlock::NotBlocked);
        }
    }

    /// Yield the current thread, suspending execution and handing control back
    /// to the executor ([Self::run]), waiting until the current `thread_block`
    /// condition is met.
    pub fn yield_thread(&mut self, thread_block: ThreadBlock) {
        assert!(!self.threads[self.current_thread].is_blocked());
        log_dbg!(
            "Thread {} yielding on {:?}",
            self.current_thread,
            thread_block
        );
        unsafe {
            self.threads[self.current_thread].blocked_by = thread_block;
            // The yielder is set up by `with_yielder` when a coroutine starts
            // executing this thread. If we ever reach `yield_thread` without
            // an active yielder (for example when host code invokes us
            // synchronously before the main coroutine has been entered), we
            // have no coroutine to suspend into. Previously `unwrap()` here
            // panicked the entire emulator; instead, log loudly and clear
            // the block so the host-side caller can keep going. This is the
            // best we can do without a host stack to unwind to.
            let yielder = match self.yielder.as_ref() {
                Some(yielder) => yielder,
                None => {
                    log_no_panic!(
                        "Warning: yield_thread called on thread {} with no \
                         active yielder (block={:?}). Treating as no-op so \
                         the host caller can continue.",
                        self.current_thread,
                        self.threads[self.current_thread].blocked_by,
                    );
                    self.threads[self.current_thread].blocked_by = ThreadBlock::NotBlocked;
                    return;
                }
            };
            self.yielder = std::ptr::null();
            let panic_cell = self.panic_cell.clone();
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let env = std::mem::replace(self, Self::new_fake());
                yielder.suspend(env)
            }));
            match res {
                Ok(env) => {
                    let _ = std::mem::replace(self, env);
                    self.yielder = yielder;
                }
                Err(payload) => {
                    let Some(env) = panic_cell.take() else {
                        log_no_panic!("Did not recieve env for coroutine unwind, must abort!");
                        std::process::exit(-1)
                    };
                    let _ = std::mem::replace(self, env);
                    self.yielder = yielder;
                    std::panic::resume_unwind(payload);
                }
            }
        }
        assert!(!self.threads[self.current_thread].is_blocked());
    }

    /// Find the next thread to execute, and set it up to be switched to.
    ///
    /// This also handles all the required bookkeeping (unlocking mutexes,
    /// decrementing semaphores, setting the thread to be unblocked, etc.).
    /// It is not required that the thread is switched to immediately.
    fn schedule_next_thread(&mut self) -> ThreadId {
        // GDB can allow schedule_next_thread to be called twice in a row -
        // we make sure that this works by immediately fufilling conditions
        // (relocking mutexes, decrementing semaphores, etc.)!
        loop {
            // Try to find a new thread to execute, starting with the thread
            // following the one currently executing.
            let mut next_awakening: Option<Instant> = None;
            for i in 0..self.threads.len() {
                let thread_id = (self.current_thread + 1 + i) % self.threads.len();
                let candidate = &mut self.threads[thread_id];

                if !candidate.active {
                    continue;
                }
                match candidate.blocked_by {
                    ThreadBlock::Sleeping(sleeping_until) => {
                        if sleeping_until <= Instant::now() {
                            log_dbg!("Thread {} finished sleeping.", thread_id);
                            candidate.blocked_by = ThreadBlock::NotBlocked;
                            return thread_id;
                        } else {
                            next_awakening = match next_awakening {
                                None => Some(sleeping_until),
                                Some(other) => Some(other.min(sleeping_until)),
                            };
                        }
                    }
                    ThreadBlock::Mutex(mutex_id) => {
                        if !self.mutex_state.mutex_is_locked(mutex_id) {
                            log_dbg!("Thread {} was unblocked due to mutex #{} unlocking, relocking mutex.", thread_id, mutex_id);
                            self.threads[thread_id].blocked_by = ThreadBlock::NotBlocked;
                            self.relock_unblocked_mutex_for_thread(thread_id, mutex_id);
                            return thread_id;
                        }
                    }
                    ThreadBlock::Semaphore(sem) => {
                        let host_sem_rc: &mut _ = self
                            .libc_state
                            .semaphore
                            .open_semaphores
                            .get_mut(&sem)
                            .unwrap();
                        let mut host_sem = (*host_sem_rc).borrow_mut();
                        if host_sem.value > 0 {
                            log_dbg!(
                                "Thread {} has awaken on semaphore {:?} with value {}",
                                thread_id,
                                sem,
                                host_sem.value
                            );
                            host_sem.value -= 1;
                            host_sem.waiting.remove(&self.current_thread);
                            self.threads[thread_id].blocked_by = ThreadBlock::NotBlocked;
                            return thread_id;
                        }
                    }
                    ThreadBlock::Condition(cond, deadline) => {
                        let host_cond = self
                            .libc_state
                            .pthread
                            .cond
                            .condition_variables
                            .get_mut(&cond)
                            .unwrap();
                        let mutex = host_cond.curr_mutex.unwrap();
                        if host_cond
                            .waking
                            .front()
                            .is_some_and(|waking_thread| *waking_thread == thread_id)
                            && !self.mutex_state.mutex_is_locked(mutex)
                        {
                            log_dbg!("Thread {} is unblocking on cond var {:?}.", thread_id, cond);
                            host_cond.waking.pop_front();
                            self.threads[thread_id].blocked_by = ThreadBlock::NotBlocked;
                            self.relock_unblocked_mutex_for_thread(thread_id, mutex);
                            return thread_id;
                        } else if let Some(deadline) = deadline {
                            let time = SystemTime::now()
                                .duration_since(SystemTime::UNIX_EPOCH)
                                .unwrap();
                            if deadline <= time {
                                log_dbg!(
                                    "Thread {} is timed out on cond var {:?}.",
                                    thread_id,
                                    cond
                                );
                                assert!(!host_cond.timed_out.contains(&thread_id));
                                host_cond.timed_out.insert(thread_id);

                                // FIX 1: Если тред уже был в очереди waking
                                // (ему отправили
                                // сигнал, но он ещё не успел захватить
                                // мьютекс),
                                // удаляем его оттуда вместо паники.
                                host_cond.waking.retain(|&t| t != thread_id);
                                host_cond.waiting.retain(|&t| t != thread_id);

                                // FIX 2: Если мьютекс всё ещё занят другим
                                // тредом при
                                // таймауте, не паникуем, а переводим тред в
                                // ожидание
                                // мьютекса (как в настоящем
                                // pthread_cond_timedwait).
                                if self.mutex_state.mutex_is_locked(mutex) {
                                    log_dbg!(
                                        "Thread {} timed out on cond var {:?} but mutex is locked, blocking on mutex.",
                                        thread_id,
                                        cond
                                    );
                                    self.threads[thread_id].blocked_by = ThreadBlock::Mutex(mutex);
                                } else {
                                    self.threads[thread_id].blocked_by = ThreadBlock::NotBlocked;
                                    self.relock_unblocked_mutex_for_thread(thread_id, mutex);
                                    return thread_id;
                                }
                            } else {
                                // --- ГЛАВНОЕ ИСПРАВЛЕНИЕ ДЕДЛОКА ---
                                // Если таймаут еще не вышел, вычисляем остаток
                                // времени
                                // и добавляем его в next_awakening
                                // планировщика!
                                // Теперь эмулятор не упадет, а честно уснет до
                                // этого момента.
                                let remaining = deadline - time;
                                let awakening = Instant::now() + remaining;
                                next_awakening = match next_awakening {
                                    None => Some(awakening),
                                    Some(other) => Some(other.min(awakening)),
                                };
                                // ------------------------------------
                            }
                        }
                    }
                    ThreadBlock::Joining(joinee_thread, ptr) => {
                        if !self.threads[joinee_thread].active {
                            log_dbg!(
                                "Thread {} joining with now finished thread {}.",
                                self.current_thread,
                                joinee_thread
                            );
                            // Write the return value, unless the pointer to
                            // write to is null.
                            if !ptr.is_null() {
                                self.mem
                                    .write(ptr, self.threads[joinee_thread].return_value.unwrap());
                            }
                            self.threads[thread_id].blocked_by = ThreadBlock::NotBlocked;
                            return thread_id;
                        }
                    }
                    ThreadBlock::NotBlocked => {
                        return thread_id;
                    }
                    ThreadBlock::WaitingForDebugger(_) => unreachable!(),
                    ThreadBlock::Suspended(cnt, _) => {
                        // Original enforced assertion
                        assert!(cnt > 0);
                    }
                }
            }

            // All suitable threads are blocked and at least one is asleep.
            // Sleep until one of them wakes up.
            if let Some(next_awakening) = next_awakening {
                let duration = next_awakening.duration_since(Instant::now());
                log_dbg!("All threads blocked/asleep, sleeping for {:?}.", duration);
                std::thread::sleep(duration);
                // Try again, there should be some thread awake now (or
                // there will be soon, since timing is approximate).
                continue;
            } else {
                // All threads are blocked but none are sleeping — potential
                // deadlock. Before panicking, give conditions with timeouts
                // a brief grace period (10ms). This handles the edge case
                // where a condition-wait with timeout hasn't been detected
                // as "sleeping" because the scheduler loop hasn't
                // re-evaluated it yet. After the grace period, if still
                // stuck, abort with a clear diagnostic.
                static DEADLOCK_GRACE_COUNT: std::sync::atomic::AtomicU32 =
                    std::sync::atomic::AtomicU32::new(0);
                let count = DEADLOCK_GRACE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if count < 3 {
                    log!(
                        "Warning: All threads appear blocked (attempt {}/3). \
                         Sleeping 10ms before retrying…",
                        count + 1
                    );
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                DEADLOCK_GRACE_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
                panic!("No active threads, program has deadlocked!");
            }
        }
    }

    fn set_up_initial_env_vars(&mut self) {
        // TODO: Provide all the system environment variables an app might
        // expect to find.
        // Initialize HOME envvar
        let home_value_cstr = self
            .mem
            .alloc_and_write_cstr(self.fs.home_directory().as_str().as_bytes());
        self.env_vars.insert(b"HOME".to_vec(), home_value_cstr);
    }

    fn get_sorted_bin_indices(&self) -> Result<Vec<usize>, String> {
        let dylib_graph: Vec<BinaryDependencyNode> = self
            .bins
            .iter()
            .map(|bin| BinaryDependencyNode {
                name: bin.name.clone(),
                dependencies: bin.dynamic_libraries.clone(),
            })
            .collect();
        generate_binary_load_order(&dylib_graph)
    }

    /// Run a function using window and options on the parent stack if we are
    /// inside a coroutine, or run it directly if we aren't. Some
    /// [window::Window] functions require to be called inside this function.
    ///
    /// Android's ABI seems to dislike if certain functions aren't called from
    /// the main stack. Since corosensei uses seperate stacks to run
    /// coroutines, Android doesn't recognize it as the main stack, so those
    /// functions need to be run on the main stack. Unfortunately, there's no
    /// documentation of which functions need to be called with this, so we
    /// have to check ourselves.
    pub fn on_parent_stack_in_coroutine<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut window::Window, &mut options::Options) -> R + Send,
    {
        struct WindowWrapper<'a> {
            window: &'a mut window::Window,
        }
        // SAFETY: we're not sending across threads, we're only sending across
        // the coroutine boundary so it's ok.
        unsafe impl Send for WindowWrapper<'_> {}

        if !self.yielder.is_null() {
            unsafe {
                let yielder = self.yielder.as_ref().unwrap();
                let wrapped = WindowWrapper {
                    window: self.window.as_mut().unwrap(),
                };
                let res = yielder.on_parent_stack(|| {
                    let wrapped = wrapped;
                    wrapped.window.on_main_stack = true;
                    f(wrapped.window, self.options.as_mut())
                });
                self.window.as_mut().unwrap().on_main_stack = false;
                res
            }
        } else {
            if let Some(w) = self.window.as_mut() {
                w.on_main_stack = true;
            }
            f(self.window.as_mut().unwrap(), self.options.as_mut())
        }
    }
}

impl Drop for Environment {
    // Clean up all the remaining HostContexts. This isn't strictly required,
    // since this should only occur after a sucessful panic or the app ending,
    // but it is a bit cleaner and avoids confusion inside the logs.
    fn drop(&mut self) {
        if self.objc.is_null() {
            return;
        }
        if let Some(w) = self.window.as_mut() {
            w.on_main_stack = false;
        }
        if self.threads.is_empty()
            || self
                .threads
                .iter()
                .all(|thread| thread.host_context.is_none())
        {
            ENVIRONMENT_INSTANCE_EXISTS.store(false, std::sync::atomic::Ordering::SeqCst);
            return;
        }
        unsafe {
            let mut env = std::mem::replace(self, Environment::new_fake());
            let panic_cell = env.panic_cell.clone();
            let threads_len = env.threads.len();
            for i in 0..threads_len {
                let host_context = env.threads[i].host_context.take();
                panic_cell.set(Some(env));
                std::mem::drop(host_context);
                env = panic_cell.take().unwrap_or_else(|| {
                    log_no_panic!(
                        "Did not recieve env from coroutine unwind during drop, must abort!"
                    );
                    std::process::exit(-1)
                });
            }
            *self = env;
        }
        ENVIRONMENT_INSTANCE_EXISTS.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod dylib_sorting_tests {
    use std::collections::HashSet;

    use super::*;
    fn create_dylib_graph(bin_configs: &[(&str, &[&str])]) -> Vec<BinaryDependencyNode> {
        bin_configs
            .iter()
            .map(|(name, dependencies)| BinaryDependencyNode {
                name: name.to_string(),
                dependencies: dependencies.iter().map(|s| s.to_string()).collect(),
            })
            .collect()
    }

    /// Verify dylib sort by checking that no dependents are needed
    /// before their import
    fn verify_sort(graph: &[BinaryDependencyNode], sorted_indices: &[usize]) {
        assert_eq!(sorted_indices.len(), graph.len());
        let bin_to_index: HashMap<_, _> = graph
            .iter()
            .enumerate()
            .map(|(idx, node)| (node.name.as_str(), idx))
            .collect();
        let mut loaded_dylibs = HashSet::new();

        for &index in sorted_indices {
            let current_bin = graph.get(index).unwrap();
            for dependency in current_bin
                .dependencies
                .iter()
                .map(|path| path.strip_prefix("/usr/lib/").unwrap_or(path.as_str()))
            {
                // Ignore dependencies that are not included in packaged dylibs
                let Some(&dylib_index) = bin_to_index.get(dependency) else {
                    continue;
                };

                assert!(loaded_dylibs.contains(&dylib_index));
            }

            loaded_dylibs.insert(index);
        }
    }

    #[test]
    fn test_no_dependencies() {
        let dylib_graph = create_dylib_graph(&[]);
        let sorted_indices = generate_binary_load_order(&dylib_graph).unwrap();
        verify_sort(&dylib_graph, &sorted_indices);
    }

    #[test]
    fn test_single_bin() {
        let dylib_graph = create_dylib_graph(&[("A", &[])]);
        let sorted_indices = generate_binary_load_order(&dylib_graph).unwrap();

        verify_sort(&dylib_graph, &sorted_indices);
    }

    #[test]
    fn test_linear_dependencies() {
        // A -> B -> C -> D
        let dylib_graph = create_dylib_graph(&[
            ("A", &[]),
            ("B", &["/usr/lib/A"]),
            ("C", &["/usr/lib/B"]),
            ("D", &["/usr/lib/C"]),
        ]);

        let sorted_indices = generate_binary_load_order(&dylib_graph).unwrap();

        verify_sort(&dylib_graph, &sorted_indices);
    }

    #[test]
    fn test_diamond_dependencies() {
        // A -> B -> D
        //  \-> C -/
        let dylib_graph =
            create_dylib_graph(&[("A", &[]), ("B", &["A"]), ("C", &["A"]), ("D", &["B", "C"])]);
        let sorted_indices = generate_binary_load_order(&dylib_graph).unwrap();

        verify_sort(&dylib_graph, &sorted_indices);
    }

    #[test]
    fn test_with_isolated_nodes() {
        // A -> B
        // C
        // D
        let dylib_graph = create_dylib_graph(&[("A", &[]), ("B", &["A"]), ("C", &[]), ("D", &[])]);
        let sorted_indices = generate_binary_load_order(&dylib_graph).unwrap();

        verify_sort(&dylib_graph, &sorted_indices);
    }

    #[test]
    fn test_complex_dependency_graph() {
        // A -> B -> D
        // A -> C -> E
        // F -> G
        // H
        let dylib_graph = create_dylib_graph(&[
            ("A", &[]),
            ("B", &["A"]),
            ("C", &["A"]),
            ("D", &["B"]),
            ("E", &["C"]),
            ("F", &[]),
            ("G", &["F"]),
            ("H", &[]),
        ]);
        let sorted_indices = generate_binary_load_order(&dylib_graph).unwrap();

        verify_sort(&dylib_graph, &sorted_indices);
    }

    #[test]
    fn test_with_external_dependencies() {
        let dylib_graph = create_dylib_graph(&[
            ("A", &["external1"]),
            ("B", &["A", "external2"]),
            ("C", &["B"]),
        ]);
        let sorted_indices = generate_binary_load_order(&dylib_graph).unwrap();

        verify_sort(&dylib_graph, &sorted_indices);
    }

    #[test]
    fn test_cycle() {
        // A -> B -> C -> A
        let dylib_graph = create_dylib_graph(&[("A", &["C"]), ("B", &["A"]), ("C", &["B"])]);
        let result = generate_binary_load_order(&dylib_graph);

        assert!(
            result.is_err(),
            "Sort should detect cycle and return an error"
        );
    }

    #[test]
    fn test_self_dependency() {
        let dylib_graph = create_dylib_graph(&[("A", &["A"])]);
        let result = generate_binary_load_order(&dylib_graph);

        assert!(
            result.is_err(),
            "Sort should detect self-dependency as a cycle and return an error"
        );
    }
}
