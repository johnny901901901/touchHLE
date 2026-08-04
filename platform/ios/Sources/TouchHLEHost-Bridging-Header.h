#include <stdbool.h>
#include <stdint.h>
#include <stddef.h>

// Whether a debugger is attached to service dynarmic's JIT trap. Without one,
// starting a game kills the app.
bool touchhle_ios_jit_available(void);

/// Whether JIT came from an attached debugger (per-process, dies on relaunch)
/// rather than the dynamic-codesigning entitlement (permanent).
bool touchhle_ios_jit_is_from_debugger(void);

/// A debugger is attached RIGHT NOW and can service oaknut's `brk` JIT trap.
/// Required by HyperHLE.
bool touchhle_ios_debugger_attached(void);

/// The process has been made debuggable (CS_DEBUGGED), or carries
/// dynamic-codesigning. Survives the debugger detaching, and is enough for a
/// core that maps RWX memory directly, such as touchHLE.
bool touchhle_ios_process_is_debuggable(void);

/// Raw signals behind the JIT verdict, so a wrong verdict can be diagnosed on
/// device instead of guessed at.
typedef struct {
    /// CS_DEBUGGED: survives the debugger detaching, so this is the signal that
    /// still reports the truth after TrollStore has done its work.
    int csops_result;
    unsigned int cs_flags;
    bool cs_debugged;
    /// P_TRACED: only true while a debugger is actually attached.
    int sysctl_result;
    int sysctl_errno;
    unsigned int proc_flags;
    bool traced;
    bool has_dynamic_codesigning;
    bool mmap_rwx_ok;
    bool mprotect_exec_ok;
} TouchHLEJITDiagnostics;

void touchhle_ios_jit_diagnostics(TouchHLEJITDiagnostics *out);
void touchhle_ios_log_jit_status(const char *context);

// The emulator core lives in a dylib that the app loads at runtime (see
// EmulatorCore.swift), so its entry points are found with dlsym rather than
// declared here. Only the SDL shim below is part of the app binary.

typedef int32_t (*TouchHLEIOSRunGameFn)(
    const char *path,
    int32_t scale_hack,
    int32_t orientation,
    int32_t network_access,
    int32_t analog_stick_tilt_controls
);

int32_t touchhle_ios_launch_game(
    TouchHLEIOSRunGameFn run_game,
    const char *path,
    int32_t scale_hack,
    int32_t orientation,
    int32_t network_access,
    int32_t analog_stick_tilt_controls
);
