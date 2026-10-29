// Prints "id x y width height" for each on-screen window of the process
// whose pid is given, in points.
import CoreGraphics

let pid = Int(CommandLine.arguments[1])!
let windows = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as! [[String: Any]]
for w in windows where w["kCGWindowOwnerPID"] as? Int == pid && w["kCGWindowLayer"] as? Int == 0 {
    let b = w["kCGWindowBounds"] as! [String: Any]
    print(w["kCGWindowNumber"]!, b["X"]!, b["Y"]!, b["Width"]!, b["Height"]!)
}
