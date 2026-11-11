import Foundation
import CoreGraphics

let process = Int(CommandLine.arguments[1])!
let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
guard let window = list.first(where: { ($0[kCGWindowOwnerPID as String] as? Int) == process && ($0[kCGWindowLayer as String] as? Int) == 0 }),
      let identifier = window[kCGWindowNumber as String] as? Int else {
    fputs("No on-screen native window belongs to the owned PID\n", stderr)
    exit(1)
}
let data = try JSONSerialization.data(withJSONObject: ["pid": process, "window_id": identifier])
print(String(data: data, encoding: .utf8)!)
