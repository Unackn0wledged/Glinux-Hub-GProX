// The popup (full representation): header, headset, volume, mic, then Equalizer, Blue VO!CE and
// Headset cards. gprox settings go through
// `backend` (the gprox CLI); the two volumes are PipeWire's default output
// and input, the ones the volume keys and the headset's wheel change.
import QtCore
import QtQuick
import QtQuick.Layouts
import QtQuick.Shapes
import org.kde.kirigami as Kirigami
import org.kde.plasma.components as PC3
import org.kde.plasma.private.volume as Vol

Item {
    id: popup
    property var backend
    // Headphone EQ preset the Equalizer switch turns back on.
    property string lastEqPreset: ""
    signal lastEqPresetSeen(string name)
    signal openSettings()

    implicitWidth: Kirigami.Units.gridUnit * 24
    // Sized to its content: no scrolling.
    implicitHeight: content.implicitHeight

    readonly property var st: backend.status
    readonly property var sink: Vol.PreferredDevice.sink
    readonly property var source: Vol.PreferredDevice.source
    readonly property real normalVolume: Vol.PulseAudio.NormalVolume

    readonly property string flat: "flat"
    // Preset lists, replaced only when they change: a new model every poll
    // would close an open dropdown.
    property var eqPresets: []
    property var bvPresets: []
    readonly property bool eqOn: !!(st && st.hp_eq.selected !== flat)
    // The preset shown in the Equalizer dropdown: the selected one, or the
    // one the switch would turn back on.
    readonly property string eqShown: eqOn ? st.hp_eq.selected
        : (eqPresets.indexOf(lastEqPreset) >= 0 ? lastEqPreset : (eqPresets.length ? eqPresets[0] : ""))

    readonly property var autoOffChoices: [0, 1, 2, 5, 10, 15, 30]

    // Controls are set from each status (every poll, and the answer to
    // every change), so a failed change snaps back to the real state.
    // Controls the user is holding are left alone.
    onStChanged: sync()
    onLastEqPresetChanged: sync()

    function sameList(a, b) {
        return a.length === b.length && a.every((x, i) => x === b[i]);
    }

    function sync() {
        if (!st) {
            return;
        }
        const eq = st.hp_eq.presets.filter(n => n !== flat);
        if (!sameList(eq, eqPresets)) {
            eqPresets = eq;
        }
        const bv = st.bluevoice.presets;
        if (!sameList(bv, bvPresets)) {
            bvPresets = bv;
        }
        if (eqOn) {
            lastEqPresetSeen(st.hp_eq.selected);
        }
        eqSwitch.checked = eqOn;
        if (!eqCombo.popup.visible) {
            eqCombo.currentIndex = eqPresets.indexOf(eqShown);
        }
        bvSwitch.checked = st.bluevoice.enabled;
        if (!bvCombo.popup.visible) {
            bvCombo.currentIndex = st.bluevoice.selected ? bvPresets.indexOf(st.bluevoice.selected) : -1;
        }
        if (!autoOffCombo.popup.visible) {
            autoOffCombo.currentIndex = autoOffChoices.indexOf(st.headset.auto_off_minutes);
        }
        if (!sidetone.pressed && !sidetoneWrite.running && st.headset.sidetone !== null
                && st.headset.sidetone !== undefined) {
            sidetone.value = st.headset.sidetone;
        }
    }

    function pct(obj) {
        return obj ? Math.round(obj.volume / normalVolume * 100) : 0;
    }

    function stateText() {
        if (!backend.daemonUp) {
            return backend.daemonUnreachable ? "gproxd not running"
                : backend.daemonDown ? "gprox not available" : "Connecting…";
        }
        if (!backend.dongle) {
            return backend.headset.error ? "Dongle unavailable" : "Dongle not found";
        }
        if (!backend.connected) {
            return "Off or out of range";
        }
        return backend.charging ? "Connected · charging" : "Connected";
    }

    function hoursText() {
        const b = backend.battery;
        if (!b) {
            return "";
        }
        if (b.charging) {
            return "Charging";
        }
        if (b.hours_left !== null && b.hours_left !== undefined) {
            return "About " + Math.round(b.hours_left) + " h left";
        }
        return "";
    }

    Connections {
        target: popup.backend
        function onCommandFailed(message) {
            errorBanner.text = message;
            errorBanner.visible = true;
            errorTimer.restart();
        }
    }

    Timer {
        id: errorTimer
        interval: 8000
        onTriggered: errorBanner.visible = false
    }

    ColumnLayout {
        id: content
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        spacing: Kirigami.Units.largeSpacing

        // ── Header ────────────────────────────────────────────────
        RowLayout {
            Layout.fillWidth: true
            Layout.topMargin: Kirigami.Units.smallSpacing
            spacing: Kirigami.Units.largeSpacing

            ColumnLayout {
                Layout.fillWidth: true
                spacing: 0
                Kirigami.Heading {
                    level: 3
                    text: "PRO X Wireless"
                    font.weight: Font.DemiBold
                }
                RowLayout {
                    spacing: Kirigami.Units.smallSpacing
                    Rectangle {
                        implicitWidth: Kirigami.Units.smallSpacing * 2
                        implicitHeight: implicitWidth
                        radius: width / 2
                        color: popup.backend.connected ? Kirigami.Theme.positiveTextColor
                            : popup.backend.daemonUp && popup.backend.dongle ? Kirigami.Theme.disabledTextColor
                            : Kirigami.Theme.negativeTextColor
                    }
                    PC3.Label {
                        text: popup.stateText()
                        opacity: 0.75
                    }
                }
            }
            BatteryBadge {
                visible: popup.backend.battery !== null
                percent: popup.backend.batteryPercent
                millivolts: popup.backend.battery ? popup.backend.battery.millivolts : 0
                charging: popup.backend.charging
            }
            PC3.ToolButton {
                icon.name: "configure"
                display: PC3.AbstractButton.IconOnly
                text: "Open full settings"
                PC3.ToolTip.text: text
                PC3.ToolTip.visible: hovered
                PC3.ToolTip.delay: Kirigami.Units.toolTipDelay
                onClicked: popup.openSettings()
            }
        }

        Kirigami.InlineMessage {
            id: errorBanner
            Layout.fillWidth: true
            type: Kirigami.MessageType.Error
            showCloseButton: true
            visible: false
        }

        // ── Headset picture ───────────────────────────────────────
        Card {
            Item {
                Layout.fillWidth: true
                implicitHeight: Kirigami.Units.gridUnit * 8

                // Soft light behind the (black) headset: a radial
                // gradient circle, squashed into an ellipse.
                Shape {
                    id: glow
                    anchors.centerIn: parent
                    width: Math.min(parent.width, parent.height * 2.2)
                    height: width
                    transform: Scale {
                        origin.x: glow.width / 2
                        origin.y: glow.height / 2
                        yScale: 0.5
                    }
                    ShapePath {
                        strokeWidth: -1
                        fillGradient: RadialGradient {
                            centerX: glow.width / 2
                            centerY: glow.height / 2
                            centerRadius: glow.width / 2
                            focalX: centerX
                            focalY: centerY
                            GradientStop { position: 0.0; color: Qt.rgba(1, 1, 1, 0.30) }
                            GradientStop { position: 0.55; color: Qt.rgba(1, 1, 1, 0.10) }
                            GradientStop { position: 1.0; color: Qt.rgba(1, 1, 1, 0.0) }
                        }
                        PathAngleArc {
                            centerX: glow.width / 2
                            centerY: glow.height / 2
                            radiusX: glow.width / 2
                            radiusY: glow.height / 2
                            startAngle: 0
                            sweepAngle: 360
                        }
                    }
                }

                Image {
                    id: photo
                    anchors.fill: parent
                    anchors.margins: Kirigami.Units.smallSpacing
                    // The user's own picture (~/.config/gprox/headset.png); none is shipped.
                    source: StandardPaths.writableLocation(StandardPaths.GenericConfigLocation) + "/gprox/headset.png"
                    sourceSize.height: Kirigami.Units.gridUnit * 16
                    fillMode: Image.PreserveAspectFit
                    asynchronous: true
                    smooth: true
                    mipmap: true
                    opacity: popup.backend.connected ? 1 : 0.45
                }
                Kirigami.Icon {
                    anchors.centerIn: parent
                    visible: photo.status !== Image.Ready
                    source: "audio-headset"
                    implicitWidth: Kirigami.Units.iconSizes.enormous
                    implicitHeight: Kirigami.Units.iconSizes.enormous
                    opacity: popup.backend.connected ? 1 : 0.45
                }
            }
            PC3.Label {
                Layout.alignment: Qt.AlignHCenter
                visible: text !== ""
                text: popup.hoursText()
                opacity: 0.7
            }
        }

        // ── Volume ────────────────────────────────────────────────
        Card {
            RowLayout {
                Layout.fillWidth: true
                ColumnLayout {
                    Layout.fillWidth: true
                    spacing: 0
                    PC3.Label {
                        text: "Volume"
                        font.weight: Font.DemiBold
                    }
                    PC3.Label {
                        Layout.fillWidth: true
                        text: popup.sink ? popup.sink.description : "No output device"
                        elide: Text.ElideRight
                        font: Kirigami.Theme.smallFont
                        opacity: 0.65
                    }
                }
                PC3.Label {
                    text: popup.sink && popup.sink.muted ? "Muted" : popup.pct(popup.sink) + " %"
                    font.pixelSize: Kirigami.Units.gridUnit * 1.6
                    font.weight: Font.Bold
                }
            }
            RowLayout {
                Layout.fillWidth: true
                PC3.ToolButton {
                    icon.name: popup.sink && popup.sink.muted ? "audio-volume-muted-symbolic" : "audio-volume-high-symbolic"
                    enabled: popup.sink !== null
                    checkable: true
                    checked: !!(popup.sink && popup.sink.muted)
                    display: PC3.AbstractButton.IconOnly
                    text: checked ? "Unmute output" : "Mute output"
                    PC3.ToolTip.text: text
                    PC3.ToolTip.visible: hovered
                    onToggled: popup.sink.muted = checked
                }
                GradientSlider {
                    Layout.fillWidth: true
                    trackHeight: Kirigami.Units.smallSpacing * 3
                    enabled: popup.sink !== null && !popup.sink.muted
                    value: popup.pct(popup.sink)
                    onMoved: popup.sink.volume = Math.round(value * popup.normalVolume / 100)
                }
            }
        }

        // ── Microphone ────────────────────────────────────────────
        Card {
            RowLayout {
                Layout.fillWidth: true
                ColumnLayout {
                    Layout.fillWidth: true
                    spacing: 0
                    PC3.Label {
                        text: "Microphone"
                        font.weight: Font.DemiBold
                    }
                    PC3.Label {
                        Layout.fillWidth: true
                        text: popup.source ? popup.source.description : "No input device"
                        elide: Text.ElideRight
                        font: Kirigami.Theme.smallFont
                        opacity: 0.65
                    }
                }
                PC3.Label {
                    text: popup.source && popup.source.muted ? "Muted" : popup.pct(popup.source) + " %"
                    font.pixelSize: Kirigami.Units.gridUnit * 1.1
                    font.weight: Font.DemiBold
                }
            }
            RowLayout {
                Layout.fillWidth: true
                PC3.ToolButton {
                    icon.name: popup.source && popup.source.muted ? "microphone-sensitivity-muted-symbolic" : "audio-input-microphone-symbolic"
                    enabled: popup.source !== null
                    checkable: true
                    checked: !!(popup.source && popup.source.muted)
                    display: PC3.AbstractButton.IconOnly
                    text: checked ? "Unmute microphone" : "Mute microphone"
                    PC3.ToolTip.text: text
                    PC3.ToolTip.visible: hovered
                    onToggled: popup.source.muted = checked
                }
                GradientSlider {
                    Layout.fillWidth: true
                    enabled: popup.source !== null && !popup.source.muted
                    value: popup.pct(popup.source)
                    onMoved: popup.source.volume = Math.round(value * popup.normalVolume / 100)
                }
            }
            PC3.Label {
                visible: popup.backend.micMuted
                Layout.fillWidth: true
                text: "Muted with the headset's mute button"
                color: Kirigami.Theme.negativeTextColor
                font: Kirigami.Theme.smallFont
                wrapMode: Text.Wrap
            }
        }

        // ── gproxd not running ────────────────────────────────────
        Card {
            visible: popup.backend.daemonDown
            RowLayout {
                Layout.fillWidth: true
                Kirigami.Icon {
                    source: "dialog-warning"
                    implicitWidth: Kirigami.Units.iconSizes.medium
                    implicitHeight: Kirigami.Units.iconSizes.medium
                }
                ColumnLayout {
                    Layout.fillWidth: true
                    spacing: 0
                    PC3.Label {
                        text: popup.backend.daemonUnreachable ? "gproxd is not running" : "gprox is not available"
                        font.weight: Font.DemiBold
                    }
                    PC3.Label {
                        Layout.fillWidth: true
                        text: popup.backend.error
                        wrapMode: Text.Wrap
                        font: Kirigami.Theme.smallFont
                        opacity: 0.7
                    }
                }
            }
            PC3.Button {
                Layout.alignment: Qt.AlignRight
                visible: popup.backend.daemonUnreachable
                icon.name: "media-playback-start"
                text: "Start gproxd"
                onClicked: {
                    popup.backend.launch(["systemctl", "--user", "start", "gproxd.service"]);
                    retry.restart();
                }
                Timer {
                    id: retry
                    interval: 1500
                    onTriggered: popup.backend.refresh()
                }
            }
        }

        // ── Equalizer ─────────────────────────────────────────────
        Card {
            visible: popup.backend.daemonUp
            RowLayout {
                Layout.fillWidth: true
                PC3.Label {
                    Layout.fillWidth: true
                    text: "Equalizer"
                    font.weight: Font.DemiBold
                }
                PC3.Switch {
                    id: eqSwitch
                    enabled: popup.eqPresets.length > 0
                    onToggled: {
                        popup.backend.setHpEq(checked ? popup.eqShown : popup.flat);
                    }
                }
            }
            PC3.ComboBox {
                id: eqCombo
                Layout.fillWidth: true
                model: popup.eqPresets
                enabled: popup.eqPresets.length > 0
                opacity: popup.eqOn ? 1 : 0.6
                onActivated: index => popup.backend.setHpEq(popup.eqPresets[index])
            }
        }

        // ── Blue VO!CE ────────────────────────────────────────────
        Card {
            id: bvCard
            visible: popup.backend.daemonUp
            readonly property var bv: popup.st ? popup.st.bluevoice : null
            RowLayout {
                Layout.fillWidth: true
                PC3.Label {
                    Layout.fillWidth: true
                    text: "Blue VO!CE"
                    font.weight: Font.DemiBold
                }
                PC3.Switch {
                    id: bvSwitch
                    enabled: !!(bvCard.bv && bvCard.bv.available)
                    onToggled: popup.backend.bluevoiceEnable(checked)
                }
            }
            PC3.ComboBox {
                id: bvCombo
                Layout.fillWidth: true
                visible: !!(bvCard.bv && bvCard.bv.available)
                model: popup.bvPresets
                opacity: bvCard.bv && bvCard.bv.enabled ? 1 : 0.6
                onActivated: index => popup.backend.setBluevoice(popup.bvPresets[index])
            }
            PC3.Label {
                Layout.fillWidth: true
                visible: !!(bvCard.bv && !bvCard.bv.available)
                text: "Not set up: run gprox setup with your G HUB folder, then restart gproxd."
                wrapMode: Text.Wrap
                font: Kirigami.Theme.smallFont
                opacity: 0.7
            }
        }

        // ── Headset controls ─────────────────────────────────────
        Card {
            visible: popup.backend.daemonUp
            enabled: popup.backend.connected
            opacity: enabled ? 1 : 0.6
            spacing: Kirigami.Units.largeSpacing

            RowLayout {
                Layout.fillWidth: true
                PC3.Label {
                    Layout.fillWidth: true
                    text: "Headset"
                    font.weight: Font.DemiBold
                }
                PC3.Label {
                    visible: !popup.backend.connected
                    text: "Turn the headset on to change these"
                    font: Kirigami.Theme.smallFont
                    opacity: 0.7
                }
            }

            ColumnLayout {
                Layout.fillWidth: true
                spacing: Kirigami.Units.smallSpacing
                RowLayout {
                    Layout.fillWidth: true
                    PC3.Label {
                        Layout.fillWidth: true
                        text: "Sidetone"
                    }
                    PC3.Label {
                        text: Math.round(sidetone.value)
                        opacity: 0.75
                    }
                }
                GradientSlider {
                    id: sidetone
                    Layout.fillWidth: true
                    // Each value is one HID write: send once the slider
                    // rests (on release, or after keyboard/wheel steps).
                    onMoved: sidetoneWrite.restart()
                    Timer {
                        id: sidetoneWrite
                        interval: 300
                        onTriggered: {
                            if (sidetone.pressed) {
                                restart();
                            } else if (popup.st && Math.round(sidetone.value) !== popup.st.headset.sidetone) {
                                popup.backend.setSidetone(sidetone.value);
                            }
                        }
                    }
                }
            }

            RowLayout {
                Layout.fillWidth: true
                PC3.Label {
                    Layout.fillWidth: true
                    text: "Auto-off"
                }
                PC3.ComboBox {
                    id: autoOffCombo
                    model: popup.autoOffChoices.map(m => m === 0 ? "Never" : m === 1 ? "1 minute" : m + " minutes")
                    onActivated: index => popup.backend.setAutoOff(popup.autoOffChoices[index])
                }
            }

            RowLayout {
                // Headset mute button state; hidden until gproxd knows it
                // (it has to hear the mic once after starting).
                id: muteRow
                readonly property var m: popup.backend.connected ? popup.backend.headset.mic_muted : null
                Layout.fillWidth: true
                visible: m === true || m === false
                PC3.Label {
                    Layout.fillWidth: true
                    text: "Mic (headset button)"
                }
                Kirigami.Icon {
                    source: muteRow.m === true ? "microphone-sensitivity-muted-symbolic" : "audio-input-microphone-symbolic"
                    color: muteRow.m === true ? Kirigami.Theme.negativeTextColor : Kirigami.Theme.textColor
                    isMask: true
                    implicitWidth: Kirigami.Units.iconSizes.small
                    implicitHeight: Kirigami.Units.iconSizes.small
                }
                PC3.Label {
                    text: muteRow.m === true ? "Muted" : "Live"
                    color: muteRow.m === true ? Kirigami.Theme.negativeTextColor : Kirigami.Theme.textColor
                }
            }
        }
    }
}
