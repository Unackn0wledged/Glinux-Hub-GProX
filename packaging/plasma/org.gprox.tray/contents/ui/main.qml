// gprox tray widget: sits in Plasma's system tray (X-Plasma-NotificationArea).
// Icon = headset with battery bar and mute badge; popup = Popup.qml.
import QtQuick
import QtQuick.Layouts
import org.kde.plasma.plasmoid
import org.kde.plasma.core as PlasmaCore
import org.kde.kirigami as Kirigami

PlasmoidItem {
    id: root

    Backend {
        id: gproxd
        command: Plasmoid.configuration.gproxCommand || "gprox"
    }

    function openSettings() {
        gproxd.launch(["gprox-gui"]);
        root.expanded = false;
    }

    Plasmoid.icon: "audio-headset-symbolic"
    Plasmoid.title: "PRO X Wireless"
    Plasmoid.status: PlasmaCore.Types.ActiveStatus
    toolTipMainText: "PRO X Wireless"
    toolTipSubText: gproxd.summary
    preferredRepresentation: compactRepresentation

    compactRepresentation: TrayIcon {
        backend: gproxd
        expanded: root.expanded
        onToggle: root.expanded = !root.expanded
    }

    fullRepresentation: FullView {
        Layout.minimumWidth: Kirigami.Units.gridUnit * 20
        Layout.preferredWidth: Kirigami.Units.gridUnit * 24
        Layout.minimumHeight: implicitHeight
        Layout.preferredHeight: implicitHeight
        Layout.maximumHeight: implicitHeight
        backend: gproxd
        lastEqPreset: Plasmoid.configuration.lastEqPreset
        onLastEqPresetSeen: name => {
            if (Plasmoid.configuration.lastEqPreset !== name) {
                Plasmoid.configuration.lastEqPreset = name;
            }
        }
        onOpenSettings: root.openSettings()
    }

    // Status every 2 s while the popup is open, every 10 s for the icon.
    Timer {
        interval: root.expanded ? 2000 : 10000
        running: true
        repeat: true
        triggeredOnStart: true
        onTriggered: gproxd.refresh()
    }
    onExpandedChanged: if (root.expanded) gproxd.refresh()

    Plasmoid.contextualActions: [
        PlasmaCore.Action {
            text: "Open full settings"
            icon.name: "configure"
            onTriggered: root.openSettings()
        }
    ]
}
