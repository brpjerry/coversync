#include "notifier.h"

#include <QtCore/QFile>
#include <QtDBus/QDBusArgument>
#include <QtDBus/QDBusConnection>
#include <QtDBus/QDBusMessage>
#include <QtDBus/QDBusPendingCallWatcher>
#include <QtDBus/QDBusPendingReply>
#include <QtDBus/QDBusVariant>
#include <QtGui/QIcon>
#include <QtQml/QQmlEngine>
#include <QtWebEngineCore/QWebEngineNotification>

#include <cstdio>

namespace {

const QString kPortal = QStringLiteral("org.freedesktop.portal.Desktop");
const QString kPortalPath = QStringLiteral("/org/freedesktop/portal/desktop");
const QString kPortalNotification = QStringLiteral("org.freedesktop.portal.Notification");
const QString kDesktop = QStringLiteral("org.freedesktop.Notifications");
const QString kDesktopPath = QStringLiteral("/org/freedesktop/Notifications");
const QString kProperties = QStringLiteral("org.freedesktop.DBus.Properties");
// The action behind a click on the notification itself.
const QString kDesktopDefault = QStringLiteral("default");
const QString kPortalDefault = QStringLiteral("open");

// Whether the app runs in a sandbox, where the portal is the way to the desktop.
bool sandboxed()
{
    return QFile::exists(QStringLiteral("/.flatpak-info")) || qEnvironmentVariableIsSet("SNAP");
}

QDBusConnection bus()
{
    return QDBusConnection::sessionBus();
}

template <typename Reply, typename Then>
void whenAnswered(const QDBusMessage &call, QObject *owner, Then &&then)
{
    auto *watcher = new QDBusPendingCallWatcher(bus().asyncCall(call), owner);
    QObject::connect(watcher, &QDBusPendingCallWatcher::finished, owner, [then = std::forward<Then>(then)](QDBusPendingCallWatcher *answered) {
        answered->deleteLater();
        then(Reply(*answered));
    });
}

} // namespace

Notifier::Notifier(const QString &appId, const QString &icon, std::function<void(bool)> found, QObject *parent)
    : QObject(parent)
    , m_appId(appId)
    , m_icon(icon)
    , m_found(std::move(found))
{
    if (!bus().isConnected())
        settle(Service::None);
    else if (sandboxed())
        lookForPortal();
    else
        lookForDesktop();
}

// The portal's parts come from different programs: its version is asked for, which only a notification portal
// that is there can tell.
void Notifier::lookForPortal()
{
    QDBusMessage get = QDBusMessage::createMethodCall(kPortal, kPortalPath, kProperties, QStringLiteral("Get"));
    get << kPortalNotification << QStringLiteral("version");
    whenAnswered<QDBusPendingReply<QDBusVariant>>(get, this, [this](const QDBusPendingReply<QDBusVariant> &reply) {
        if (reply.isError()) {
            settle(Service::None);
            return;
        }
        bus().connect(kPortal, kPortalPath, kPortalNotification, QStringLiteral("ActionInvoked"), this,
                      SLOT(portalAction(QString, QString, QVariantList)));
        settle(Service::Portal);
    });
}

void Notifier::lookForDesktop()
{
    // What it says it can do also tells that it's there (and starts it, where it's started on demand).
    const QDBusMessage ask = QDBusMessage::createMethodCall(kDesktop, kDesktopPath, kDesktop, QStringLiteral("GetCapabilities"));
    whenAnswered<QDBusPendingReply<QStringList>>(ask, this, [this](const QDBusPendingReply<QStringList> &reply) {
        if (reply.isError()) {
            settle(Service::None);
            return;
        }
        m_markup = reply.value().contains(QStringLiteral("body-markup"));
        bus().connect(kDesktop, kDesktopPath, kDesktop, QStringLiteral("ActionInvoked"), this, SLOT(desktopAction(uint, QString)));
        bus().connect(kDesktop, kDesktopPath, kDesktop, QStringLiteral("ActivationToken"), this, SLOT(desktopToken(uint, QString)));
        bus().connect(kDesktop, kDesktopPath, kDesktop, QStringLiteral("NotificationClosed"), this, SLOT(desktopClosed(uint, uint)));
        settle(Service::Desktop);
    });
}

void Notifier::settle(Service service)
{
    m_service = service;
    if (m_found)
        m_found(service != Service::None);
    m_found = nullptr;
}

void Notifier::dismiss(QWebEngineNotification *notification)
{
    QQmlEngine::setObjectOwnership(notification, QQmlEngine::CppOwnership);
    notification->close();
    notification->deleteLater();
}

void Notifier::present(QWebEngineNotification *notification)
{
    if (m_service == Service::None) {
        dismiss(notification);
        return;
    }
    // Qt WebEngine leaves it to its QML engine's garbage collector, which never hears of it.
    QQmlEngine::setObjectOwnership(notification, QQmlEngine::CppOwnership);
    notification->setParent(this);

    // One with the same tag, from the same page, takes the earlier one's place on screen.
    QString key;
    Shown shown;
    for (auto it = m_shown.begin(); it != m_shown.end(); ++it) {
        if (it->notification && it->notification->matches(notification)) {
            key = it.key();
            shown.desktopId = it->desktopId;
            it->notification->disconnect(this);
            it->notification->deleteLater();
            break;
        }
    }
    if (key.isEmpty())
        key = QStringLiteral("n%1").arg(++m_next);
    shown.notification = notification;
    m_shown.insert(key, shown);
    // The page closed it, or replaced it.
    connect(notification, &QWebEngineNotification::closed, this, [this, key, notification] {
        if (m_shown.value(key).notification == notification)
            withdraw(key);
        notification->deleteLater();
    });

    if (m_service == Service::Portal)
        showInPortal(key);
    else
        showOnDesktop(key);
    notification->show();
}

