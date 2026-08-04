import SwiftUI
import UniformTypeIdentifiers
import UIKit

// MARK: - Back-deployment shims
//
// The deployment target is iOS 15.0 so that TrollStore devices are supported.
// Anything newer has to sit behind `#available`, including types like
// NavigationStack that would otherwise fail to resolve when the process starts.

/// `NavigationStack` on iOS 16+, `NavigationView` in stack style on iOS 15.
private struct NavigationContainer<Content: View>: View {
    @ViewBuilder var content: () -> Content

    var body: some View {
        if #available(iOS 16.0, *) {
            NavigationStack(root: content)
        } else {
            NavigationView(content: content)
                .navigationViewStyle(.stack)
        }
    }
}

/// `ContentUnavailableView` on iOS 17+, a hand-rolled equivalent on iOS 15/16.
private struct EmptyLibraryPlaceholder: View {
    private let title = "No Games Yet"
    private let message = "Import a 32-bit iPhone game to add it to your library."

    var body: some View {
        if #available(iOS 17.0, *) {
            ContentUnavailableView {
                Label(title, systemImage: "gamecontroller")
            } description: {
                Text(message)
            }
        } else {
            VStack(spacing: 10) {
                Image(systemName: "gamecontroller")
                    .font(.system(size: 46, weight: .medium))
                    .foregroundStyle(.secondary)
                Text(title)
                    .font(.title2.weight(.semibold))
                Text(message)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
            }
            .padding(.horizontal, 40)
        }
    }
}

private extension View {
    /// `onChange(of:)` without the iOS 17 two-parameter signature, which does
    /// not exist on iOS 15/16.
    @ViewBuilder
    func touchHLEOnChange<Value: Equatable>(
        of value: Value,
        perform action: @escaping (Value) -> Void
    ) -> some View {
        if #available(iOS 17.0, *) {
            onChange(of: value) { _, newValue in action(newValue) }
        } else {
            onChange(of: value, perform: action)
        }
    }
}

/// UIKit rotation, which on iOS 16+ is driven by `requestGeometryUpdate` and
/// `setNeedsUpdateOfSupportedInterfaceOrientations`. Neither exists on iOS 15,
/// so there we set the device orientation and re-ask UIKit to rotate.
@MainActor
private func touchHLEApplyOrientation(
    _ mask: UIInterfaceOrientationMask,
    viewController: UIViewController?,
    scene: UIWindowScene?
) {
    if #available(iOS 16.0, *) {
        viewController?.setNeedsUpdateOfSupportedInterfaceOrientations()
        scene?.requestGeometryUpdate(.iOS(interfaceOrientations: mask))
    } else {
        // UIDevice.orientation is read-only in the public API; UIKit backs it
        // with a setter that KVC can reach. Check before poking it so a future
        // OS that drops the setter degrades instead of raising.
        let device = UIDevice.current
        if let deviceOrientation = touchHLEDeviceOrientation(for: mask),
           device.responds(to: NSSelectorFromString("setOrientation:")) {
            device.setValue(deviceOrientation.rawValue, forKey: "orientation")
        }
        UIViewController.attemptRotationToDeviceOrientation()
    }
}

/// Interface and device orientations are mirrored for the landscape cases.
private func touchHLEDeviceOrientation(
    for mask: UIInterfaceOrientationMask
) -> UIDeviceOrientation? {
    if mask.contains(.landscapeLeft) { return .landscapeRight }
    if mask.contains(.landscapeRight) { return .landscapeLeft }
    if mask.contains(.portrait) { return .portrait }
    return nil
}


/// Values stored in the "orientation" setting. These are the user's choice, not
/// the guest orientation codes the emulator takes - `launchOrientation` maps
/// between them.
private enum OrientationSetting {
    static let automatic = 0
    static let landscapeLeft = 1
    static let landscapeRight = 2
    // 3 is skipped on purpose: the emulator maps that to --upside-down.
    static let portrait = 4
}

private struct GameFile: Identifiable {
    let url: URL
    let displayName: String
    let bundleIdentifier: String?
    let orientationCapabilities: UInt32
    let icon: UIImage?

    var id: String { url.path }

    func launchOrientation(
        override orientation: Int,
        currentInterfaceOrientation: UIInterfaceOrientation
    ) -> Int {
        let supportsPortrait = orientationCapabilities & 1 != 0
        let supportsLandscape = orientationCapabilities & 2 != 0
        // An explicit landscape or portrait choice overrides what the device is
        // doing, but never what a single-orientation bundle declares.
        let isExplicitLandscape = orientation == OrientationSetting.landscapeLeft
            || orientation == OrientationSetting.landscapeRight
        let isExplicitPortrait = orientation == OrientationSetting.portrait

        if supportsPortrait && !supportsLandscape {
            return 0
        }
        if supportsLandscape && !supportsPortrait {
            if isExplicitLandscape {
                return orientation
            }
            // Some games advertise landscape but render portrait anyway, so
            // allow forcing it.
            if isExplicitPortrait {
                return 0
            }
            return currentInterfaceOrientation == .landscapeRight ? 2 : 1
        }
        if isExplicitLandscape {
            return orientation
        }
        if isExplicitPortrait {
            return 0
        }
        switch currentInterfaceOrientation {
        case .landscapeLeft:
            return 1
        case .landscapeRight:
            return 2
        default:
            return 0
        }
    }
}

/// A launch held back because JIT is not enabled, kept so it can be started
/// unchanged if the user goes ahead anyway.
private struct HeldLaunch: Identifiable {
    let game: GameFile
    let scaleHack: Int
    let orientation: Int
    let networkAccess: Bool
    let analogTilt: Bool

    var id: String { game.id }
}

@MainActor
private final class GameLibrary: ObservableObject {
    @Published var games: [GameFile] = []
    @Published var importError: String?
    @Published var launchError: String?
    @Published var heldLaunch: HeldLaunch?
    @Published var isLaunching = false

    let appsDirectory: URL

