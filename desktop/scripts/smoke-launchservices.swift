// Open the real packaged app through LaunchServices in a private fixture home.
// Never modify launchctl's global environment or target an app by bundle ID.
import AppKit
import Foundation

let arguments = Array(CommandLine.arguments.dropFirst())
guard arguments.count == 6 else {
    fputs("Expected app, ready file, home, runtime, config and state paths\n", stderr)
    exit(1)
}
let configuration = NSWorkspace.OpenConfiguration()
configuration.createsNewApplicationInstance = true
configuration.arguments = ["--update-ready", arguments[1]]
configuration.environment = [
    "HOME": arguments[2], "SHELL": "", "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
    "XDG_RUNTIME_DIR": arguments[3], "XDG_CONFIG_HOME": arguments[4],
    "XDG_STATE_HOME": arguments[5],
]
var launched: NSRunningApplication?
var launchError: Error?
var completed = false
NSWorkspace.shared.openApplication(at: URL(fileURLWithPath: arguments[0]),
                                   configuration: configuration) { app, error in
    launched = app
    launchError = error
    completed = true
}
func waitUntil(_ seconds: TimeInterval, _ predicate: () -> Bool) -> Bool {
    let deadline = Date().addingTimeInterval(seconds)
    while !predicate() && Date() < deadline {
        RunLoop.current.run(until: Date().addingTimeInterval(0.05))
    }
    return predicate()
}
guard waitUntil(30, { completed }), let app = launched else {
    fputs("LaunchServices failed: \(String(describing: launchError))\n", stderr)
    exit(1)
}
let ready = waitUntil(30, {
    app.isTerminated || FileManager.default.fileExists(atPath: arguments[1])
}) && !app.isTerminated && FileManager.default.fileExists(atPath: arguments[1])
// This native handle identifies only the instance created above, including on
// failure. Quitting the GUI must leave its fixture daemon and managed Shells up.
if !app.isTerminated {
    app.terminate()
    if !waitUntil(10, { app.isTerminated }) {
        app.forceTerminate()
        _ = waitUntil(5, { app.isTerminated })
    }
}
guard ready && app.isTerminated else {
    fputs("LaunchServices app did not create a window or quit within its deadline\n", stderr)
    exit(1)
}
print("LaunchServices window ready with minimal PATH and absent SHELL")
