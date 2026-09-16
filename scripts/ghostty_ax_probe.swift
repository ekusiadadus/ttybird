// Manual macOS integration probe. Creates ONLY synthetic Ghostty surfaces.
// Never use this title-based fixture isolation as a production UUID mapping.
import Foundation
import AppKit
import ApplicationServices

func record(_ id: String, _ status: String, _ fields: [String: Any]) {
    var row = fields
    row["checkpoint"] = id; row["status"] = status
    row["at"] = ISO8601DateFormatter().string(from: Date())
    let data = try! JSONSerialization.data(withJSONObject: row, options: [.sortedKeys])
    print(String(data: data, encoding: .utf8)!); fflush(stdout)
}
func script(_ body: String) throws -> String {
    let p = Process(); p.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
    p.arguments = ["-e", "tell application \"Ghostty\"\n\(body)\nend tell"]
    let out = Pipe(), err = Pipe(); p.standardOutput = out; p.standardError = err
    try p.run()
    let limit = Date().addingTimeInterval(10)
    while p.isRunning && Date() < limit { Thread.sleep(forTimeInterval: 0.02) }
    if p.isRunning { p.terminate(); throw NSError(domain: "script timeout", code: 1) }
    let output = String(data: out.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
    if p.terminationStatus != 0 {
        let error = String(data: err.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
        throw NSError(domain: error, code: Int(p.terminationStatus))
    }
    return output.trimmingCharacters(in: .whitespacesAndNewlines)
}
func attr(_ e: AXUIElement, _ name: String) -> (AXError, CFTypeRef?) {
    var value: CFTypeRef?
    let error = AXUIElementCopyAttributeValue(e, name as CFString, &value)
    return (error, value)
}
func text(_ e: AXUIElement, _ name: String) -> String? { attr(e, name).1 as? String }
func children(_ e: AXUIElement, _ name: String = kAXChildrenAttribute) -> [AXUIElement] {
    attr(e, name).1 as? [AXUIElement] ?? []
}
func areas(_ e: AXUIElement, depth: Int = 0) -> [AXUIElement] {
    guard depth < 20 else { return [] }
    if text(e, kAXRoleAttribute) == kAXTextAreaRole { return [e] }
    return children(e).flatMap { areas($0, depth: depth + 1) }
}
func range(_ e: AXUIElement, _ name: String) -> [String: Int] {
    let (error, object) = attr(e, name)
    guard error == .success, let object, CFGetTypeID(object) == AXValueGetTypeID() else {
        return ["error": Int(error.rawValue)]
    }
    var r = CFRange(); let value = unsafeBitCast(object, to: AXValue.self)
    guard AXValueGetValue(value, .cfRange, &r) else { return ["error": -1] }
    return ["location": r.location, "length": r.length]
}
var notifications: [[String: Any]] = []
var droppedNotifications = 0
var observedArea: AXUIElement?, observedWindow: AXUIElement?
func callback(_ observer: AXObserver, _ element: AXUIElement, _ name: CFString, _ context: UnsafeMutableRawPointer?) {
    guard notifications.count < 256 else { droppedNotifications += 1; return }
    var pid: pid_t = 0; AXUIElementGetPid(element, &pid)
    notifications.append(["name": name as String, "uptime": ProcessInfo.processInfo.systemUptime,
        "pid": pid, "matches_area": observedArea.map { CFEqual($0, element) } ?? false,
        "matches_window": observedWindow.map { CFEqual($0, element) } ?? false])
}
func pump(_ seconds: Double) { RunLoop.current.run(until: Date().addingTimeInterval(seconds)) }

let args = CommandLine.arguments
guard args.count == 3 else { fatalError("usage: probe FIXTURE_PYTHON_COMMAND FIXTURE_DIRECTORY") }
let fixtureCommand = args[1], directory = args[2]
let token = "TTYbird-AX-" + UUID().uuidString
record("C01", AXIsProcessTrusted() ? "passed" : "blocked", ["trusted": AXIsProcessTrusted(), "prompt_requested": false])
guard AXIsProcessTrusted() else { exit(2) }
guard let ghostty = NSRunningApplication.runningApplications(withBundleIdentifier: "com.mitchellh.ghostty").first else {
    record("C02", "blocked", ["reason": "Ghostty not running"]); exit(2)
}
let application = AXUIElementCreateApplication(ghostty.processIdentifier)
AXUIElementSetMessagingTimeout(application, 1)
let bundle = ghostty.bundleURL.flatMap { Bundle(url: $0) }
var axPID: pid_t = 0
let pidError = AXUIElementGetPid(application, &axPID)
record("C01-identity", "observed", ["bundle_path": ghostty.bundleURL?.path ?? "unknown",
    "version": bundle?.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "unknown",
    "build": bundle?.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "unknown",
    "launch_date": ghostty.launchDate.map { ISO8601DateFormatter().string(from: $0) } ?? "unknown",
    "pid": ghostty.processIdentifier, "ax_pid": axPID, "ax_pid_error": pidError.rawValue])
guard pidError == .success, axPID == ghostty.processIdentifier else { exit(2) }
var windowID = "", firstID = "", tabID = ""
var ownedTerminalIDs: [String] = []
var observer: AXObserver?
var failed = false
func fixtureConfig(_ marker: String) -> String {
    """
    set cfg to new surface configuration
    set command of cfg to "\(fixtureCommand) \(directory) \(token) \(marker)"
    set initial working directory of cfg to "\(directory)"
    set wait after command of cfg to false
    """
}
func phase(_ value: String) throws {
    try value.write(toFile: directory + "/phase", atomically: true, encoding: .utf8)
}
func confirmedPhase(_ value: String) throws -> Date {
    let start = Date(); try phase(value)
    while (try? String(contentsOfFile: directory + "/ack-FIRST", encoding: .utf8)) != value {
        if Date().timeIntervalSince(start) > 5 { throw NSError(domain: "fixture ack timeout: \(value)", code: 1) }
        pump(0.01)
    }
    let confirmed = Date()
    record("fixture-ack", "passed", ["phase": value, "ack_delay_ms": confirmed.timeIntervalSince(start) * 1000])
    return confirmed
}
func fixtureWindows() -> [AXUIElement] {
    children(application, kAXWindowsAttribute).filter { text($0, kAXTitleAttribute) == token }
}
func snapshot(_ area: AXUIElement) -> [String: Any] {
    let start = ProcessInfo.processInfo.systemUptime
    let (error, object) = attr(area, kAXValueAttribute)
    let attributeElapsed = (ProcessInfo.processInfo.systemUptime - start) * 1000
    let value = object as? String ?? ""
    return ["error": error.rawValue, "attribute_ms": attributeElapsed, "utf8_bytes": value.utf8.count,
            "utf16_units": value.utf16.count, "graphemes": value.count,
            "oldest_present": value.contains("AX_OLDEST"), "initial_present": value.contains("AX_INITIAL"),
            "unicode_present": value.contains("日本語 e\u{301} 😀 👩‍💻"),
            "phase_B": value.contains("AX_PHASE_B"), "phase_C": value.contains("AX_PHASE_C"),
            "phase_D": value.contains("AX_PHASE_D"), "alternate": value.contains("AX_ALTERNATE"),
            "other_tab": value.contains("AX_SECOND"),
            "hidden_first": value.contains("AX_HIDDEN_FIRST"), "hidden_second": value.contains("AX_HIDDEN_SECOND"),
            "large_end": value.contains("AX_LARGE_END"),
            "elapsed_ms": (ProcessInfo.processInfo.systemUptime - start) * 1000]
}
do {
    let created = try script(fixtureConfig("FIRST") + """
    
    set w to new window with configuration cfg
    return (id of w) & "|" & (id of terminal 1 of selected tab of w) & "|" & (id of selected tab of w)
    """)
    let ids = created.components(separatedBy: "|")
    guard ids.count == 3 else { throw NSError(domain: "fixture identity", code: 1) }
    windowID = ids[0]; firstID = ids[1]; tabID = ids[2]
    ownedTerminalIDs.append(firstID)
    record("C02", "passed", ["window_id": windowID, "terminal_id": firstID, "tab_id": tabID,
        "ghostty_pid": ghostty.processIdentifier, "fixture_title": token])
    pump(1)
    let windows = fixtureWindows()
    guard windows.count == 1 else { throw NSError(domain: "fixture AX window not unique: \(windows.count)", code: 1) }
    let window = windows[0], found = areas(window)
    guard found.count == 1 else { throw NSError(domain: "fixture text area not unique: \(found.count)", code: 1) }
    let area = found[0]
    observedArea = area; observedWindow = window
    var names: CFArray?, params: CFArray?
    AXUIElementCopyAttributeNames(area, &names); AXUIElementCopyParameterizedAttributeNames(area, &params)
    record("C03", "passed", ["fixture_text_areas": found.count, "identifier": text(area, kAXIdentifierAttribute) ?? "absent",
        "attributes": names as? [String] ?? [], "parameterized_attributes": params as? [String] ?? [],
        "production_uuid_mapping_proven": false])
    var writable = DarwinBoolean(false)
    let writeError = AXUIElementIsAttributeSettable(area, kAXValueAttribute as CFString, &writable)
    record("C04", "observed", ["snapshot": snapshot(area), "value_settable": writable.boolValue,
        "settable_error": writeError.rawValue, "visible_range": range(area, kAXVisibleCharacterRangeAttribute),
        "number_of_characters": attr(area, kAXNumberOfCharactersAttribute).1 as? Int ?? -1])
    let initial = snapshot(area)
    guard initial["oldest_present"] as? Bool == true, initial["initial_present"] as? Bool == true,
          initial["unicode_present"] as? Bool == true else { throw NSError(domain: "initial fixture markers missing", code: 1) }
    let value = text(area, kAXValueAttribute) ?? ""
    for length in [0, value.count, value.utf16.count, value.utf16.count + 1] {
        var r = CFRange(location: 0, length: length)
        let parameter = AXValueCreate(.cfRange, &r)!
        var result: CFTypeRef?
        let e = AXUIElementCopyParameterizedAttributeValue(area, kAXStringForRangeParameterizedAttribute as CFString, parameter, &result)
        let returned = result as? String
        record("C04-range", "observed", ["requested_length": length, "error": e.rawValue,
            "returned_utf16": returned?.utf16.count ?? -1, "matches_full_value": returned == value])
    }
    let unicodeRange = (value as NSString).range(of: "😀")
    for (label, location, length) in [("tail", value.utf16.count - 1, 1), ("empty_end", value.utf16.count, 0),
                                    ("past_end", value.utf16.count + 1, 0), ("emoji", unicodeRange.location, unicodeRange.length)] {
        var r = CFRange(location: location, length: length)
        var result: CFTypeRef?
        let e = AXUIElementCopyParameterizedAttributeValue(area, kAXStringForRangeParameterizedAttribute as CFString, AXValueCreate(.cfRange, &r)!, &result)
        record("C04-range-edge", "observed", ["case": label, "location": location, "length": length,
            "error": e.rawValue, "returned_utf16": (result as? String)?.utf16.count ?? -1,
            "emoji_matches": (result as? String) == "😀"])
    }
    let observerError = AXObserverCreate(ghostty.processIdentifier, callback, &observer)
    record("C05-observer", observerError == .success ? "passed" : "failed", ["create_error": observerError.rawValue])
    if let observer {
        CFRunLoopAddSource(CFRunLoopGetCurrent(), AXObserverGetRunLoopSource(observer), .defaultMode)
        let valueRegistration = AXObserverAddNotification(observer, area, kAXValueChangedNotification as CFString, nil)
        let titleRegistration = AXObserverAddNotification(observer, window, kAXTitleChangedNotification as CFString, nil)
        record("C05-registration", "observed", ["create_error": observerError.rawValue,
            "value_error": valueRegistration.rawValue, "title_error": titleRegistration.rawValue])
    }
    // Seed cache, then change output. Actual child write acknowledged by a file.
    _ = snapshot(area)
    let started = try confirmedPhase("B")
    for delay in [0.0, 0.25, 0.35, 0.35] {
        pump(delay)
        record("C05-cache", "observed", ["after_ack_ms": Date().timeIntervalSince(started) * 1000, "snapshot": snapshot(area)])
    }
    for p in ["C", "D"] { _ = try confirmedPhase(p); pump(0.8); record("C05-output", "observed", ["phase": p, "snapshot": snapshot(area)]) }
    // Control event proves the observer runloop can receive notifications.
    _ = try script("perform action \"set_surface_title:\(token)-control\" on terminal id \"\(firstID)\"")
    pump(0.7)
    _ = try script("perform action \"set_surface_title:\(token)\" on terminal id \"\(firstID)\"")
    pump(0.7)
    record("C05-notifications", "observed", ["events": notifications, "dropped": droppedNotifications])
    _ = try confirmedPhase("ALT"); pump(0.7)
    record("C04-alternate", "observed", ["snapshot": snapshot(area)])
    _ = try confirmedPhase("NORMAL"); pump(0.7)
    record("C04-restored", "observed", ["snapshot": snapshot(area)])
    _ = try confirmedPhase("LARGE"); pump(0.6)
    for i in 0..<3 { record("C05-cost", "observed", ["sample": i, "snapshot": snapshot(area)]); pump(0.55) }
    _ = try confirmedPhase("AFTER_LARGE")
    let secondID = try script(fixtureConfig("SECOND") + """
    
    set t to new tab in window id "\(windowID)" with configuration cfg
    return id of terminal 1 of t
    """)
    ownedTerminalIDs.append(secondID)
    pump(0.8)
    let tabWindows = fixtureWindows()
    let visibleCandidates = tabWindows.flatMap { areas($0) }
    guard visibleCandidates.count == 1 else { throw NSError(domain: "second fixture pane not unique", code: 1) }
    let secondArea = visibleCandidates[0]
    guard snapshot(secondArea)["other_tab"] as? Bool == true else { throw NSError(domain: "second fixture marker missing", code: 1) }
    let firstInVisibleTree = tabWindows.flatMap { areas($0) }.contains { CFEqual($0, area) }
    record("C06-hidden", "observed", ["matching_windows": tabWindows.count,
        "text_area_counts": tabWindows.map { areas($0).count }, "first_in_tree": firstInVisibleTree,
        "first_reference": snapshot(area), "second_terminal_id": secondID])
    _ = try confirmedPhase("HIDDEN"); pump(0.8)
    record("C06-hidden-updated", "observed", ["first_reference": snapshot(area)])
    _ = try script("focus terminal id \"\(firstID)\""); pump(0.7)
    record("C06-refocused", "observed", ["first_reference": snapshot(area)])
    let splitID = try script(fixtureConfig("SPLIT") + "\nset s to split terminal id \"\(firstID)\" direction right with configuration cfg\nreturn id of s")
    ownedTerminalIDs.append(splitID)
    pump(0.8)
    record("C06-split", "observed", ["text_area_counts": fixtureWindows().map { areas($0).count },
        "split_terminal_id": splitID, "ambiguous_values_read": false])
    record("C06-split-reference", "observed", ["first_reference": snapshot(area)])
    // Stop only controlled fixtures via their control file, not arbitrary terminal input.
    try phase("EXIT"); pump(1)
    // Some configurations retain an exited pane. Explicitly close only owned IDs.
    for id in ownedTerminalIDs {
        _ = try script("if exists terminal id \"\(id)\" then close terminal id \"\(id)\"")
    }
    pump(0.5)
    record("C07", "observed", ["closed_reference": snapshot(secondArea), "previously_valid_second_reference": true,
        "split_invalidated_first_reference": snapshot(area), "fixture_windows_remaining": fixtureWindows().count])
    // Same title/cwd, new surface. Old AX object must not acquire the replacement.
    try phase("")
    let replacement = try script(fixtureConfig("REPLACEMENT") + "\nset w to new window with configuration cfg\nreturn id of terminal 1 of selected tab of w")
    ownedTerminalIDs.append(replacement)
    pump(0.8)
    record("C07-replacement", "observed", ["old_reference": snapshot(secondArea), "replacement_terminal_id": replacement,
        "id_changed": replacement != firstID, "replacement_windows": fixtureWindows().count])
} catch {
    failed = true
    record("ERROR", "failed", ["reason": String(describing: error)])
}
if let observer { CFRunLoopRemoveSource(CFRunLoopGetCurrent(), AXObserverGetRunLoopSource(observer), .defaultMode) }
try? phase("EXIT"); pump(2)
var closeFailures: [String] = []
for id in ownedTerminalIDs {
    do { _ = try script("if exists terminal id \"\(id)\" then close terminal id \"\(id)\"") }
    catch { closeFailures.append(id) }
}
pump(0.5)
let allIDs = try? script("return id of every terminal")
let remaining = allIDs.map { ids in ownedTerminalIDs.filter { ids.contains($0) } }
// Check owned terminal UUIDs too: title drift must not masquerade as cleanup.
record("C08", remaining?.isEmpty == true ? "passed" : "needs_cleanup", ["fixture_windows_remaining": fixtureWindows().count,
    "owned_terminal_ids_remaining": remaining ?? ownedTerminalIDs, "terminal_id_query_succeeded": allIDs != nil,
    "explicit_close_failures": closeFailures,
    "non_fixture_value_reads_attempted_by_probe": false, "clipboard_used": false, "production_code_changed": false])
if failed || remaining?.isEmpty != true { exit(1) }