    /// The core used to read game metadata. Whichever core will actually run a
    /// game is loaded when it starts. If the default core cannot be loaded, any
    /// other core reports the same things, and a library with names and icons
    /// beats one without.
    private var metadataCore: EmulatorCore? {
        let preferred = CoreSelection.defaultKind
        if let core = try? EmulatorCore.load(preferred) {
            return core
        }
        return CoreKind.installed
            .filter { $0 != preferred }
            .lazy
            .compactMap { try? EmulatorCore.load($0) }
            .first
    }

    init() {
        let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        appsDirectory = documents.appendingPathComponent("touchHLE_apps", isDirectory: true)
        migrateLegacyNetworkSetting(in: documents)
        reload()
    }

    func reload() {
        do {
            try FileManager.default.createDirectory(
                at: appsDirectory,
                withIntermediateDirectories: true
            )
            games = try FileManager.default.contentsOfDirectory(
                at: appsDirectory,
                includingPropertiesForKeys: nil,
                options: [.skipsHiddenFiles]
            )
            .filter { ["ipa", "app"].contains($0.pathExtension.lowercased()) }
            .map(gameFile(from:))
            .sorted {
                $0.displayName.localizedCaseInsensitiveCompare($1.displayName) == .orderedAscending
            }
        } catch {
            importError = error.localizedDescription
        }
    }

    func importGame(from sourceURL: URL) {
        let hasAccess = sourceURL.startAccessingSecurityScopedResource()
        defer {
            if hasAccess {
                sourceURL.stopAccessingSecurityScopedResource()
            }
        }

        do {
            try FileManager.default.createDirectory(
                at: appsDirectory,
                withIntermediateDirectories: true
            )
            let destinationURL = uniqueDestination(for: sourceURL.lastPathComponent)
            try FileManager.default.copyItem(at: sourceURL, to: destinationURL)
            reload()
        } catch {
            importError = error.localizedDescription
        }
    }

    func delete(_ game: GameFile) {
        do {
            try FileManager.default.removeItem(at: game.url)
            reload()
        } catch {
            importError = error.localizedDescription
        }
    }

    func launch(
        _ game: GameFile,
        scaleHack: Int,
        orientation: Int,
        networkAccess: Bool,
        analogTilt: Bool
    ) {
        // Which core will run this game decides what JIT it needs, so resolve
        // that before gating rather than applying one rule to both.
        let kind = CoreSelection.kind(forBundleIdentifier: game.bundleIdentifier)
        guard kind.isJITSatisfied else {
            heldLaunch = HeldLaunch(
                game: game,
                scaleHack: scaleHack,
                orientation: orientation,
                networkAccess: networkAccess,
                analogTilt: analogTilt
            )
            return
        }

        start(
            game,
            scaleHack: scaleHack,
            orientation: orientation,
            networkAccess: networkAccess,
            analogTilt: analogTilt
        )
    }

    func start(_ held: HeldLaunch) {
        start(
            held.game,
            scaleHack: held.scaleHack,
            orientation: held.orientation,
            networkAccess: held.networkAccess,
            analogTilt: held.analogTilt
        )
    }

    private func start(
        _ game: GameFile,
        scaleHack: Int,
        orientation: Int,
        networkAccess: Bool,
        analogTilt: Bool
    ) {
        let core: EmulatorCore
        do {
            core = try EmulatorCore.load(
                CoreSelection.kind(forBundleIdentifier: game.bundleIdentifier)
            )
        } catch {
            launchError = error.localizedDescription
            return
        }

        isLaunching = true

        DispatchQueue.main.asyncAfter(deadline: .now() + 0.12) { [weak self] in
            guard let self else { return }
            let launchOrientation = game.launchOrientation(
                override: orientation,
                currentInterfaceOrientation: TouchHLENativeHost.currentInterfaceOrientation
            )
            TouchHLENativeHost.hideHostWindow()
            TouchHLENativeHost.prepareGameControls(
                launchOrientation: launchOrientation
            ) { [weak self] in
                guard let self else { return }
                let result = EmulatorCore.withRunning(core) {
                    game.url.path.withCString { path in
                        touchhle_ios_launch_game(
                            core.runGame,
                            path,
                            Int32(scaleHack),
                            Int32(launchOrientation),
                            networkAccess ? 1 : 0,
                            analogTilt ? 1 : 0
                        )
                    }
                }

                TouchHLENativeHost.hideGameControls()
                TouchHLENativeHost.restoreHostWindow()
                self.isLaunching = false
                if result != 0 {
                    self.launchError = "\(core.kind.displayName) could not start this game. The diagnostic log has been saved in Files."
                }
            }
        }
    }

    private func uniqueDestination(for fileName: String) -> URL {
        let original = appsDirectory.appendingPathComponent(fileName)
        guard FileManager.default.fileExists(atPath: original.path) else {
            return original
        }

        let source = URL(fileURLWithPath: fileName)
        let stem = source.deletingPathExtension().lastPathComponent
        let fileExtension = source.pathExtension
        var index = 2

        while true {
            let candidateName = fileExtension.isEmpty
                ? "\(stem) \(index)"
                : "\(stem) \(index).\(fileExtension)"
            let candidate = appsDirectory.appendingPathComponent(candidateName)
            if !FileManager.default.fileExists(atPath: candidate.path) {
                return candidate
            }
            index += 1
        }
    }

    private func gameFile(from url: URL) -> GameFile {
        let fallbackName = url.deletingPathExtension().lastPathComponent
        // Reading a game's name, icon and orientations does not run it, and the
        // cores all report the same things, so the default core does this for
        // the whole library rather than loading every core up front.
        guard let core = metadataCore,
              let metadata = url.path.withCString({ core.metadataCreate($0) })
        else {
            return GameFile(
                url: url,
                displayName: fallbackName,
                bundleIdentifier: nil,
                orientationCapabilities: 1,
                icon: nil
            )
        }
        defer { core.metadataFree(metadata) }

        let metadataDisplayName = core.metadataDisplayName(metadata)
            .map { String(cString: $0) } ?? fallbackName
        let displayName = preferredDisplayName(
            metadataName: metadataDisplayName,
            fallbackName: fallbackName
        )
        let bundleIdentifier = core.metadataBundleIdentifier(metadata)
            .map { String(cString: $0) }
        let orientationCapabilities = core.metadataOrientationCapabilities(metadata)

        return GameFile(
            url: url,
            displayName: displayName,
            bundleIdentifier: bundleIdentifier,
            orientationCapabilities: orientationCapabilities,
            icon: gameIcon(from: metadata, core: core)
        )
    }

