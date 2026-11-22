// Tray icon: headset glyph, a battery bar along the bottom, a red mic-slash
// badge while the headset mic is muted; greyed out when the headset is off.
import QtQuick
import org.kde.kirigami as Kirigami

MouseArea {
    id: icon
    property var backend
    property bool expanded: false
    signal toggle()

    readonly property bool live: backend.connected
    property bool wasExpanded: false

    hoverEnabled: true
    acceptedButtons: Qt.LeftButton
    onPressed: wasExpanded = expanded
    onClicked: if (wasExpanded === expanded) toggle()

    readonly property bool showBar: live && backend.batteryPercent !== null
    readonly property int barHeight: Math.max(3, Math.round(height * 0.16))

    Kirigami.Icon {
        id: glyph
        anchors.top: parent.top
        anchors.horizontalCenter: parent.horizontalCenter
        // Leave room for the battery bar below the headset.
        width: height
        height: icon.showBar ? parent.height - icon.barHeight - 1 : parent.height
        source: "audio-headset-symbolic"
        active: icon.containsMouse
        opacity: icon.live ? 1 : 0.4
    }

    // Battery level bar.
    Rectangle {
        id: track
        visible: icon.showBar
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.bottom: parent.bottom
        anchors.leftMargin: Math.round(parent.width * 0.1)
        anchors.rightMargin: Math.round(parent.width * 0.1)
        height: icon.barHeight
        radius: height / 2
        color: Qt.rgba(0, 0, 0, 0.45)
        border.width: 1
        border.color: Kirigami.Theme.backgroundColor

        Rectangle {
            readonly property var p: icon.backend.batteryPercent
            x: 1
            y: 1
            height: parent.height - 2
            width: Math.max(height, (parent.width - 2) * (p === null ? 0 : p) / 100)
            radius: height / 2
            color: icon.backend.charging ? Kirigami.Theme.positiveTextColor
                : (p !== null && p <= 15) ? Kirigami.Theme.negativeTextColor
                : (p !== null && p <= 30) ? Kirigami.Theme.neutralTextColor
                : Kirigami.Theme.textColor
        }
    }

    // Mute badge.
    Rectangle {
        visible: icon.backend.micMuted
        anchors.top: parent.top
        anchors.right: parent.right
        width: Math.round(parent.width * 0.55)
        height: width
        radius: width / 2
        color: Kirigami.Theme.negativeTextColor
        border.width: 1
        border.color: Kirigami.Theme.backgroundColor
        Kirigami.Icon {
            anchors.fill: parent
            anchors.margins: Math.max(1, parent.width * 0.14)
            source: "microphone-sensitivity-muted-symbolic"
            isMask: true
            color: "white"
        }
    }
}
