// Moves the pointer to the given point and clicks the left button.
import CoreGraphics
import Foundation

let point = CGPoint(x: Double(CommandLine.arguments[1])!, y: Double(CommandLine.arguments[2])!)
func post(_ type: CGEventType) {
    CGEvent(mouseEventSource: nil, mouseType: type, mouseCursorPosition: point, mouseButton: .left)!
        .post(tap: .cghidEventTap)
}
post(.mouseMoved)
usleep(150_000)
post(.leftMouseDown)
usleep(80_000)
post(.leftMouseUp)