    private func preferredDisplayName(metadataName: String, fallbackName: String) -> String {
        let metadataIsAbbreviated = metadataName.contains("...") || metadataName.contains("…")
        let fallbackIsComplete = !fallbackName.contains("...") && !fallbackName.contains("…")
        guard metadataIsAbbreviated, fallbackIsComplete else { return metadataName }

        return fallbackName.replacingOccurrences(of: "_", with: " ")
    }

    private func gameIcon(from metadata: OpaquePointer, core: EmulatorCore) -> UIImage? {
        let width = Int(core.metadataIconWidth(metadata))
        let height = Int(core.metadataIconHeight(metadata))
        guard width > 0,
              height > 0,
              let pixels = core.metadataIconRGBA(metadata)
        else {
            return nil
        }

        let data = Data(bytes: pixels, count: width * height * 4)
        guard let provider = CGDataProvider(data: data as CFData),
              let image = CGImage(
                width: width,
                height: height,
                bitsPerComponent: 8,
                bitsPerPixel: 32,
                bytesPerRow: width * 4,
                space: CGColorSpaceCreateDeviceRGB(),
                bitmapInfo: [
                    CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedLast.rawValue),
                    .byteOrder32Big
                ],
                provider: provider,
                decode: nil,
                shouldInterpolate: true,
                intent: .defaultIntent
              )
        else {
            return nil
        }

        return UIImage(cgImage: image)
    }

    private func migrateLegacyNetworkSetting(in documents: URL) {
        let defaults = UserDefaults.standard
        guard defaults.object(forKey: "networkAccess") == nil else { return }

        let legacyURL = documents.appendingPathComponent(".touchHLE_network_access")
        guard let value = try? String(contentsOf: legacyURL, encoding: .utf8) else { return }
        defaults.set(value.trimmingCharacters(in: .whitespacesAndNewlines) == "enabled", forKey: "networkAccess")
    }
}

private final class GameControlsWindow: UIWindow {
    weak var interactiveView: UIView?

    // Only `hitTest` may be overridden here. Overriding `point(inside:)` on a
    // UIWindow also changes how UIKit picks which window an event belongs to,
    // which stops *every* window in the scene (including the SDL game window)
    // from receiving touches. Letting `super.hitTest` do the work and merely
    // filtering the result keeps the pass-through behaviour without that.
    override func hitTest(_ point: CGPoint, with event: UIEvent?) -> UIView? {
        guard let hitView = super.hitTest(point, with: event),
              let interactiveView,
              hitView === interactiveView || hitView.isDescendant(of: interactiveView)
        else {
            return nil
        }
        return hitView
    }
}

private final class GameControlsViewController: UIViewController {
    var allowedOrientations: UIInterfaceOrientationMask = .portrait
    var onExit: (() -> Void)?

    private(set) lazy var exitButton: UIButton = {
        let button = UIButton(type: .system)
        var configuration: UIButton.Configuration
        if #available(iOS 26.0, *) {
            configuration = .glass()
        } else {
            configuration = .gray()
        }
        let symbolConfiguration = UIImage.SymbolConfiguration(pointSize: 19, weight: .bold)
        configuration.image = UIImage(
            systemName: "rectangle.portrait.and.arrow.right",
            withConfiguration: symbolConfiguration
        )?.withTintColor(.systemRed, renderingMode: .alwaysOriginal)
        configuration.cornerStyle = .capsule
        configuration.baseForegroundColor = .systemRed
        button.configuration = configuration
        button.tintColor = .systemRed
        button.accessibilityLabel = "Exit Game"
        button.accessibilityHint = "Stops the game and returns to your library"
        button.addTarget(self, action: #selector(exitGame), for: .touchUpInside)
        button.translatesAutoresizingMaskIntoConstraints = false
        return button
    }()

    private lazy var fpsIndicator: UIButton = {
        let indicator = UIButton(type: .system)
        var configuration: UIButton.Configuration
        if #available(iOS 26.0, *) {
            configuration = .glass()
        } else {
            configuration = .gray()
        }
        configuration.title = "— FPS"
        configuration.cornerStyle = .capsule
        configuration.baseForegroundColor = .systemYellow
        indicator.configuration = configuration
        indicator.isUserInteractionEnabled = false
        indicator.accessibilityLabel = "Frame rate"
        indicator.translatesAutoresizingMaskIntoConstraints = false
        return indicator
    }()

    private var fpsTimer: Timer?

    override var supportedInterfaceOrientations: UIInterfaceOrientationMask {
        allowedOrientations
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .clear
        view.addSubview(exitButton)

        NSLayoutConstraint.activate([
            exitButton.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 10),
            exitButton.trailingAnchor.constraint(equalTo: view.trailingAnchor, constant: -16),
            exitButton.widthAnchor.constraint(equalToConstant: 48),
            exitButton.heightAnchor.constraint(equalToConstant: 48)
        ])

        guard UserDefaults.standard.bool(forKey: "showFPSOverlay") else { return }

        view.addSubview(fpsIndicator)
        NSLayoutConstraint.activate([
            fpsIndicator.centerYAnchor.constraint(equalTo: exitButton.centerYAnchor),
            fpsIndicator.trailingAnchor.constraint(equalTo: exitButton.leadingAnchor, constant: -8),
            fpsIndicator.heightAnchor.constraint(equalToConstant: 48)
        ])

        updateFPS()
        fpsTimer = Timer.scheduledTimer(
            timeInterval: 0.5,
            target: self,
            selector: #selector(updateFPS),
            userInfo: nil,
            repeats: true
        )
        RunLoop.main.add(fpsTimer!, forMode: .common)
    }

    deinit {
        fpsTimer?.invalidate()
    }

    @objc private func exitGame() {
        exitButton.isEnabled = false
        onExit?()
    }

    @objc private func updateFPS() {
        let fps = EmulatorCore.running?.currentFPS() ?? 0
        fpsIndicator.configuration?.title = fps > 0 ? "\(Int(fps.rounded())) FPS" : "— FPS"
        fpsIndicator.accessibilityValue = fps > 0 ? "\(Int(fps.rounded())) frames per second" : "Unavailable"
    }
}

