// The app's window: one web view, and the little the window draws itself (a page's alert, confirm and prompt).
// cpp/shell.cpp loads this, with these names set:
//   backend         what the view's signals are passed to, and the desktop's file chooser (cpp/backend.h)
//   appStoragePath  where the view keeps local storage and the like
//   appCachePath    where it keeps its HTTP cache
//   relayScript     the script that passes the fallback page's commands on (relay.js)
// and then gives the view its first page.
import QtQuick
import QtQuick.Window
import QtWebEngine

Window {
    id: window

    // The fallback page's colours (apps/desktop/ui/fallback.css), for what shows before a page has drawn and
    // for the dialogs.
    readonly property bool dark: Qt.styleHints.colorScheme !== Qt.Light
    readonly property color pageColor: dark ? "#0a0b0e" : "#f5f5f7"
    readonly property color cardColor: dark ? "#17181c" : "#ffffff"
    readonly property color lineColor: dark ? "#29ffffff" : "#29000000"
    readonly property color textColor: dark ? "#eef0f4" : "#14161b"
    readonly property color accentColor: dark ? "#9b84f5" : "#6040dd"
    readonly property color accentTextColor: dark ? "#0a0b0e" : "#ffffff"

    width: 1280
    height: 860
    minimumWidth: 720
    minimumHeight: 480
    visible: true
    title: "coversync"
    color: pageColor

    WebEngineProfilePrototype {
        id: prototype
        storageName: "coversync"
        persistentStoragePath: appStoragePath
        cachePath: appCachePath
    }

    WebEngineView {
        id: view

        // The page's file input that the desktop's file chooser is open for.
        property var fileRequest: null

        objectName: "view"
        anchors.fill: parent
        focus: true
        backgroundColor: window.pageColor
        // Nothing else of the view's may be set before this takes effect (so no plain values for its settings
        // here: they'd be applied first, to a view on the default profile, and changing the profile then crashes).
        profile: prototype.instance()

        onNewWindowRequested: function (request) {
            backend.openInBrowser(request.requestedUrl);
        }
        onRenderProcessTerminated: function (terminationStatus, exitCode) {
            backend.renderTerminated(terminationStatus, exitCode);
        }
        onJavaScriptConsoleMessage: function (level, message, lineNumber, sourceID) {
            backend.consoleMessage(level, message, lineNumber, sourceID);
        }
        // A page's alert(), confirm() and prompt(): drawn here, so Enter and Escape answer them.
        onJavaScriptDialogRequested: function (request) {
            request.accepted = true;
            dialog.ask(request);
        }
        // The desktop's file chooser, when there is one; Qt's own otherwise, and for saving.
        onFileDialogRequested: function (request) {
            const multiple = request.mode === FileDialogRequest.FileModeOpenMultiple;
            if (!backend.hasFileChooser || (!multiple && request.mode !== FileDialogRequest.FileModeOpen))
                return;
            request.accepted = true;
            if (fileRequest)
                fileRequest.dialogReject();
            fileRequest = request;
            backend.chooseFiles(multiple, request.acceptedMimeTypes);
        }
        // A menu only where it has something for the app's user: text to edit or copy, a link.
        onContextMenuRequested: function (request) {
            if (!request.isContentEditable && request.selectedText === "" && request.linkUrl.toString() === "")
                request.accepted = true;
        }

        Component.onCompleted: {
            // A page may put text on the clipboard (the pairing link's "Copy"); reading it stays off.
            settings.javascriptCanAccessClipboard = true;
            // In a script world of its own: a page's own scripts can't see it.
            userScripts.collection = [
                {
                    name: "coversync-relay",
                    sourceCode: relayScript,
                    injectionPoint: WebEngineScript.DocumentCreation,
                    worldId: WebEngineScript.ApplicationWorld,
                    runsOnSubFrames: false
                }
            ];
        }
    }

    // The view hands light or dark to its page only along with its settings: when the desktop's preference
    // changes while the app runs, changing a setting that does nothing here (icons for touch screens) passes
    // it on.
    Connections {
        target: Qt.styleHints

        function onColorSchemeChanged() {
            view.settings.touchIconsEnabled = !view.settings.touchIconsEnabled;
        }
    }

    Connections {
        target: backend

        function onFilesChosen(files) {
            const request = view.fileRequest;
            view.fileRequest = null;
            if (!request)
                return;
            if (files.length > 0)
                request.dialogAccept(files);
            else
                request.dialogReject();
        }
    }

    component DialogButton: Rectangle {
        id: button

        property alias text: label.text
        property bool primary: false
        signal clicked

        width: Math.max(84, label.implicitWidth + 32)
        height: 34
        radius: 9
        color: primary ? window.accentColor : "transparent"
        border.color: primary ? "transparent" : window.lineColor
        opacity: area.pressed ? 0.75 : 1

        Text {
            id: label
            anchors.centerIn: parent
            color: button.primary ? window.accentTextColor : window.textColor
            font.pixelSize: 14
            font.weight: Font.Medium
        }
        MouseArea {
            id: area
            anchors.fill: parent
            onClicked: button.clicked()
        }
    }

    // A page's alert(), confirm() or prompt(), over the page until it's answered.
    Item {
        id: dialog

        property var request: null
        readonly property int kind: request ? request.type : -1
        readonly property bool isAlert: kind === JavaScriptDialogRequest.DialogTypeAlert
        readonly property bool isPrompt: kind === JavaScriptDialogRequest.DialogTypePrompt
        readonly property bool isLeave: kind === JavaScriptDialogRequest.DialogTypeBeforeUnload

        function ask(asked) {
            if (request)
                request.dialogReject();
            request = asked;
            input.text = asked.defaultText;
            (isPrompt ? input : card).forceActiveFocus();
        }

        function answer(accept) {
            const asked = request;
            request = null;
            if (!asked)
                return;
            if (accept)
                asked.dialogAccept(input.text);
            else
                asked.dialogReject();
            view.forceActiveFocus();
        }

        anchors.fill: parent
        visible: request !== null

        // The page behind, dimmed and out of reach.
        Rectangle {
            anchors.fill: parent
            color: "#80000000"

            MouseArea {
                anchors.fill: parent
                acceptedButtons: Qt.AllButtons
                hoverEnabled: true
                onWheel: function (wheel) {
                    wheel.accepted = true;
                }
            }
        }

        Rectangle {
            id: card

            anchors.centerIn: parent
            width: Math.min(440, parent.width - 48)
            height: column.implicitHeight + 44
            radius: 12
            color: window.cardColor
            border.color: window.lineColor
            Keys.onPressed: function (event) {
                if (event.key === Qt.Key_Return || event.key === Qt.Key_Enter)
                    dialog.answer(true);
                else if (event.key === Qt.Key_Escape)
                    dialog.answer(dialog.isAlert);
                event.accepted = true;
            }

            Column {
                id: column

                x: 22
                y: 22
                width: parent.width - 44
                spacing: 18

                Text {
                    width: parent.width
                    text: {
                        if (!dialog.request)
                            return "";
                        if (dialog.request.message !== "")
                            return dialog.request.message;
                        return dialog.isLeave ? "Leave this page? Changes you made may not be saved." : "";
                    }
                    color: window.textColor
                    wrapMode: Text.Wrap
                    font.pixelSize: 15
                    lineHeight: 1.25
                }

                Rectangle {
                    visible: dialog.isPrompt
                    width: parent.width
                    height: 36
                    radius: 9
                    color: "transparent"
                    border.color: input.activeFocus ? window.accentColor : window.lineColor

                    TextInput {
                        id: input

                        anchors.fill: parent
                        anchors.leftMargin: 10
                        anchors.rightMargin: 10
                        verticalAlignment: TextInput.AlignVCenter
                        clip: true
                        color: window.textColor
                        selectionColor: window.accentColor
                        selectedTextColor: window.accentTextColor
                        font.pixelSize: 14
                        selectByMouse: true
                        Keys.onReturnPressed: dialog.answer(true)
                        Keys.onEnterPressed: dialog.answer(true)
                        Keys.onEscapePressed: dialog.answer(false)
                    }
                }

                Row {
                    anchors.right: parent.right
                    spacing: 10
                    layoutDirection: Qt.RightToLeft

                    DialogButton {
                        text: dialog.isLeave ? "Leave" : "OK"
                        primary: true
                        onClicked: dialog.answer(true)
                    }
                    DialogButton {
                        visible: !dialog.isAlert
                        text: dialog.isLeave ? "Stay" : "Cancel"
                        onClicked: dialog.answer(false)
                    }
                }
            }
        }
    }
}
