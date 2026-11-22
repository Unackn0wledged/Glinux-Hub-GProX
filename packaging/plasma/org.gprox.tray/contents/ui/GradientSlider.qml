// Slider with a rounded track and a gradient fill (cyan → indigo → violet).
import QtQuick
import QtQuick.Templates as T
import org.kde.kirigami as Kirigami

T.Slider {
    id: control
    // Track thickness; the volume slider is thicker than the others.
    property real trackHeight: Kirigami.Units.smallSpacing * 2
    property color colorFrom: "#22d3ee"
    property color colorMid: "#6366f1"
    property color colorTo: "#a855f7"

    from: 0
    to: 100
    stepSize: 1
    implicitWidth: Kirigami.Units.gridUnit * 10
    implicitHeight: Math.max(trackHeight, handleSize) + Kirigami.Units.smallSpacing
    focusPolicy: Qt.StrongFocus
    opacity: enabled ? 1 : 0.45
    wheelEnabled: true

    readonly property real handleSize: trackHeight + Kirigami.Units.smallSpacing * 2

    background: Rectangle {
        x: control.leftPadding
        y: control.topPadding + (control.availableHeight - height) / 2
        width: control.availableWidth
        height: control.trackHeight
        radius: height / 2
        color: Kirigami.ColorUtils.tintWithAlpha(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, 0.15)

        Rectangle {
            // Clip the gradient to the filled part, so the colours stay put
            // along the track instead of being squeezed.
            width: Math.max(height, control.visualPosition * parent.width)
            height: parent.height
            radius: parent.radius
            clip: true
            visible: control.visualPosition > 0

            Rectangle {
                width: control.availableWidth
                height: parent.height
                radius: parent.radius
                gradient: Gradient {
                    orientation: Gradient.Horizontal
                    GradientStop { position: 0.0; color: control.colorFrom }
                    GradientStop { position: 0.55; color: control.colorMid }
                    GradientStop { position: 1.0; color: control.colorTo }
                }
            }
        }
    }

    handle: Rectangle {
        x: control.leftPadding + control.visualPosition * (control.availableWidth - width)
        y: control.topPadding + (control.availableHeight - height) / 2
        width: control.handleSize
        height: width
        radius: width / 2
        color: "white"
        border.width: control.visualFocus ? 2 : 1
        border.color: control.visualFocus ? Kirigami.Theme.focusColor : Qt.rgba(0, 0, 0, 0.25)
        scale: control.pressed ? 1.1 : 1
        Behavior on scale { NumberAnimation { duration: Kirigami.Units.shortDuration } }
    }
}
