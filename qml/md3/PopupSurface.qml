import QtQuick
import io.lecooctrl.gui

Rectangle {
    id: surface
    radius: Appearance.rounding.normal
    color: ColorUtils.applyAlpha(Appearance.colors.colSurfaceContainerHigh, 0.98)
    border.color: Appearance.colors.colOutlineVariant
    border.width: 1

    StyledRectangularShadow {
        target: surface
        z: -1
        blur: 8
        spread: 0
        color: Qt.rgba(0, 0, 0, 0.28)
        offset: Qt.vector2d(0, 3)
    }
}
