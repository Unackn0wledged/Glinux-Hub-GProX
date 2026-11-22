// Pill with a battery icon and the charge ("64 %"), green while charging.
import QtQuick
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.plasma.components as PC3

Rectangle {
    id: badge
    property var percent: null      // 0–100 or null (no battery model)
    property int millivolts: 0
    property bool charging: false

    readonly property color tone: charging ? Kirigami.Theme.positiveTextColor
        : (percent !== null && percent <= 15) ? Kirigami.Theme.negativeTextColor
        : (percent !== null && percent <= 30) ? Kirigami.Theme.neutralTextColor
        : Kirigami.Theme.textColor

    implicitWidth: row.implicitWidth + Kirigami.Units.largeSpacing * 1.5
    implicitHeight: row.implicitHeight + Kirigami.Units.smallSpacing * 1.5
    radius: height / 2
    color: Kirigami.ColorUtils.tintWithAlpha(Kirigami.Theme.backgroundColor, tone, 0.16)

    function iconName() {
        const p = percent === null ? 50 : percent;
        const step = Math.min(100, Math.max(0, Math.round(p / 10) * 10));
        return "battery-" + String(step).padStart(3, "0") + (charging ? "-charging" : "") + "-symbolic";
    }

    RowLayout {
        id: row
        anchors.centerIn: parent
        spacing: Kirigami.Units.smallSpacing
        Kirigami.Icon {
            source: badge.iconName()
            implicitWidth: Kirigami.Units.iconSizes.small
            implicitHeight: Kirigami.Units.iconSizes.small
            color: badge.tone
            isMask: true
        }
        PC3.Label {
            text: badge.percent !== null ? badge.percent + " %" : (badge.millivolts / 1000).toFixed(2) + " V"
            color: badge.tone
            font.weight: Font.DemiBold
        }
    }
}
