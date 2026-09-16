// Manual smoke wrapper: preserve the user's clipboard in memory, seed two
// synthetic representations, execute the driver, verify both, then restore.
import AppKit
import Foundation
let board = NSPasteboard.general
let initial = board.changeCount
var saved: [NSPasteboardItem] = []
var total = 0
for item in board.pasteboardItems ?? [] {
    let copy = NSPasteboardItem()
    for type in item.types {
        guard let data = item.data(forType: type) else { fatalError("clipboard unavailable; no changes made") }
        total += data.count
        guard total <= 4 * 1024 * 1024 else { fatalError("clipboard too large; no changes made") }
        copy.setData(data, forType: type)
    }
    saved.append(copy)
}
guard board.changeCount == initial else { fatalError("clipboard changed; no changes made") }
let plain = Data("TTYbird synthetic clipboard sentinel".utf8)
let custom = Data([0, 1, 2, 255, 0, 91])
let kind = NSPasteboard.PasteboardType("org.ttybird.synthetic-export-test")
let seed = NSPasteboardItem()
seed.setData(plain, forType: .string)
seed.setData(custom, forType: kind)
board.clearContents()
guard board.writeObjects([seed]) else { fatalError("unable to seed clipboard") }
let process = Process()
process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
process.arguments = Array(CommandLine.arguments.dropFirst())
var failed = false
do { try process.run(); process.waitUntilExit() } catch { failed = true }
let count = board.changeCount
var restored = false
let matched = board.data(forType: .string) == plain && board.data(forType: kind) == custom
// Do not overwrite an unrelated copy made during the integration run.
if matched && board.changeCount == count {
    board.clearContents()
    if !saved.isEmpty { failed = !board.writeObjects(saved) || failed }
    let current = board.pasteboardItems ?? []
    restored = current.count == saved.count && zip(current, saved).allSatisfy { actual, expected in
        Set(actual.types) == Set(expected.types) && expected.types.allSatisfy { type in
            actual.data(forType: type) == expected.data(forType: type)
        }
    }
    failed = !restored || failed
} else { failed = true }
let record: [String: Any] = ["at": ISO8601DateFormatter().string(from: Date()),
    "checkpoint": "E07-clipboard", "passed": matched && restored && !failed,
    "clipboard_multitype_preserved": matched, "original_clipboard_readback_matched": restored]
let encoded = try JSONSerialization.data(withJSONObject: record, options: [.sortedKeys])
if let evidence = CommandLine.arguments.last, let output = FileHandle(forWritingAtPath: evidence) {
    output.seekToEndOfFile(); output.write(encoded); output.write(Data([10])); output.closeFile()
}
print("clipboard_multitype_preserved=\(matched) original_clipboard_readback_matched=\(restored)")
exit(failed ? 1 : process.terminationStatus)
