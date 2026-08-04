import Foundation

/// One of the emulator cores shipped inside the app.
///
/// The cores are forks of each other and export the same symbols, so they can
/// never be linked into one binary. Each is built as a dylib in
/// `touchHLE.app/Frameworks` instead, and the one a game needs is loaded with
/// `dlopen` when that game starts.
enum CoreKind: String, CaseIterable, Identifiable {
    case hyperHLE = "hyperhle"
    case touchHLE = "touchhle"

    var id: String { rawValue }

    var displayName: String {
        switch self {
        case .hyperHLE: return "HyperHLE"
        case .touchHLE: return "touchHLE"
        }
    }

    var version: String {
        switch self {
        case .hyperHLE: return "v1.0.6"
        case .touchHLE: return "0.2.3"
        }
    }

    var summary: String {
        switch self {
        case .hyperHLE:
            return "A fork of touchHLE that runs more games."
        case .touchHLE:
            return "The original emulator. Worth trying if a game misbehaves on HyperHLE."
        }
    }

    /// The cores understand different options, and an option a core does not
    /// recognise aborts the launch, so each reads its own defaults file out of
    /// the app bundle.
    var defaultOptionsFileName: String {
        switch self {
        case .hyperHLE: return "touchHLE_default_options.txt"
        case .touchHLE: return "touchHLE_default_options.touchhle.txt"
        }
    }

    /// Whether this core can get its executable memory right now.
    ///
    /// The cores differ, so this cannot be one global check. HyperHLE asks for
    /// JIT memory by executing oaknut's `brk #0xf00d` trap, which a debugger has
    /// to be attached to service - and TrollStore's "Enable JIT" detaches once
    /// it is done, so a process that is merely marked debuggable is not enough.
    /// touchHLE maps RWX directly, so for it CS_DEBUGGED (which survives the
    /// detach) does the job.
    var isJITSatisfied: Bool {
        switch self {
        case .hyperHLE:
            // oaknut normally gets its executable memory by trapping into a
            // debugger that implements a handler for `brk #0xf00d`, which only
            // StikDebug does. platform/ios/patches/oaknut-rwx-jit.patch makes it
            // prefer a plain RWX mapping when the process carries CS_DEBUGGED,
            // so TrollStore's JIT is enough as well - the same requirement as
            // the touchHLE core.
            return touchhle_ios_debugger_attached()
                || touchhle_ios_process_is_debuggable()
        case .touchHLE:
            return touchhle_ios_debugger_attached()
                || touchhle_ios_process_is_debuggable()
        }
    }

    /// What to tell the user when `isJITSatisfied` is false.
    var jitRequirementMessage: String {
        switch self {
        case .hyperHLE:
            return """
                HyperHLE needs JIT. Enable it through TrollStore or StikDebug and \
                start the game again.
                """
        case .touchHLE:
            return """
                touchHLE needs JIT. Enable it through TrollStore or StikDebug and \
                start the game again.
                """
        }
    }

    fileprivate var libraryName: String { "lib\(rawValue)_core.dylib" }

    fileprivate var libraryURL: URL? {
        Bundle.main.privateFrameworksURL?.appendingPathComponent(libraryName)
    }

    var isAvailable: Bool {
        guard let libraryURL else { return false }
        return FileManager.default.fileExists(atPath: libraryURL.path)
    }

    /// The cores this build actually shipped, in menu order.
    static var installed: [CoreKind] { allCases.filter(\.isAvailable) }
}

/// A loaded core dylib and the entry points looked up inside it.
final class EmulatorCore {
    typealias RunGame = @convention(c) (
        UnsafePointer<CChar>?, Int32, Int32, Int32, Int32
    ) -> Int32
    typealias MetadataCreate = @convention(c) (UnsafePointer<CChar>?) -> OpaquePointer?
    typealias MetadataString = @convention(c) (OpaquePointer?) -> UnsafePointer<CChar>?
    typealias MetadataNumber = @convention(c) (OpaquePointer?) -> UInt32
    typealias MetadataBytes = @convention(c) (OpaquePointer?) -> UnsafePointer<UInt8>?
    typealias MetadataFree = @convention(c) (OpaquePointer?) -> Void
    typealias RequestExit = @convention(c) () -> Void
    typealias CurrentFPS = @convention(c) () -> Float

    let kind: CoreKind
    let runGame: RunGame
    let metadataCreate: MetadataCreate
    let metadataDisplayName: MetadataString
    let metadataBundleIdentifier: MetadataString
    let metadataOrientationCapabilities: MetadataNumber
    let metadataIconRGBA: MetadataBytes
    let metadataIconWidth: MetadataNumber
    let metadataIconHeight: MetadataNumber
    let metadataFree: MetadataFree
    let requestExit: RequestExit
    let currentFPS: CurrentFPS

    /// Cores are never unloaded: a core that has run a game leaves SDL and its
    /// audio and CPU state behind it, and there is nothing to gain from
    /// reclaiming the mapping.
    private static var loaded: [CoreKind: EmulatorCore] = [:]

    /// The core currently running a game, if any. The exit button and the FPS
    /// counter have to talk to this one rather than the default.
    private(set) static var running: EmulatorCore?

    static func load(_ kind: CoreKind) throws -> EmulatorCore {
        if let core = loaded[kind] { return core }
        let core = try EmulatorCore(kind: kind)
        loaded[kind] = core
        return core
    }

