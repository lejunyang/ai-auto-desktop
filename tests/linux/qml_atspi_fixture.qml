import QtQuick 2.15
import QtQuick.Controls 2.15
import QtQuick.Layouts 1.15

ApplicationWindow {
    id: window
    width: 640
    height: 360
    visible: true
    title: "AI Auto Desktop QML Fixture"

    ColumnLayout {
        anchors.centerIn: parent
        spacing: 16

        TextField {
            id: entry
            objectName: "qml-fixture-entry"
            text: "QML fixture initial text"
            Accessible.name: "QML fixture text entry"
            Accessible.description: "Owned Qt Quick accessibility fixture"
            Layout.preferredWidth: 360
        }

        Button {
            id: button
            objectName: "qml-fixture-button"
            text: "Invoke QML fixture button"
            Accessible.name: text
            onClicked: status.text = "QML fixture status invoked"
        }

        Label {
            id: status
            objectName: "qml-fixture-status"
            text: "QML fixture status idle"
            Accessible.name: text
        }
    }
}
