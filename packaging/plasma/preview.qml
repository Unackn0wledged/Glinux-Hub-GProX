// Development preview of the tray popup and icon without Plasma:
//   qml6 packaging/plasma/preview.qml
//   qml6 packaging/plasma/preview.qml -- /path/to/gprox [screenshot.png]
// Uses the real gprox CLI ("gprox" on PATH unless given) and PipeWire. With
// a PNG path it saves a screenshot after 3 s and quits.
import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import "org.gprox.tray/contents/ui"

ApplicationWindow {
    id: win
    visible: true
    width: Kirigami.Units.gridUnit * 34
    height: Kirigami.Units.gridUnit * 56
    title: "gprox tray preview"
    color: Kirigami.Theme.backgroundColor

    Backend {
        id: gproxd
        command: win.arg(0) || "gprox"
    }

    // Arguments after "--".
    function arg(i) {
        const a = Qt.application.arguments;
        const dd = a.indexOf("--");
        return dd >= 0 && dd + 1 + i < a.length ? a[dd + 1 + i] : "";
    }

    Timer {
        interval: 3000
        running: win.arg(1) !== ""
        onTriggered: shot.grabToImage(r => {
            r.saveToFile(win.arg(1));
            Qt.quit();
        })
    }
    Timer {
        interval: 2000
        running: true
        repeat: true
        triggeredOnStart: true
        onTriggered: gproxd.refresh()
    }

    Rectangle {
        id: shot
        anchors.fill: parent
        color: Kirigami.Theme.backgroundColor
    RowLayout {
        anchors.fill: parent
        anchors.margins: Kirigami.Units.largeSpacing
        spacing: Kirigami.Units.gridUnit
        ColumnLayout {
            Layout.alignment: Qt.AlignTop
            Repeater {
                model: [16, 22, 32, 48]
                TrayIcon {
                    required property int modelData
                    Layout.preferredWidth: modelData
                    Layout.preferredHeight: modelData
                    backend: gproxd
                }
            }
            Label {
                Layout.maximumWidth: Kirigami.Units.gridUnit * 6
                text: gproxd.summary
                wrapMode: Text.Wrap
                font: Kirigami.Theme.smallFont
            }
        }
        FullView {
            Layout.fillWidth: true
            Layout.alignment: Qt.AlignTop
            Layout.preferredHeight: implicitHeight
            backend: gproxd
            property string saved: ""
            lastEqPreset: saved
            onLastEqPresetSeen: name => saved = name
            onOpenSettings: gproxd.launch(["gprox-gui"])
        }
    }
    }
}