@objc(TouchHLENativeHost)
final class TouchHLENativeHost: NSObject {
    private static let shared = TouchHLENativeHost()
    private var window: UIWindow?
    private var gameControlsWindow: GameControlsWindow?

    @MainActor
    static var currentInterfaceOrientation: UIInterfaceOrientation {
        shared.window?.windowScene?.interfaceOrientation ?? .portrait
    }

    @MainActor
    @objc class func start() {
        shared.presentLibrary()
    }

    @MainActor
    static func restoreHostWindow() {
        guard let window = shared.window else { return }
        window.makeKeyAndVisible()
        touchHLEApplyOrientation(
            .portrait,
            viewController: window.rootViewController,
            scene: window.windowScene
        )
    }

    @MainActor
    static func hideHostWindow() {
        shared.window?.isHidden = true
    }

    @MainActor
    static func prepareGameControls(
        launchOrientation: Int,
        completion: @escaping @MainActor () -> Void
    ) {
        shared.presentGameControls(
            launchOrientation: launchOrientation,
            completion: completion
        )
    }

    @MainActor
    static func hideGameControls() {
        shared.dismissGameControls()
    }

    @MainActor
    private func presentLibrary() {
        guard let windowScene = UIApplication.shared.connectedScenes
            .compactMap({ $0 as? UIWindowScene })
            .first else {
            return
        }

        let window = UIWindow(windowScene: windowScene)
        window.rootViewController = UIHostingController(rootView: LibraryView())
        window.tintColor = .systemBlue
        window.makeKeyAndVisible()
        self.window = window
    }

    @MainActor
    private func presentGameControls(
        launchOrientation: Int,
        completion: @escaping @MainActor () -> Void
    ) {
        guard gameControlsWindow == nil,
              let windowScene = window?.windowScene
        else {
            completion()
            return
        }

        let controlsWindow = GameControlsWindow(windowScene: windowScene)
        controlsWindow.windowLevel = UIWindow.Level.normal + 3
        controlsWindow.accessibilityIdentifier = "touchHLE.gameControls"
        controlsWindow.backgroundColor = .clear

        let viewController = GameControlsViewController()
        let launchOrientationMask: UIInterfaceOrientationMask
        switch launchOrientation {
        case 1:
            launchOrientationMask = .landscapeLeft
        case 2:
            launchOrientationMask = .landscapeRight
        default:
            launchOrientationMask = .portrait
        }
        viewController.allowedOrientations = launchOrientationMask
        viewController.onExit = { [weak self] in
            self?.returnToLibrary()
        }

        controlsWindow.rootViewController = viewController
        viewController.loadViewIfNeeded()
        controlsWindow.interactiveView = viewController.exitButton
        controlsWindow.isHidden = false
        gameControlsWindow = controlsWindow

        touchHLEApplyOrientation(
            launchOrientationMask,
            viewController: viewController,
            scene: windowScene
        )
        waitForGameSurface(
            windowScene: windowScene,
            orientationMask: launchOrientationMask,
            expectsLandscape: launchOrientation == 1 || launchOrientation == 2,
            remainingAttempts: 30,
            completion: completion
        )
    }

    @MainActor
    private func waitForGameSurface(
        windowScene: UIWindowScene,
        orientationMask: UIInterfaceOrientationMask,
        expectsLandscape: Bool,
        remainingAttempts: Int,
        completion: @escaping @MainActor () -> Void
    ) {
        let bounds = windowScene.coordinateSpace.bounds
        let hasExpectedShape = expectsLandscape
            ? bounds.width > bounds.height
            : bounds.height >= bounds.width
        let hasExpectedOrientation = expectsLandscape
            ? windowScene.interfaceOrientation.isLandscape
            : windowScene.interfaceOrientation.isPortrait

        if (hasExpectedShape && hasExpectedOrientation) || remainingAttempts == 0 {
            gameControlsWindow?.frame = bounds
            gameControlsWindow?.layoutIfNeeded()
            if let viewController = gameControlsWindow?.rootViewController as? GameControlsViewController {
                viewController.allowedOrientations = orientationMask
                touchHLEApplyOrientation(
                    orientationMask,
                    viewController: viewController,
                    scene: nil
                )
            }
            print(
                "touchHLE game surface ready: orientation=\(windowScene.interfaceOrientation.rawValue) " +
                "bounds=\(Int(bounds.width))x\(Int(bounds.height))"
            )
            // `interfaceOrientation` flips as soon as the rotation is applied,
            // but UIKit's scene-geometry transaction can still be in flight.
            // The emulator takes the main thread and never gives it back, so if
            // we start it mid-transition the scene never finishes rotating and
            // UIKit stops dispatching touches to every window in it — timers
            // and hit-testing keep working, which is what made this so hard to
            // see. Give the transition run-loop turns to commit first.
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.0) {
                completion()
            }
            return
        }

        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) { [weak self] in
            self?.waitForGameSurface(
                windowScene: windowScene,
                orientationMask: orientationMask,
                expectsLandscape: expectsLandscape,
                remainingAttempts: remainingAttempts - 1,
                completion: completion
            )
        }
    }

    @MainActor
    private func dismissGameControls() {
        gameControlsWindow?.isHidden = true
        gameControlsWindow?.rootViewController = nil
        gameControlsWindow = nil
    }

    @MainActor
    @objc private func returnToLibrary() {
        EmulatorCore.running?.requestExit()
    }
}

private struct LibraryView: View {
    @StateObject private var library = GameLibrary()
    @State private var showingImporter = false
    @State private var showingSettings = false
    @State private var showingAbout = false
    @Environment(\.scenePhase) private var scenePhase
    @Environment(\.openURL) private var openURL

