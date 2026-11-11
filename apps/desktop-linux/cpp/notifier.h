// Shows a page's notifications (the web's Notification, which Qt WebEngine hands to the app) as the desktop's.
//
// Through the desktop's notification service (org.freedesktop.Notifications), under the app's name and icon, as
// libnotify does. In a sandbox (Flatpak, Snap), where that service is out of reach, through the desktop portal
// instead (org.freedesktop.portal.Notification), which knows a sandboxed app by itself.
//
// The portal isn't used outside a sandbox, though it would take an app that registers its desktop entry with
// it: its GTK backend passes the notification on to the same service without the app's name or icon, and
// with the body unescaped for a service that reads it as markup.
#pragma once

#include <QtCore/QHash>
#include <QtCore/QObject>
#include <QtCore/QPointer>
#include <QtCore/QString>
#include <QtCore/QVariantList>
#include <functional>

class QWebEngineNotification;

class Notifier : public QObject {
    Q_OBJECT

public:
    // `appId` is the app's desktop entry, `icon` an image file to show where the desktop has no icon by that
    // name. `found` hears once, when it's known whether there is anything to show notifications with.
    Notifier(const QString &appId, const QString &icon, std::function<void(bool)> found, QObject *parent = nullptr);

    // Shows `notification`, which is the notifier's from here on, in place of an earlier one it replaces.
    void present(QWebEngineNotification *notification);
    // Lets go of `notification` without showing it.
    void dismiss(QWebEngineNotification *notification);

signals:
    // A notification was chosen: the window comes up, with the desktop's token for that (or none).
    void chosen(const QByteArray &activationToken);

private slots:
    void desktopAction(uint id, const QString &action);
    void desktopToken(uint id, const QString &token);
    void desktopClosed(uint id, uint reason);
    void portalAction(const QString &id, const QString &action, const QVariantList &parameter);

private:
    enum class Service { None, Portal, Desktop };
    // One notification on screen.
    struct Shown {
        QPointer<QWebEngineNotification> notification;
        uint desktopId = 0; // the desktop service's number for it
        QString token; // the desktop's token for bringing the window up, once it's chosen
    };

    void lookForPortal();
    void lookForDesktop();
    void settle(Service service);
    void showOnDesktop(const QString &key);
    void showInPortal(const QString &key);
    void withdraw(const QString &key);
    void failed(const QString &key, QWebEngineNotification *notification, const QString &why);
    void choose(const QString &key, const QByteArray &token);
    QString keyOf(uint desktopId) const;
    QString bodyText(const QString &text) const;

    QString m_appId;
    QString m_icon;
    std::function<void(bool)> m_found;
    Service m_service = Service::None;
    bool m_markup = false; // the desktop service reads a body as markup
    quint64 m_next = 0;
    QHash<QString, Shown> m_shown; // by the portal's ID for each
};
