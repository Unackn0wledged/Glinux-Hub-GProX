// A rounded card on the popup; content goes into a column with padding.
import QtQuick
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

Rectangle {
    id: card
    default property alias content: column.data
    property alias spacing: column.spacing

    Layout.fillWidth: true
    implicitHeight: column.implicitHeight + 2 * Kirigami.Units.largeSpacing
    radius: Kirigami.Units.cornerRadius * 2.5
    color: Kirigami.ColorUtils.tintWithAlpha(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, 0.055)
    border.width: 1
    border.color: Kirigami.ColorUtils.tintWithAlpha(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, 0.10)

    ColumnLayout {
        id: column
        anchors.fill: parent
        anchors.margins: Kirigami.Units.largeSpacing
        spacing: Kirigami.Units.smallSpacing
    }
}
