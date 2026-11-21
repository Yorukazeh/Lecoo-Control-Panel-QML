import QtQuick
import QtQuick.Controls
import io.lecooctrl.gui

Menu {
    id: menu
    required property TextInput editor
    width: 176
    padding: 6
    margins: 8
    enter: PopupTransition { }
    exit: PopupTransition { closing: true }
    background: PopupSurface { }

    delegate: MenuItem {
        id: entry
        implicitHeight: 36
        leftPadding: 12
        contentItem: StyledText {
            text: entry.text
            color: Appearance.colors.colOnSurface
            opacity: entry.enabled ? 1 : 0.42
            verticalAlignment: Text.AlignVCenter
        }
        background: Rectangle {
            radius: Appearance.rounding.unsharpenmore
            color: entry.highlighted ? Appearance.colors.colSurfaceContainerHighest : "transparent"
        }
    }

    Action {
        text: I18n.t("edit.cut")
        enabled: !menu.editor.readOnly && menu.editor.selectedText.length > 0
        onTriggered: menu.editor.cut()
    }
    Action {
        text: I18n.t("edit.copy")
        enabled: menu.editor.selectedText.length > 0
        onTriggered: menu.editor.copy()
    }
    Action {
        text: I18n.t("edit.paste")
        enabled: menu.editor.canPaste
        onTriggered: menu.editor.paste()
    }
    Action {
        text: I18n.t("edit.selectAll")
        enabled: menu.editor.length > 0
        onTriggered: menu.editor.selectAll()
    }
}
