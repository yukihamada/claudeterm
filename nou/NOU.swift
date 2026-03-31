import Cocoa
import WebKit

// ─── Entry Point ───────────────────────────────────────────────────────────
let app = NSApplication.shared
app.setActivationPolicy(.accessory) // menu bar only, no Dock icon
let delegate = AppDelegate()
app.delegate = delegate
app.run()

// ─── AppDelegate ───────────────────────────────────────────────────────────
class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    var statusItem: NSStatusItem!
    var window: NSWindow?
    var serverProcess: Process?
    let port = 3001

    // Per-install random token stored in UserDefaults
    lazy var localToken: String = {
        let key = "NOU_LOCAL_TOKEN_v1"
        if let t = UserDefaults.standard.string(forKey: key) { return t }
        let chars = Array("abcdefghijklmnopqrstuvwxyz0123456789")
        let t = String((0..<32).map { _ in chars[Int.random(in: 0..<chars.count)] })
        UserDefaults.standard.set(t, forKey: key)
        return t
    }()

    func applicationDidFinishLaunching(_ n: Notification) {
        buildMenuBar()
        startServer()
        // Wait for server to be ready before opening window
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.4) {
            self.openWindow()
        }
    }

    func applicationWillTerminate(_ n: Notification) {
        serverProcess?.terminate()
    }

    // ── Menu bar ─────────────────────────────────────────────────────────
    func buildMenuBar() {
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        if let btn = statusItem.button {
            btn.title = "◆"
            btn.toolTip = "NOU — Local Claude Terminal"
        }
        let menu = NSMenu()
        addItem(menu, "Open Terminal", #selector(openWindow))
        addItem(menu, "Open in Browser", #selector(openInBrowser))
        menu.addItem(.separator())
        addItem(menu, "Restart Server", #selector(restartServer))
        menu.addItem(.separator())
        addItem(menu, "Quit NOU", #selector(quitApp))
        statusItem.menu = menu
    }

    private func addItem(_ menu: NSMenu, _ title: String, _ sel: Selector) {
        let item = NSMenuItem(title: title, action: sel, keyEquivalent: "")
        item.target = self
        menu.addItem(item)
    }

    // ── Server lifecycle ─────────────────────────────────────────────────
    func startServer() {
        guard let binary = findBinary() else {
            showError("claudeterm binary not found", detail: "Expected inside NOU.app/Contents/MacOS/claudeterm")
            return
        }
        let workdir = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("claudeterm-workspaces").path
        try? FileManager.default.createDirectory(atPath: workdir, withIntermediateDirectories: true)

        var env = ProcessInfo.processInfo.environment
        env["PORT"]        = "\(port)"
        env["WORKDIR"]     = workdir
        env["DB_PATH"]     = "\(workdir)/claudeterm.db"
        env["LOCAL_TOKEN"] = localToken
        env["BASE_URL"]    = "http://localhost:\(port)"
        if let claude = findClaude() { env["CLAUDE_COMMAND"] = claude }

        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: binary)
        proc.environment = env
        proc.terminationHandler = { [weak self] _ in
            NSLog("NOU: server exited")
            self?.serverProcess = nil
        }
        do {
            try proc.run()
            serverProcess = proc
            NSLog("NOU: server started pid=\(proc.processIdentifier) port=\(port)")
        } catch {
            showError("Server start failed", detail: error.localizedDescription)
        }
    }

    @objc func restartServer() {
        serverProcess?.terminate()
        serverProcess = nil
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.6) { self.startServer() }
    }

    // ── Window ────────────────────────────────────────────────────────────
    @objc func openWindow() {
        if let w = window, w.isVisible {
            w.makeKeyAndOrderFront(nil)
            NSApp.activate(ignoringOtherApps: true)
            return
        }
        let w = NSWindow(
            contentRect: NSMakeRect(0, 0, 1280, 820),
            styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView],
            backing: .buffered, defer: false)
        w.title = "NOU"
        w.titlebarAppearsTransparent = true
        w.center()
        w.delegate = self

        let wv = makeWebView(frame: w.contentView!.bounds)
        wv.autoresizingMask = [.width, .height]
        w.contentView?.addSubview(wv)
        w.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        self.window = w
    }

    func windowWillClose(_ n: Notification) {
        window = nil
    }

    private func makeWebView(frame: NSRect) -> WKWebView {
        let cfg = WKWebViewConfiguration()
        // Allow localStorage across loads
        cfg.websiteDataStore = .default()
        let wv = WKWebView(frame: frame, configuration: cfg)
        let url = URL(string: "http://localhost:\(port)/?local=\(localToken)")!
        wv.load(URLRequest(url: url))
        return wv
    }

    @objc func openInBrowser() {
        let url = URL(string: "http://localhost:\(port)/?local=\(localToken)")!
        NSWorkspace.shared.open(url)
    }

    @objc func quitApp() {
        serverProcess?.terminate()
        NSApp.terminate(nil)
    }

    // ── Helpers ───────────────────────────────────────────────────────────
    func findBinary() -> String? {
        let execDir = (Bundle.main.executablePath as NSString?)?.deletingLastPathComponent ?? ""
        let candidates = [
            "\(execDir)/claudeterm",                         // same dir as NOU binary
            Bundle.main.resourcePath.map { "\($0)/claudeterm" } ?? "",
            "/usr/local/bin/claudeterm",
        ]
        return candidates.first { !$0.isEmpty && FileManager.default.fileExists(atPath: $0) }
    }

    func findClaude() -> String? {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        let candidates = [
            "/usr/local/bin/claude",
            "/opt/homebrew/bin/claude",
            "\(home)/.npm/bin/claude",
            "\(home)/.local/bin/claude",
            "\(home)/Library/pnpm/claude",
        ]
        return candidates.first { FileManager.default.fileExists(atPath: $0) }
    }

    func showError(_ msg: String, detail: String) {
        DispatchQueue.main.async {
            let alert = NSAlert()
            alert.messageText = msg
            alert.informativeText = detail
            alert.alertStyle = .critical
            alert.runModal()
        }
    }
}