QString Notifier::bodyText(const QString &text) const
{
    return m_markup ? text.toHtmlEscaped() : text;
}

void Notifier::showOnDesktop(const QString &key)
{
    const Shown shown = m_shown.value(key);
    QVariantMap hints;
    hints.insert(QStringLiteral("desktop-entry"), m_appId);
    QDBusMessage notify = QDBusMessage::createMethodCall(kDesktop, kDesktopPath, kDesktop, QStringLiteral("Notify"));
    notify << QStringLiteral("coversync") << shown.desktopId
           // The app's icon by its name, once it's installed; the file until then.
           << (QIcon::hasThemeIcon(m_appId) ? m_appId : m_icon) << shown.notification->title()
           << bodyText(shown.notification->message()) << QStringList { kDesktopDefault, QStringLiteral("Open") } << hints << -1;
    const QPointer<QWebEngineNotification> notification = shown.notification;
    whenAnswered<QDBusPendingReply<uint>>(notify, this, [this, key, notification](const QDBusPendingReply<uint> &reply) {
        if (reply.isError()) {
            std::fprintf(stderr, "coversync: couldn't show a notification: %s\n", qUtf8Printable(reply.error().message()));
            return;
        }
        const auto it = m_shown.find(key);
        if (it != m_shown.end() && it->notification == notification) {
            it->desktopId = reply.value();
            return;
        }
        // Closed, or replaced, while the desktop was being asked: the one it put up goes.
        if (it == m_shown.end() || it->desktopId != reply.value()) {
            QDBusMessage close = QDBusMessage::createMethodCall(kDesktop, kDesktopPath, kDesktop, QStringLiteral("CloseNotification"));
            close << reply.value();
            bus().asyncCall(close);
        }
    });
}

void Notifier::showInPortal(const QString &key)
{
    const Shown shown = m_shown.value(key);
    QVariantMap notification;
    notification.insert(QStringLiteral("title"), shown.notification->title());
    notification.insert(QStringLiteral("body"), shown.notification->message());
    notification.insert(QStringLiteral("priority"), QStringLiteral("normal"));
    // Not one of the app's exported actions ("app.…"): it comes back as the ActionInvoked signal.
    notification.insert(QStringLiteral("default-action"), kPortalDefault);
    QDBusMessage add = QDBusMessage::createMethodCall(kPortal, kPortalPath, kPortalNotification, QStringLiteral("AddNotification"));
    add << key << notification;
    whenAnswered<QDBusPendingReply<>>(add, this, [](const QDBusPendingReply<> &reply) {
        if (reply.isError())
            std::fprintf(stderr, "coversync: couldn't show a notification: %s\n", qUtf8Printable(reply.error().message()));
    });
}

// Takes a notification off the screen and forgets it.
void Notifier::withdraw(const QString &key)
{
    const Shown shown = m_shown.take(key);
    if (m_service == Service::Portal) {
        QDBusMessage remove = QDBusMessage::createMethodCall(kPortal, kPortalPath, kPortalNotification, QStringLiteral("RemoveNotification"));
        remove << key;
        bus().asyncCall(remove);
    } else if (shown.desktopId != 0) {
        QDBusMessage close = QDBusMessage::createMethodCall(kDesktop, kDesktopPath, kDesktop, QStringLiteral("CloseNotification"));
        close << shown.desktopId;
        bus().asyncCall(close);
    }
}

// The user chose it: the window comes up, and the page hears of the click.
void Notifier::choose(const QString &key, const QByteArray &token)
{
    const Shown shown = m_shown.value(key);
    if (!shown.notification)
        return;
    emit chosen(token);
    shown.notification->click();
}

QString Notifier::keyOf(uint desktopId) const
{
    if (desktopId == 0)
        return QString();
    for (auto it = m_shown.constBegin(); it != m_shown.constEnd(); ++it) {
        if (it->desktopId == desktopId)
            return it.key();
    }
    return QString();
}

// The desktop's token for the click that follows (ActionInvoked comes after it).
void Notifier::desktopToken(uint id, const QString &token)
{
    const auto it = m_shown.find(keyOf(id));
    if (it != m_shown.end())
        it->token = token;
}

void Notifier::desktopAction(uint id, const QString &action)
{
    const QString key = keyOf(id);
    if (!key.isEmpty() && action == kDesktopDefault)
        choose(key, m_shown.value(key).token.toUtf8());
}

// Every app hears of every notification closed: only one of ours counts.
void Notifier::desktopClosed(uint id, uint)
{
    const QString key = keyOf(id);
    if (key.isEmpty())
        return;
    const Shown shown = m_shown.take(key);
    if (shown.notification) {
        shown.notification->disconnect(this);
        shown.notification->close();
        shown.notification->deleteLater();
    }
}

void Notifier::portalAction(const QString &id, const QString &action, const QVariantList &parameter)
{
    if (action != kPortalDefault || !m_shown.contains(id))
        return;
    // Among what comes with it: the platform's data, with the token for bringing the window up.
    QByteArray token;
    for (QVariant value : parameter) {
        if (value.canConvert<QDBusVariant>())
            value = value.value<QDBusVariant>().variant();
        QVariantMap data;
        if (value.canConvert<QDBusArgument>())
            value.value<QDBusArgument>() >> data;
        else
            data = value.toMap();
        const QString found = data.value(QStringLiteral("activation-token")).toString();
        if (!found.isEmpty())
            token = found.toUtf8();
    }
    choose(id, token);
    // The portal takes a notification down once it's chosen; the page's copy goes with it.
    const Shown shown = m_shown.take(id);
    if (shown.notification) {
        shown.notification->disconnect(this);
        shown.notification->close();
        shown.notification->deleteLater();
    }
}