    @AppStorage("scaleHack") private var scaleHack = 3
    @AppStorage("orientation") private var orientation = 0
    @AppStorage("networkAccess") private var networkAccess = false
    @AppStorage("analogTilt") private var analogTilt = true
    @AppStorage("autoEnableJIT") private var autoEnableJIT = true

    @ObservedObject private var jit = JITStatus.shared

    private static let ipaType = UTType(filenameExtension: "ipa") ?? .archive

    var body: some View {
        NavigationContainer {
            libraryContent
        }
        .touchHLEOnChange(of: scenePhase) { newPhase in
            if newPhase == .active {
                library.reload()
                // Returning from TrollStore looks exactly like a debugger
                // having attached while we were backgrounded.
                jit.refresh()
                autoEnableJITIfNeeded()
            }
        }
    }

    /// Hand off to TrollStore on launch so tapping the app icon is enough. The
    /// round trip is visible - TrollStore comes to the foreground briefly and
    /// bounces straight back - but it needs no interaction.
    ///
    /// Runs at most once per process. If the handoff does not work (TrollStore
    /// missing, or its URL scheme switched off) nothing happens and the bolt
    /// button remains as the manual route.
    private func autoEnableJITIfNeeded() {
        guard autoEnableJIT,
              !jit.isAvailable,
              !jit.hasAttemptedAutoEnable,
              let provider = JITProvider.supported.first,
              let url = provider.enableJITURL
        else {
            return
        }

        jit.markAutoEnableAttempted()
        // openURL is dropped if it fires while the scene is still settling into
        // the foreground, so let the launch finish first.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
            openURL(url) { _ in }
        }
    }

    // Split out of `body`: with NavigationContainer wrapping it, the whole
    // expression was large enough that the type checker gave up on it.
    @ViewBuilder
    private var libraryContent: some View {
        ZStack {
            LibraryBackground()

            if library.games.isEmpty {
                EmptyLibraryPlaceholder()
            } else {
                ScrollView {
                    LazyVGrid(
                        columns: [GridItem(.adaptive(minimum: 150), spacing: 16)],
                        spacing: 16
                    ) {
                        ForEach(library.games) { game in
                            GameCard(
                                game: game,
                                launch: {
                                    library.launch(
                                        game,
                                        scaleHack: scaleHack,
                                        orientation: orientation,
                                        networkAccess: networkAccess,
                                        analogTilt: analogTilt
                                    )
                                },
                                delete: { library.delete(game) }
                            )
                        }
                    }
                    .padding(.horizontal, 18)
                    .padding(.top, 12)
                    .padding(.bottom, 110)
                }
                .refreshable {
                    library.reload()
                }
            }
        }
        .navigationTitle("Library")
        .toolbar {
            ToolbarItem(placement: .topBarLeading) {
                Button {
                    showingAbout = true
                } label: {
                    Label("About", systemImage: "info.circle")
                }
            }

            ToolbarItemGroup(placement: .topBarTrailing) {
                EnableJITButton()

                Button {
                    showingSettings = true
                } label: {
                    Label("Settings", systemImage: "gearshape")
                }
            }
        }
        .safeAreaInset(edge: .bottom) {
            Button {
                showingImporter = true
            } label: {
                Label("Import Game", systemImage: "plus")
                    .font(.headline)
                    .padding(.horizontal, 22)
                    .padding(.vertical, 13)
            }
            .buttonStyle(.plain)
            .touchHLEImportButtonStyle()
            .padding(.bottom, 8)
        }
        .fileImporter(
            isPresented: $showingImporter,
            allowedContentTypes: [Self.ipaType],
            allowsMultipleSelection: false
        ) { result in
            switch result {
            case .success(let urls):
                if let url = urls.first {
                    library.importGame(from: url)
                }
            case .failure(let error):
                library.importError = error.localizedDescription
            }
        }
        .sheet(isPresented: $showingSettings) {
            SettingsView()
        }
        .sheet(isPresented: $showingAbout) {
            AboutView()
        }
        .alert("Couldn’t Import Game", isPresented: errorBinding(for: $library.importError)) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(library.importError ?? "Unknown error")
        }
        .alert("Game Couldn’t Start", isPresented: errorBinding(for: $library.launchError)) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(library.launchError ?? "Unknown error")
        }
        .alert(
            "JIT Isn’t Enabled",
            isPresented: heldLaunchBinding,
            presenting: library.heldLaunch
        ) { held in
            Button("Enable JIT") {
                if let url = StikDebug.enableJITURL {
                    openURL(url)
                }
            }
            Button("Start Anyway", role: .destructive) {
                library.start(held)
            }
            Button("Cancel", role: .cancel) {}
        } message: { _ in
            Text("Games need JIT, and without it touchHLE closes the moment one starts. Enable JIT in StikDebug, then start the game again.")
        }
        .overlay {
            if library.isLaunching {
                VStack(spacing: 12) {
                    ProgressView()
                    Text("Starting game…")
                        .font(.headline)
                }
                .padding(24)
                .touchHLELaunchOverlayStyle()
            }
        }
    }

    private var heldLaunchBinding: Binding<Bool> {
        Binding(
            get: { library.heldLaunch != nil },
            set: { isPresented in
                if !isPresented {
                    library.heldLaunch = nil
                }
            }
        )
    }

    private func errorBinding(for error: Binding<String?>) -> Binding<Bool> {
        Binding(
            get: { error.wrappedValue != nil },
            set: { isPresented in
                if !isPresented {
                    error.wrappedValue = nil
                }
            }
        )
    }
}

private enum StikDebug {
    /// Asks StikDebug to attach to this app and enable JIT.
    static var enableJITURL: URL? {
        guard let bundleIdentifier = Bundle.main.bundleIdentifier else { return nil }

        var components = URLComponents()
        components.scheme = "stikdebug"
        components.host = "enable-jit"
        components.queryItems = [
            URLQueryItem(name: "bundle-id", value: bundleIdentifier),
            URLQueryItem(name: "script-name", value: "universal.js")
        ]
        return components.url
    }
}