    static func withRunning<T>(_ core: EmulatorCore, _ body: () -> T) -> T {
        running = core
        setenv("TOUCHHLE_DEFAULT_OPTIONS_FILE", core.kind.defaultOptionsFileName, 1)
        setenv("TOUCHHLE_IPA_CACHE_BUDGET_MIB", ipaCacheBudgetMiB(), 1)
        defer { running = nil }
        return body()
    }

    /// The decompressed-IPA cache limit to hand the core, in MiB, or "0" for
    /// unlimited.
    ///
    /// Content read out of a game's .ipa is decompressed into memory and kept
    /// there. Without a limit that grows with everything the game has ever
    /// opened, which is enough for an asset-heavy game to exceed iOS's
    /// per-process memory limit and be killed by jetsam. Turning the limit off
    /// restores the old cache-everything behaviour, which is faster for games
    /// that re-read one big resource bundle but unsafe for large ones.
    ///
    /// Passed through the environment rather than as a `runGame` argument so the
    /// C ABI stays identical for both cores; a core that does not know the
    /// variable simply ignores it.
    private static func ipaCacheBudgetMiB() -> String {
        let defaults = UserDefaults.standard
        if defaults.object(forKey: "limitIPACache") == nil {
            return String(defaultIPACacheBudgetMiB)
        }
        guard defaults.bool(forKey: "limitIPACache") else { return "0" }
        let configured = defaults.integer(forKey: "ipaCacheBudgetMiB")
        return String(configured > 0 ? configured : defaultIPACacheBudgetMiB)
    }

    static let defaultIPACacheBudgetMiB = 64

    private init(kind: CoreKind) throws {
        guard let libraryURL = kind.libraryURL else {
            throw CoreLoadError(kind: kind, reason: "the app bundle has no Frameworks folder")
        }

        // RTLD_LOCAL keeps the core's symbols out of the global namespace, so
        // loading a second core later cannot shadow the first one's.
        guard let handle = dlopen(libraryURL.path, RTLD_NOW | RTLD_LOCAL) else {
            let reason = dlerror().map { String(cString: $0) } ?? "unknown error"
            throw CoreLoadError(kind: kind, reason: reason)
        }

        func symbol<T>(_ name: String, as type: T.Type = T.self) throws -> T {
            guard let address = dlsym(handle, name) else {
                throw CoreLoadError(kind: kind, reason: "\(name) is missing")
            }
            return unsafeBitCast(address, to: type)
        }

        self.kind = kind
        runGame = try symbol("touchhle_ios_run_game")
        metadataCreate = try symbol("touchhle_ios_game_metadata_create")
        metadataDisplayName = try symbol("touchhle_ios_game_metadata_display_name")
        metadataBundleIdentifier = try symbol("touchhle_ios_game_metadata_bundle_identifier")
        metadataOrientationCapabilities =
            try symbol("touchhle_ios_game_metadata_orientation_capabilities")
        metadataIconRGBA = try symbol("touchhle_ios_game_metadata_icon_rgba")
        metadataIconWidth = try symbol("touchhle_ios_game_metadata_icon_width")
        metadataIconHeight = try symbol("touchhle_ios_game_metadata_icon_height")
        metadataFree = try symbol("touchhle_ios_game_metadata_free")
        requestExit = try symbol("touchhle_ios_request_exit")
        currentFPS = try symbol("touchhle_ios_current_fps")
    }
}

struct CoreLoadError: LocalizedError {
    let kind: CoreKind
    let reason: String

    var errorDescription: String? {
        "The \(kind.displayName) core could not be loaded: \(reason)"
    }
}

/// Which core runs which game.
///
/// A game that only works on one core should stay on it, so the choice is per
/// game, keyed by the guest app's bundle identifier, with a global default for
/// everything that has never been set.
enum CoreSelection {
    private static let defaultCoreKey = "defaultCore"
    private static let overridesKey = "coreOverrides"

    static var defaultKind: CoreKind {
        get { kind(forStoredDefault: UserDefaults.standard.string(forKey: defaultCoreKey)) }
        set { UserDefaults.standard.set(newValue.rawValue, forKey: defaultCoreKey) }
    }

    /// Resolves a stored default. Views pass the value they read through
    /// `@AppStorage` so that SwiftUI redraws them when it changes.
    static func kind(forStoredDefault stored: String?) -> CoreKind {
        if let stored, let kind = CoreKind(rawValue: stored), kind.isAvailable {
            return kind
        }
        return CoreKind.installed.first ?? .hyperHLE
    }

    static func kind(forBundleIdentifier bundleIdentifier: String?) -> CoreKind {
        guard let bundleIdentifier,
              let stored = overrides[bundleIdentifier],
              let kind = CoreKind(rawValue: stored),
              kind.isAvailable
        else {
            return defaultKind
        }
        return kind
    }

    /// `nil` means "follow the default".
    static func override(forBundleIdentifier bundleIdentifier: String?) -> CoreKind? {
        guard let bundleIdentifier, let stored = overrides[bundleIdentifier] else { return nil }
        return CoreKind(rawValue: stored)
    }

    static func setOverride(_ kind: CoreKind?, forBundleIdentifier bundleIdentifier: String?) {
        guard let bundleIdentifier else { return }
        var overrides = self.overrides
        overrides[bundleIdentifier] = kind?.rawValue
        UserDefaults.standard.set(overrides, forKey: overridesKey)
    }

    private static var overrides: [String: String] {
        UserDefaults.standard.dictionary(forKey: overridesKey) as? [String: String] ?? [:]
    }
}
