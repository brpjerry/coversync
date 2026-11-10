// What the window's QML calls, and the desktop's settings and file chooser, which the window follows.
//
// Both come from the desktop portal (org.freedesktop.portal.Desktop), as they do for GTK apps and browsers,
// rather than from Qt's own theme: light or dark is the desktop's preference, whatever palette Qt apps are
// set to, and a page's "Choose file" opens the desktop's file chooser, not one Qt draws inside the window.
#pragma once

#include <QtCore/QObject>
#include <QtCore/QString>
#include <QtCore/QStringList>
#include <QtCore/QUrl>
#include <QtCore/QVariantMap>
#include <QtDBus/QDBusVariant>

class Backend : public QObject {
    Q_OBJECT
    // Whether the desktop's file chooser can be asked for: its portal has answered.
    Q_PROPERTY(bool hasFileChooser READ hasFileChooser NOTIFY hasFileChooserChanged)

public:
    explicit Backend(bool console, QObject *parent = nullptr);

    bool hasFileChooser() const { return m_fileChooser; }

    // A link that would open a new window: the app has one window, so web links go to the default browser.
    Q_INVOKABLE void openInBrowser(const QUrl &url);
    // The process that draws the page ended; `status` is the view's RenderProcessTerminationStatus.
    Q_INVOKABLE void renderTerminated(int status, int exitCode);
    // A page's console message, printed in debug builds.
    Q_INVOKABLE void consoleMessage(int level, const QString &message, int line, const QString &source);
    // Opens the desktop's file chooser for a page: `accepted` is what its input takes (MIME types, or
    // extensions as ".png"). filesChosen follows, with the files' paths, or none if it was closed.
    Q_INVOKABLE void chooseFiles(bool multiple, const QStringList &accepted);

signals:
    void hasFileChooserChanged();
    void filesChosen(const QStringList &files);

private slots:
    void settingChanged(const QString &group, const QString &key, const QDBusVariant &value);
    void chooserResponse(uint response, const QVariantMap &results);

private:
    void portalAnswered(const QVariant &colorScheme);
    void applyColorScheme(const QVariant &value);

    bool m_console;
    bool m_fileChooser = false;
    quint32 m_requests = 0;
    QString m_request; // the file chooser request under way: its object path
};