/// Live JIT state, so the verdict can be inspected on device rather than
/// guessed at, and re-checked after a handoff without relaunching.
@MainActor
private final class JITStatus: ObservableObject {
    static let shared = JITStatus()

    @Published private(set) var isAvailable = touchhle_ios_jit_available()
    @Published private(set) var diagnostics: [(String, String)] = JITStatus.read()

    /// This core always needs a debugger attached to service oaknut's JIT trap,
    /// so JIT is never permanent here however the app was signed.
    var isPermanent: Bool { false }

    /// Debugger-granted JIT dies with the process, so the handoff has to be
    /// redone on every cold start. Auto-triggering it once per launch makes
    /// that invisible - but only once, so a failed handoff cannot bounce the
    /// user back and forth forever.
    private(set) var hasAttemptedAutoEnable = false

    func markAutoEnableAttempted() {
        hasAttemptedAutoEnable = true
    }

    func refresh() {
        touchhle_ios_log_jit_status("refresh")
        let available = touchhle_ios_jit_available()
        if available != isAvailable {
            isAvailable = available
        }
        diagnostics = JITStatus.read()
    }

    static func read() -> [(String, String)] {
        var raw = TouchHLEJITDiagnostics()
        touchhle_ios_jit_diagnostics(&raw)
        return [
            // CS_DEBUGGED first: it is the one that survives TrollStore
            // detaching, so it is what usually decides the verdict.
            ("CS_DEBUGGED", raw.cs_debugged ? "yes" : "no"),
            ("cs_flags", String(format: "0x%08X", raw.cs_flags)),
            ("csops", raw.csops_result == 0 ? "ok" : "failed"),
            ("Debugger attached (P_TRACED)", raw.traced ? "yes" : "no"),
            ("p_flag", String(format: "0x%08X", raw.proc_flags)),
            ("sysctl", raw.sysctl_result == 0
                ? "ok"
                : "failed (errno \(raw.sysctl_errno))"),
            ("dynamic-codesigning", raw.has_dynamic_codesigning ? "yes" : "no"),
            // Neither probe can detect JIT on iOS - see touchhle_ios_jit_available.
            ("mmap RWX (not a test)", raw.mmap_rwx_ok ? "ok" : "denied"),
            ("mprotect R+X (not a test)", raw.mprotect_exec_ok ? "ok" : "denied")
        ]
    }
}

private struct JITDiagnosticsSection: View {
    @ObservedObject private var jit = JITStatus.shared
    @AppStorage("defaultCore") private var defaultCoreRaw = ""

    /// Reported per core: the two have different requirements, and a global
    /// "enabled" was telling users HyperHLE was ready when it was not.
    private var core: CoreKind {
        CoreSelection.kind(forStoredDefault: defaultCoreRaw)
    }

    var body: some View {
        Section {
            HStack {
                Text("Status")
                Spacer()
                Text(statusText)
                    .foregroundStyle(jit.isAvailable ? .green : .red)
            }
            .accessibilityElement(children: .combine)

            Button {
                jit.refresh()
            } label: {
                Label("Re-check Now", systemImage: "arrow.clockwise")
            }
        } header: {
            Text("JIT Status — \(core.displayName)")
        } footer: {
            Text(footerText)
        }
        .onAppear { jit.refresh() }

        Section {
            ForEach(jit.diagnostics, id: \.0) { name, value in
                HStack {
                    Text(name)
                    Spacer()
                    Text(value)
                        .foregroundStyle(.secondary)
                        .font(.callout.monospaced())
                }
                .accessibilityElement(children: .combine)
            }
        } header: {
            Text("JIT Diagnostics")
        } footer: {
            Text("Raw signals behind the verdict above. Include these when reporting that JIT will not turn on. They are also written to touchhle-host.log.")
        }
    }

    private var statusText: String {
        core.isJITSatisfied ? "Ready" : "Not available"
    }

    private var footerText: String {
        if core.isJITSatisfied {
            return """
                \(core.displayName) can get executable memory right now. This has to \
                be re-established whenever the app starts as a new process.
                """
        }
        return core.jitRequirementMessage
    }
}

private enum TrollStore {
    /// Asks TrollStore to attach to this app and enable JIT. Needs TrollStore
    /// 2.0.12 or newer with its URL scheme turned on, and the app must declare
    /// get-task-allow - see Config/TouchHLEHost-TrollStore.entitlements.
    static var enableJITURL: URL? {
        guard let bundleIdentifier = Bundle.main.bundleIdentifier else { return nil }

        var components = URLComponents()
        components.scheme = "apple-magnifier"
        components.host = "enable-jit"
        components.queryItems = [
            URLQueryItem(name: "bundle-id", value: bundleIdentifier)
        ]
        return components.url
    }
}

/// An external app that can hand this process JIT by attaching a debugger.
/// Both routes need `get-task-allow`, and both are per-process: JIT has to be
/// re-established every time touchHLE launches fresh.
private enum JITProvider: String, Identifiable, CaseIterable {
    case trollStore
    case stikDebug

    var id: String { rawValue }

    /// StikDebug's own minimum is iOS 17.4; below that TrollStore is the only
    /// route, so there is no point offering a choice.
    static var supported: [JITProvider] {
        if #available(iOS 17.4, *) {
            return [.trollStore, .stikDebug]
        }
        return [.trollStore]
    }

    var displayName: String {
        switch self {
        case .trollStore: return "TrollStore"
        case .stikDebug: return "StikDebug"
        }
    }

    var enableJITURL: URL? {
        switch self {
        case .trollStore: return TrollStore.enableJITURL
        case .stikDebug: return StikDebug.enableJITURL
        }
    }

    var unavailableMessage: String {
        switch self {
        case .trollStore:
            return """
                Open TrollStore, turn on the URL scheme in its settings, then try \
                again. TrollStore 2.0.12 or newer is required.
                """
        case .stikDebug:
            return """
                Install and configure StikDebug, then try again. LocalDevVPN must \
                be connected.
                """
        }
    }
}

private struct EnableJITButton: View {
    @Environment(\.openURL) private var openURL
    @State private var failedProvider: JITProvider?

