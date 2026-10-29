// Prints, for the first on-screen window of the process whose pid is given:
//   window x y width height
//   screen x y width height
// in points, with the origin at the top left of the primary display and y
// going down. The screen is the display that holds the window's centre.
import AppKit

let pid = Int(CommandLine.arguments[1])!
let windows = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as! [[String: Any]]
guard let w = windows.first(where: { $0["kCGWindowOwnerPID"] as? Int == pid && $0["kCGWindowLayer"] as? Int == 0 }) else {
    exit(1)
}
let b = CGRect(dictionaryRepresentation: w["kCGWindowBounds"] as! CFDictionary)!
let primaryHeight = NSScreen.screens[0].frame.height
// NSScreen frames have their origin at the bottom left; flip them.
let screens = NSScreen.screens.map {
    CGRect(x: $0.frame.minX, y: primaryHeight - $0.frame.maxY, width: $0.frame.width, height: $0.frame.height)
}
let screen = screens.first(where: { $0.contains(CGPoint(x: b.midX, y: b.midY)) }) ?? screens[0]
print("window", Int(b.minX), Int(b.minY), Int(b.width), Int(b.height))
print("screen", Int(screen.minX), Int(screen.minY), Int(screen.width), Int(screen.height))
