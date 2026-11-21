import QtQuick
import QtQuick.Controls
import io.lecooctrl.gui

ComboBox {
    id: control

    readonly property int optionHeight: 38

    palette.buttonText: Appearance.colors.colOnSurface
    palette.text: Appearance.colors.colOnSurface

    indicator: MaterialSymbol {
        x: control.mirrored ? control.padding : control.width - width - control.padding
        y: control.topPadding + (control.availableHeight - height) / 2
        text: "arrow_drop_down"
        iconSize: 28
        color: Appearance.colors.colOnSurfaceVariant
        opacity: control.enabled ? 1 : 0.38
        rotation: optionsPopup.expanded ? 180 : 0

        Behavior on rotation {
            NumberAnimation {
                duration: Appearance.animation.elementMoveFast.duration
                easing.type: Appearance.animation.elementMoveFast.type
                easing.bezierCurve: Appearance.animation.elementMoveFast.bezierCurve
            }
        }
    }

    background: Rectangle {
        implicitWidth: 120
        implicitHeight: 40
        radius: Appearance.rounding.unsharpenmore
        color: Appearance.colors.colSurfaceContainerHighest
        border.color: control.visualFocus || control.popup.visible
                      ? Appearance.colors.colPrimary : Appearance.colors.colOutlineVariant
        border.width: control.visualFocus || control.popup.visible ? 2 : 1
    }

    delegate: ItemDelegate {
        id: option
        required property int index
        required property string modelData
        width: ListView.view ? ListView.view.width : control.width
        implicitHeight: control.optionHeight
        leftPadding: 12
        rightPadding: 12
        text: modelData
        highlighted: control.highlightedIndex === index

        contentItem: StyledText {
            text: option.text
            color: Appearance.colors.colOnSurface
            verticalAlignment: Text.AlignVCenter
            elide: Text.ElideRight
        }
        background: Rectangle {
            radius: Appearance.rounding.unsharpenmore
            color: option.highlighted || option.hovered || control.currentIndex === option.index
                   ? Appearance.colors.colSurfaceContainerHighest : "transparent"
        }
    }

    popup: Popup {
        id: optionsPopup
        property bool opensAbove: false
        property bool expanded: false
        y: opensAbove ? -height - 4 : control.height + 4
        width: control.width
        padding: 6
        margins: 8
        onAboutToShow: {
            expanded = true
            // The delegate list is empty before opening; size from the model instead.
            const preferred = Math.min(control.count * control.optionHeight + topPadding + bottomPadding, 260)
            const above = control.mapToItem(null, 0, 0).y - topMargin - 4
            const below = control.Window.height - control.mapToItem(null, 0, control.height).y - bottomMargin - 4
            opensAbove = below < preferred && above > below
            height = Math.max(0, Math.min(preferred, opensAbove ? above : below))
        }
        onAboutToHide: expanded = false
        enter: PopupTransition { }
        exit: PopupTransition { closing: true }
        transformOrigin: opensAbove ? Popup.Bottom : Popup.Top
        background: PopupSurface { }
        contentItem: ListView {
            clip: true
            model: optionsPopup.visible ? control.delegateModel : null
            currentIndex: control.highlightedIndex
            ScrollIndicator.vertical: ScrollIndicator { }
        }
    }
}