    private var providers: [JITProvider] { JITProvider.supported }

    var body: some View {
        Group {
            if providers.count == 1, let only = providers.first {
                Button { enable(only) } label: { label }
                    .accessibilityHint("Enables JIT for touchHLE using \(only.displayName)")
            } else {
                Menu {
                    ForEach(providers) { provider in
                        Button("Using \(provider.displayName)") { enable(provider) }
                    }
                } label: {
                    label
                }
                .accessibilityHint("Chooses how to enable JIT for touchHLE")
            }
        }
        .alert(
            "\(failedProvider?.displayName ?? "JIT") Not Available",
            isPresented: Binding(
                get: { failedProvider != nil },
                set: { if !$0 { failedProvider = nil } }
            )
        ) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(failedProvider?.unavailableMessage ?? "")
        }
    }

    private var label: some View {
        Label("Enable JIT", systemImage: "bolt.fill")
    }

    private func enable(_ provider: JITProvider) {
        guard let url = provider.enableJITURL else {
            failedProvider = provider
            return
        }
        openURL(url) { accepted in
            if !accepted {
                failedProvider = provider
            }
        }
    }
}

private extension View {
    @ViewBuilder
    func touchHLEImportButtonStyle() -> some View {
        if #available(iOS 26.0, *) {
            glassEffect(.regular.tint(.blue).interactive(), in: Capsule())
        } else {
            background(.ultraThinMaterial, in: Capsule())
                .overlay {
                    Capsule().stroke(.blue.opacity(0.2), lineWidth: 1)
                }
        }
    }

    @ViewBuilder
    func touchHLELaunchOverlayStyle() -> some View {
        if #available(iOS 26.0, *) {
            glassEffect(.regular, in: RoundedRectangle(cornerRadius: 24, style: .continuous))
        } else {
            background(
                .ultraThinMaterial,
                in: RoundedRectangle(cornerRadius: 24, style: .continuous)
            )
        }
    }
}

private struct LibraryBackground: View {
    var body: some View {
        ZStack {
            Color(uiColor: .systemGroupedBackground)
            LinearGradient(
                colors: [
                    Color.blue.opacity(0.13),
                    Color.clear,
                    Color.indigo.opacity(0.08)
                ],
                startPoint: .topLeading,
                endPoint: .bottomTrailing
            )
        }
        .ignoresSafeArea()
    }
}

private struct GameCard: View {
    let game: GameFile
    let launch: () -> Void
    let delete: () -> Void

    /// `nil` means this game follows the default core.
    @State private var coreOverride: CoreKind?
    @AppStorage("defaultCore") private var defaultCoreRaw = ""

    private var defaultCore: CoreKind {
        CoreSelection.kind(forStoredDefault: defaultCoreRaw)
    }

    private var core: CoreKind {
        coreOverride ?? defaultCore
    }

    private var coreSelection: Binding<CoreKind?> {
        Binding(
            get: { coreOverride },
            set: { newValue in
                coreOverride = newValue
                CoreSelection.setOverride(newValue, forBundleIdentifier: game.bundleIdentifier)
            }
        )
    }

    var body: some View {
        Button(action: launch) {
            VStack(alignment: .leading, spacing: 14) {
                ZStack {
                    RoundedRectangle(cornerRadius: 22, style: .continuous)
                        .fill(
                            LinearGradient(
                                colors: [.blue.opacity(0.2), .indigo.opacity(0.12)],
                                startPoint: .topLeading,
                                endPoint: .bottomTrailing
                            )
                        )

                    if let icon = game.icon {
                        Image(uiImage: icon)
                            .resizable()
                            .interpolation(.high)
                            .scaledToFit()
                            .frame(width: 82, height: 82)
                            .shadow(color: .black.opacity(0.18), radius: 8, y: 4)
                    } else {
                        Image(systemName: "gamecontroller.fill")
                            .font(.system(size: 42, weight: .medium))
                            .foregroundStyle(.blue)
                    }
                }
                .frame(height: 112)

                VStack(alignment: .leading, spacing: 3) {
                    Text(game.displayName)
                        .font(.headline)
                        .lineLimit(2)
                        .minimumScaleFactor(0.8)
                        .fixedSize(horizontal: false, vertical: true)
                    Text(CoreKind.installed.count > 1 ? core.displayName : "Tap to play")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(12)
            .background(.thinMaterial, in: RoundedRectangle(cornerRadius: 28, style: .continuous))
        }
        .buttonStyle(.plain)
        .accessibilityLabel(game.displayName)
        .accessibilityHint("Starts this game in \(core.displayName)")
        .contextMenu {
            if CoreKind.installed.count > 1, game.bundleIdentifier != nil {
                Picker("Core", selection: coreSelection) {
                    Text("Default (\(defaultCore.displayName))")
                        .tag(CoreKind?.none)
                    ForEach(CoreKind.installed) { kind in
                        Text(kind.displayName).tag(CoreKind?.some(kind))
                    }
                }
            }

            Button(role: .destructive, action: delete) {
                Label("Remove from Library", systemImage: "trash")
            }
        }
        .onAppear {
            coreOverride = CoreSelection.override(forBundleIdentifier: game.bundleIdentifier)
        }
    }
}

private struct SettingsView: View {
    @Environment(\.dismiss) private var dismiss
    @AppStorage("scaleHack") private var scaleHack = 3
    @AppStorage("orientation") private var orientation = 0
    @AppStorage("networkAccess") private var networkAccess = false
    @AppStorage("analogTilt") private var analogTilt = true
    @AppStorage("autoEnableJIT") private var autoEnableJIT = true
    @AppStorage("defaultCore") private var defaultCoreRaw = ""

    private var defaultCore: CoreKind {
        CoreSelection.kind(forStoredDefault: defaultCoreRaw)
    }

    var body: some View {
        NavigationContainer {
            Form {
                if CoreKind.installed.count > 1 {
                    Section {
                        ForEach(CoreKind.installed) { kind in
                            CoreChoiceRow(kind: kind, isSelected: kind == defaultCore) {
                                defaultCoreRaw = kind.rawValue
                            }
                        }
                    } header: {
                        Text("Emulator Core")
                    } footer: {
                        Text("Games use this core unless you pick a different one for them. Touch and hold a game in your library to do that.")
                    }
                }

                Section("Display") {
                    Picker("Resolution Scale", selection: $scaleHack) {
                        Text("Off").tag(1)
                        Text("2×").tag(2)
                        Text("3×").tag(3)
                        Text("4×").tag(4)
                    }

                    Picker("Starting Orientation", selection: $orientation) {
                        Text("Automatic").tag(OrientationSetting.automatic)
                        Text("Portrait").tag(OrientationSetting.portrait)
                        Text("Landscape Left").tag(OrientationSetting.landscapeLeft)
                        Text("Landscape Right").tag(OrientationSetting.landscapeRight)
                    }
                }

                Section {
                    Toggle("Network Access", isOn: $networkAccess)
                } header: {
                    Text("Permissions")
                } footer: {
                    Text("Some games need network access. Leave this off unless a game requires it.")
                }

                Section("Controls") {
                    Toggle("Analog Sticks Control Tilt", isOn: $analogTilt)
                }

                Section {
                    EnableJITButton()

                    Toggle(
                        "Enable Automatically on Launch",
                        isOn: $autoEnableJIT
                    )
                } header: {
                    Text("JIT")
                } footer: {
                    Text("JIT must be enabled again whenever touchHLE starts as a new app process. The automatic handoff opens TrollStore and returns, which you will see briefly.")
                }

                JITDiagnosticsSection()

                Section("Advanced") {
                    NavigationLink {
                        DeveloperToolsView()
                    } label: {
                        Label("Developer Tools", systemImage: "wrench.and.screwdriver")
                    }
                }

                Section {
                    Text("Settings apply the next time you start a game.")
                        .foregroundStyle(.secondary)
                }
            }
            .navigationTitle("Settings")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") {
                        dismiss()
                    }
                }
            }
        }
    }
}

