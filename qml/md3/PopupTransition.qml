import QtQuick
import io.lecooctrl.gui

Transition {
    id: motion
    property bool closing: false

    NumberAnimation {
        properties: "opacity"
        from: motion.closing ? undefined : 0
        to: motion.closing ? 0 : 1
        duration: motion.closing ? 120 : 140
    }
    NumberAnimation {
        properties: "scale"
        from: motion.closing ? undefined : 0.96
        to: motion.closing ? 0.96 : 1
        duration: motion.closing ? 120 : Appearance.animation.elementMoveFast.duration
        easing.type: Appearance.animation.elementMoveFast.type
        easing.bezierCurve: Appearance.animation.elementMoveFast.bezierCurve
    }
}
