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
    var llmServerProcess: Process?
    var tunnelProcess: Process?
    var tunnelURL: String = ""
    let port = 3001
    let llmPort = 4001

    // Per-install random token stored in UserDefaults (used for web UI + LLM API key)
    lazy var localToken: String = {
        let key = "NOU_LOCAL_TOKEN_v1"
        if let t = UserDefaults.standard.string(forKey: key) { return t }
        let chars = Array("abcdefghijklmnopqrstuvwxyz0123456789")
        let t = String((0..<32).map { _ in chars[Int.random(in: 0..<chars.count)] })
        UserDefaults.standard.set(t, forKey: key)
        return t
    }()

    // LLM API key (same as localToken for simplicity)
    var llmAPIKey: String { localToken }

    func applicationDidFinishLaunching(_ n: Notification) {
        buildMenuBar()
        startServer()
        startLLMServer()
        startBonjourAdvertisement()
        startTunnel()
        // Wait for server to be ready before opening window
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.4) {
            self.openWindow()
        }
    }

    func applicationWillTerminate(_ n: Notification) {
        serverProcess?.terminate()
        llmServerProcess?.terminate()
        tunnelProcess?.terminate()
        netService?.stop()
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

        // LLM server status
        let llmItem = NSMenuItem(title: "LLM Server: port \(llmPort)", action: nil, keyEquivalent: "")
        llmItem.isEnabled = false
        menu.addItem(llmItem)

        if !tunnelURL.isEmpty {
            let tunnelItem = NSMenuItem(title: "Tunnel: \(tunnelURL)", action: #selector(copyTunnelURL), keyEquivalent: "")
            tunnelItem.target = self
            menu.addItem(tunnelItem)
        } else {
            let tunnelItem = NSMenuItem(title: "Tunnel: starting...", action: nil, keyEquivalent: "")
            tunnelItem.isEnabled = false
            menu.addItem(tunnelItem)
        }

        addItem(menu, "Restart LLM Server", #selector(restartLLMServer))
        menu.addItem(.separator())
        addItem(menu, "Pair with iPhone...", #selector(showPairingQR))

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

    // ── LLM Server (llama-server on port 4001) ──────────────────────────
    func startLLMServer() {
        guard let llamaServer = findLlamaServer() else {
            NSLog("NOU: llama-server not found, LLM server disabled")
            return
        }
        guard let modelPath = findModel() else {
            NSLog("NOU: no GGUF model found, LLM server disabled")
            return
        }

        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: llamaServer)
        proc.arguments = [
            "--model", modelPath,
            "--port", "\(llmPort)",
            "--host", "0.0.0.0",
            "--n-gpu-layers", "99",
            "--ctx-size", "8192",
            "--threads", "\(max(ProcessInfo.processInfo.activeProcessorCount - 2, 2))",
            "--api-key", llmAPIKey,
        ]
        proc.environment = ProcessInfo.processInfo.environment
        proc.terminationHandler = { [weak self] p in
            NSLog("NOU: llama-server exited code=\(p.terminationStatus)")
            self?.llmServerProcess = nil
        }
        do {
            try proc.run()
            llmServerProcess = proc
            NSLog("NOU: llama-server started pid=\(proc.processIdentifier) port=\(llmPort) model=\(modelPath)")
        } catch {
            NSLog("NOU: llama-server start failed: \(error.localizedDescription)")
        }
    }

    @objc func restartLLMServer() {
        llmServerProcess?.terminate()
        llmServerProcess = nil
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.0) { self.startLLMServer() }
    }

    // ── Bonjour Advertisement (_nou._tcp) ────────────────────────────────
    var netService: NetService?

    func startBonjourAdvertisement() {
        // Wait for llama-server to start, then advertise via Bonjour
        DispatchQueue.main.asyncAfter(deadline: .now() + 2.0) {
            let service = NetService(domain: "local.", type: "_nou._tcp.", name: "NOU-\(Host.current().localizedName ?? "Mac")", port: Int32(self.llmPort))
            service.publish()
            self.netService = service
            NSLog("NOU: Bonjour advertising _nou._tcp on port \(self.llmPort)")
        }
    }

    // ── Cloudflare Tunnel (external access via QUIC) ────────────────────
    func startTunnel() {
        guard let cloudflared = findCloudflared() else {
            NSLog("NOU: cloudflared not found, tunnel disabled")
            return
        }

        // Rename config.yml temporarily to avoid catch-all 404 rule
        let configPath = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".cloudflared/config.yml").path
        let configBackup = configPath + ".nou-bak"
        let hadConfig = FileManager.default.fileExists(atPath: configPath)
        if hadConfig {
            try? FileManager.default.moveItem(atPath: configPath, toPath: configBackup)
        }

        let pipe = Pipe()
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: cloudflared)
        proc.arguments = ["tunnel", "--url", "http://localhost:\(llmPort)"]
        proc.standardOutput = pipe
        proc.standardError = pipe
        proc.environment = ProcessInfo.processInfo.environment
        proc.terminationHandler = { [weak self] _ in
            NSLog("NOU: cloudflared exited")
            self?.tunnelProcess = nil
            // Restore config
            if hadConfig {
                try? FileManager.default.moveItem(atPath: configBackup, toPath: configPath)
            }
        }

        // Read output to capture tunnel URL
        pipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            guard !data.isEmpty, let line = String(data: data, encoding: .utf8) else { return }
            if let range = line.range(of: "https://[\\w-]+\\.trycloudflare\\.com", options: .regularExpression) {
                let url = String(line[range])
                DispatchQueue.main.async {
                    self?.tunnelURL = url
                    self?.buildMenuBar()
                    NSLog("NOU: tunnel URL = \(url)")
                    // Save for iPhone to discover
                    UserDefaults.standard.set(url, forKey: "nou_tunnel_url")
                }
            }
        }

        do {
            try proc.run()
            tunnelProcess = proc
            NSLog("NOU: cloudflared starting...")
            // Restore config after a delay (tunnel is established by then)
            DispatchQueue.main.asyncAfter(deadline: .now() + 15) {
                if hadConfig {
                    try? FileManager.default.moveItem(atPath: configBackup, toPath: configPath)
                }
            }
        } catch {
            NSLog("NOU: cloudflared start failed: \(error)")
            if hadConfig {
                try? FileManager.default.moveItem(atPath: configBackup, toPath: configPath)
            }
        }
    }

    @objc func copyTunnelURL() {
        guard !tunnelURL.isEmpty else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(tunnelURL, forType: .string)
        NSLog("NOU: tunnel URL copied to clipboard")
    }

    // ── Pairing QR Code ─────────────────────────────────────────────────
    @objc func showPairingQR() {
        // Build pairing payload
        var localIP = "localhost"
        let host = ProcessInfo.processInfo.hostName
        // Get en0 IP
        var ifaddr: UnsafeMutablePointer<ifaddrs>?
        if getifaddrs(&ifaddr) == 0, let first = ifaddr {
            defer { freeifaddrs(first) }
            for ptr in sequence(first: first, next: { $0.pointee.ifa_next }) {
                let addr = ptr.pointee
                guard addr.ifa_addr.pointee.sa_family == UInt8(AF_INET),
                      String(cString: addr.ifa_name) == "en0" else { continue }
                var hostname = [CChar](repeating: 0, count: Int(NI_MAXHOST))
                getnameinfo(addr.ifa_addr, socklen_t(addr.ifa_addr.pointee.sa_len),
                            &hostname, socklen_t(hostname.count), nil, 0, NI_NUMERICHOST)
                localIP = String(cString: hostname)
                break
            }
        }

        let payload: [String: String] = [
            "local": "http://\(localIP):\(llmPort)",
            "tunnel": tunnelURL,
            "key": llmAPIKey,
            "name": host
        ]
        guard let jsonData = try? JSONSerialization.data(withJSONObject: payload),
              let jsonStr = String(data: jsonData, encoding: .utf8) else { return }

        // Generate QR code
        guard let qrFilter = CIFilter(name: "CIQRCodeGenerator") else { return }
        qrFilter.setValue(jsonStr.data(using: .utf8), forKey: "inputMessage")
        qrFilter.setValue("M", forKey: "inputCorrectionLevel")

        guard let ciImage = qrFilter.outputImage else { return }
        let scale = CGAffineTransform(scaleX: 8, y: 8)
        let scaledImage = ciImage.transformed(by: scale)

        let rep = NSCIImageRep(ciImage: scaledImage)
        let nsImage = NSImage(size: rep.size)
        nsImage.addRepresentation(rep)

        // Show in a window
        let qrWindow = NSWindow(
            contentRect: NSMakeRect(0, 0, 360, 440),
            styleMask: [.titled, .closable],
            backing: .buffered, defer: false)
        qrWindow.title = "Pair with iPhone"
        qrWindow.center()

        let container = NSView(frame: NSMakeRect(0, 0, 360, 440))

        let imageView = NSImageView(frame: NSMakeRect(30, 100, 300, 300))
        imageView.image = nsImage
        imageView.imageScaling = .scaleProportionallyUpOrDown
        container.addSubview(imageView)

        let label = NSTextField(labelWithString: "Scan this QR code in NOU on your iPhone")
        label.frame = NSMakeRect(30, 60, 300, 30)
        label.alignment = .center
        label.font = NSFont.systemFont(ofSize: 13)
        label.textColor = .secondaryLabelColor
        container.addSubview(label)

        let keyLabel = NSTextField(labelWithString: "API Key: \(String(llmAPIKey.prefix(8)))...")
        keyLabel.frame = NSMakeRect(30, 30, 300, 20)
        keyLabel.alignment = .center
        keyLabel.font = NSFont.monospacedSystemFont(ofSize: 11, weight: .regular)
        keyLabel.textColor = .tertiaryLabelColor
        container.addSubview(keyLabel)

        qrWindow.contentView = container
        qrWindow.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    func findCloudflared() -> String? {
        ["/opt/homebrew/bin/cloudflared", "/usr/local/bin/cloudflared"]
            .first { FileManager.default.fileExists(atPath: $0) }
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
        llmServerProcess?.terminate()
        tunnelProcess?.terminate()
        netService?.stop()
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

    func findLlamaServer() -> String? {
        let candidates = [
            "/opt/homebrew/bin/llama-server",
            "/usr/local/bin/llama-server",
        ]
        return candidates.first { FileManager.default.fileExists(atPath: $0) }
    }

    func findModel() -> String? {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        let searchDirs = [
            "\(home)/Documents/models",
            "\(home)/.cache/lm-studio/models",
            "\(home)/models",
        ]
        var allGGUF: [String] = []
        for dir in searchDirs {
            guard let files = try? FileManager.default.contentsOfDirectory(atPath: dir) else { continue }
            for f in files where f.hasSuffix(".gguf") {
                allGGUF.append("\(dir)/\(f)")
            }
        }
        guard !allGGUF.isEmpty else { return nil }

        func fileSize(_ path: String) -> UInt64 {
            (try? FileManager.default.attributesOfItem(atPath: path)[.size] as? UInt64) ?? 0
        }

        // Mac has plenty of RAM — prefer the LARGEST model available
        // Sort by size descending, pick the biggest
        let sorted = allGGUF.sorted { fileSize($0) > fileSize($1) }

        // Log what we found
        for path in sorted.prefix(3) {
            let name = (path as NSString).lastPathComponent
            let mb = fileSize(path) / 1024 / 1024
            NSLog("NOU: found model \(name) (\(mb) MB)")
        }

        let chosen = sorted[0]
        NSLog("NOU: selected model \((chosen as NSString).lastPathComponent)")
        return chosen
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