/// One core in the settings list: name, version, what it is for, and a
/// checkmark. A list of these reads more clearly than a picker, and it leaves
/// room to say what each core actually does.
private struct CoreChoiceRow: View {
    let kind: CoreKind
    let isSelected: Bool
    let select: () -> Void

    var body: some View {
        Button(action: select) {
            HStack(alignment: .top, spacing: 12) {
                VStack(alignment: .leading, spacing: 4) {
                    HStack(spacing: 7) {
                        Text(kind.displayName)
                            .font(.body.weight(isSelected ? .semibold : .regular))
                            .foregroundStyle(.primary)
                        Text(kind.version)
                            .font(.caption2.weight(.medium))
                            .foregroundStyle(.secondary)
                            .padding(.horizontal, 7)
                            .padding(.vertical, 2)
                            .background(.quaternary, in: Capsule())
                    }
                    Text(kind.summary)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }

                Spacer(minLength: 0)

                Image(systemName: "checkmark")
                    .font(.body.weight(.semibold))
                    .foregroundStyle(Color.accentColor)
                    .opacity(isSelected ? 1 : 0)
                    .accessibilityHidden(true)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(isSelected ? [.isButton, .isSelected] : .isButton)
    }
}

private struct DeveloperToolsView: View {
    @AppStorage("showFPSOverlay") private var showFPSOverlay = false

    var body: some View {
        Form {
            Section {
                Toggle("Show FPS During Games", isOn: $showFPSOverlay)
            } header: {
                Text("Performance")
            } footer: {
                Text("Displays a small frame-rate counter beside the exit button. This is intended for testing and is off by default.")
            }
        }
        .navigationTitle("Developer Tools")
        .navigationBarTitleDisplayMode(.inline)
    }
}

private struct AboutView: View {
    @Environment(\.dismiss) private var dismiss

    private var appIcon: UIImage? {
        guard
            let icons = Bundle.main.infoDictionary?["CFBundleIcons"] as? [String: Any],
            let primaryIcon = icons["CFBundlePrimaryIcon"] as? [String: Any],
            let iconFiles = primaryIcon["CFBundleIconFiles"] as? [String],
            let iconName = iconFiles.last
        else {
            return nil
        }

        return UIImage(named: iconName)
    }

    var body: some View {
        NavigationContainer {
            List {
                Section {
                    VStack(spacing: 14) {
                        Group {
                            if let appIcon {
                                Image(uiImage: appIcon)
                                    .resizable()
                                    .scaledToFill()
                            } else {
                                Image(systemName: "iphone.gen3")
                                    .font(.system(size: 42, weight: .medium))
                                    .foregroundStyle(.blue)
                            }
                        }
                        .frame(width: 88, height: 88)
                        .clipShape(RoundedRectangle(cornerRadius: 20, style: .continuous))

                        Text("touchHLE")
                            .font(.title2.bold())
                        Text(
                            CoreKind.installed
                                .map { "\($0.displayName) \($0.version)" }
                                .joined(separator: " • ")
                        )
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                    }
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 18)
                }

                Section("About") {
                    Text("touchHLE runs older 32-bit iPhone applications without including any Apple software. This native iOS port is an experimental community project and is not an official release of touchHLE. It ships two emulator cores — HyperHLE v1.0.6, a fork of touchHLE that runs more games, and touchHLE 0.2.3 upstream — and you can pick which one runs each game.")

                    Link(destination: URL(string: "https://appdb.touchhle.org/")!) {
                        Label("Game Compatibility", systemImage: "checkmark.seal")
                    }

                    Link(destination: URL(string: "https://touchhle.org/")!) {
                        Label("touchHLE Website", systemImage: "safari")
                    }

                    Link(destination: URL(string: "https://github.com/HyperHLE/HyperHLE")!) {
                        Label("HyperHLE Core", systemImage: "chevron.left.forwardslash.chevron.right")
                    }

                    Link(destination: URL(string: "https://github.com/touchHLE/touchHLE")!) {
                        Label("touchHLE Upstream", systemImage: "chevron.left.forwardslash.chevron.right")
                    }
                }
            }
            .navigationTitle("About")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") {
                        dismiss()
                    }
                }
            }
        }
    }
}
