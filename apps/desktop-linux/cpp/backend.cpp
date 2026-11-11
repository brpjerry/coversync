#include "backend.h"

#include "coversync-desktop-linux/src/ffi.cxx.h"

#include <QtCore/QCoreApplication>
#include <QtCore/QList>
#include <QtDBus/QDBusArgument>
#include <QtDBus/QDBusConnection>
#include <QtDBus/QDBusMessage>
#include <QtDBus/QDBusMetaType>
#include <QtDBus/QDBusObjectPath>
#include <QtDBus/QDBusPendingCallWatcher>
#include <QtDBus/QDBusPendingReply>
#include <QtGui/QDesktopServices>
#include <QtGui/QGuiApplication>
#include <QtGui/QStyleHints>

#include <cstdio>

namespace {

const QString kPortal = QStringLiteral("org.freedesktop.portal.Desktop");
const QString kPortalPath = QStringLiteral("/org/freedesktop/portal/desktop");
const QString kSettings = QStringLiteral("org.freedesktop.portal.Settings");
const QString kFileChooser = QStringLiteral("org.freedesktop.portal.FileChooser");
const QString kRequest = QStringLiteral("org.freedesktop.portal.Request");
const QString kAppearance = QStringLiteral("org.freedesktop.appearance");
const QString kColorScheme = QStringLiteral("color-scheme");
// How long the window's setup waits for the portal's answer.
constexpr int kPortalWaitMs = 400;

// The file chooser's filters, as the portal takes them: a(sa(us)).
struct Condition {
    uint type = 0; // 0: a glob, 1: a MIME type
    QString pattern;
};
struct Filter {
    QString name;
    QList<Condition> conditions;
};

QDBusArgument &operator<<(QDBusArgument &argument, const Condition &condition)
{
    argument.beginStructure();
    argument << condition.type << condition.pattern;
    argument.endStructure();
    return argument;
}

const QDBusArgument &operator>>(const QDBusArgument &argument, Condition &condition)
{
    argument.beginStructure();
    argument >> condition.type >> condition.pattern;
    argument.endStructure();
    return argument;
}

QDBusArgument &operator<<(QDBusArgument &argument, const Filter &filter)
{
    argument.beginStructure();
    argument << filter.name << filter.conditions;
    argument.endStructure();
    return argument;
}

const QDBusArgument &operator>>(const QDBusArgument &argument, Filter &filter)
{
    argument.beginStructure();
    argument >> filter.name >> filter.conditions;
    argument.endStructure();
    return argument;
}

} // namespace

Q_DECLARE_METATYPE(Condition)
Q_DECLARE_METATYPE(Filter)

Backend::Backend(bool console, QObject *parent) : QObject(parent), m_console(console)
{
    qDBusRegisterMetaType<Condition>();
    qDBusRegisterMetaType<QList<Condition>>();
    qDBusRegisterMetaType<Filter>();
    qDBusRegisterMetaType<QList<Filter>>();

    QDBusConnection bus = QDBusConnection::sessionBus();
    if (!bus.isConnected())
        return;
    bus.connect(kPortal, kPortalPath, kSettings, QStringLiteral("SettingChanged"), this,
                SLOT(settingChanged(QString, QString, QDBusVariant)));
    lookForFileChooser();
    QDBusMessage read = QDBusMessage::createMethodCall(kPortal, kPortalPath, kSettings, QStringLiteral("Read"));
    read << kAppearance << kColorScheme;
    // The web view takes light or dark as it is set up, so this is asked before there is one, and waited for;
    // not for long, since a session without the portal takes its time to say so.
    const QDBusMessage answer = bus.call(read, QDBus::Block, kPortalWaitMs);
    if (answer.type() == QDBusMessage::ReplyMessage && !answer.arguments().isEmpty()) {
        portalAnswered(answer.arguments().constFirst());
        return;
    }
    // Late is better than never: the scheme then changes under the page, and the file chooser turns up.
    auto *watcher = new QDBusPendingCallWatcher(bus.asyncCall(read), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this](QDBusPendingCallWatcher *call) {
        call->deleteLater();
        const QDBusPendingReply<QDBusVariant> reply = *call;
        if (!reply.isError())
            portalAnswered(QVariant::fromValue(reply.value()));
    });
}

void Backend::portalAnswered(const QVariant &colorScheme)
{
    applyColorScheme(colorScheme);
}

// The portal's parts come from different programs: that its settings answer doesn't mean it has a file chooser.
// Its version is asked for, which only a file chooser that is there can tell.
void Backend::lookForFileChooser()
{
    QDBusConnection bus = QDBusConnection::sessionBus();
    QDBusMessage get = QDBusMessage::createMethodCall(kPortal, kPortalPath, QStringLiteral("org.freedesktop.DBus.Properties"),
                                                      QStringLiteral("Get"));
    get << kFileChooser << QStringLiteral("version");
    auto *watcher = new QDBusPendingCallWatcher(bus.asyncCall(get), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this](QDBusPendingCallWatcher *call) {
        call->deleteLater();
        const QDBusPendingReply<QDBusVariant> reply = *call;
        setFileChooser(!reply.isError());
    });
}

void Backend::setFileChooser(bool there)
{
    if (m_fileChooser == there)
        return;
    m_fileChooser = there;
    emit hasFileChooserChanged();
}

// The desktop's preference (0: none, 1: dark, 2: light) decides what pages get for prefers-color-scheme.
void Backend::applyColorScheme(const QVariant &value)
{
    // Read answers with a variant inside a variant.
    QVariant inner = value;
    while (inner.canConvert<QDBusVariant>())
        inner = inner.value<QDBusVariant>().variant();
    bool ok = false;
    const uint preference = inner.toUInt(&ok);
    if (!ok)
        return;
    QStyleHints *hints = QGuiApplication::styleHints();
    if (preference == 1)
        hints->setColorScheme(Qt::ColorScheme::Dark);
    else if (preference == 2)
        hints->setColorScheme(Qt::ColorScheme::Light);
    else
        hints->unsetColorScheme();
}

void Backend::settingChanged(const QString &group, const QString &key, const QDBusVariant &value)
{
    if (group == kAppearance && key == kColorScheme)
        applyColorScheme(value.variant());
}

void Backend::openInBrowser(const QUrl &url)
{
    const QByteArray uri = url.toString(QUrl::FullyEncoded).toUtf8();
    const rust::String target = browser_url(rust::Str(uri.constData(), static_cast<std::size_t>(uri.size())));
    if (target.empty())
        return;
    const QUrl address(QString::fromUtf8(target.data(), static_cast<qsizetype>(target.size())));
    if (!QDesktopServices::openUrl(address))
        std::fprintf(stderr, "coversync: couldn't open %s in the browser\n", uri.constData());
}

void Backend::renderTerminated(int status, int exitCode)
{
    render_terminated(status, exitCode);
}

void Backend::answered(const QString &id, bool accepted)
{
    ::answered(id.toULongLong(), accepted);
}

bool Backend::mayNotify(const QUrl &origin)
{
    const QByteArray asked = origin.toString(QUrl::FullyEncoded).toUtf8();
    return may_notify(rust::Str(asked.constData(), static_cast<std::size_t>(asked.size())));
}

void Backend::consoleMessage(int level, const QString &message, int line, const QString &source)
{
    if (!m_console)
        return;
    static const char *const kLevels[] = { "info", "warning", "error" };
    std::fprintf(stdout, "[%s] %s:%d: %s\n", level >= 0 && level < 3 ? kLevels[level] : "log", qUtf8Printable(source), line,
                 qUtf8Printable(message));
    std::fflush(stdout);
}

void Backend::chooseFiles(bool multiple, const QStringList &accepted)
{
    QDBusConnection bus = QDBusConnection::sessionBus();
    // One chooser at a time: the one a page asked for before goes.
    closeRequest();
    // The request's object, which answers: its path follows from this connection's name and a token of ours,
    // so the answer can be listened for before the request is made.
    const QString token = QStringLiteral("coversync%1").arg(++m_requests);
    QString sender = bus.baseService().mid(1);
    sender.replace(QLatin1Char('.'), QLatin1Char('_'));
    const QString request = QStringLiteral("/org/freedesktop/portal/desktop/request/%1/%2").arg(sender, token);
    m_request = request;
    bus.connect(kPortal, request, kRequest, QStringLiteral("Response"), this, SLOT(chooserResponse(uint, QVariantMap)));

    QVariantMap options;
    options.insert(QStringLiteral("handle_token"), token);
    options.insert(QStringLiteral("multiple"), multiple);
    Filter filter;
    filter.name = QStringLiteral("Accepted files");
    for (const QString &type : accepted) {
        const QString entry = type.trimmed();
        if (entry.startsWith(QLatin1Char('.')))
            filter.conditions.append({ 0, QLatin1Char('*') + entry });
        else if (entry.contains(QLatin1Char('/')))
            filter.conditions.append({ 1, entry });
    }
    if (!filter.conditions.isEmpty()) {
        options.insert(QStringLiteral("filters"), QVariant::fromValue(QList<Filter> { filter }));
        options.insert(QStringLiteral("current_filter"), QVariant::fromValue(filter));
    }
    QDBusMessage open = QDBusMessage::createMethodCall(kPortal, kPortalPath, kFileChooser, QStringLiteral("OpenFile"));
    // No parent window: the chooser stands on its own.
    open << QString() << QCoreApplication::translate("Backend", "Choose a file") << options;
    auto *watcher = new QDBusPendingCallWatcher(bus.asyncCall(open), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, request](QDBusPendingCallWatcher *call) {
        call->deleteLater();
        const QDBusPendingReply<QDBusObjectPath> reply = *call;
        // A later request may have taken this one's place meanwhile: that one is the page's now, and what
        // became of this one says nothing.
        if (!reply.isError() || m_request != request)
            return;
        std::fprintf(stderr, "coversync: the file chooser couldn't open: %s\n", qUtf8Printable(reply.error().message()));
        // Qt's own from now on.
        setFileChooser(false);
        forgetRequest();
        emit filesChosen({});
    });
}

// Stops listening for the request under way.
void Backend::forgetRequest()
{
    if (m_request.isEmpty())
        return;
    QDBusConnection::sessionBus().disconnect(kPortal, m_request, kRequest, QStringLiteral("Response"), this,
                                             SLOT(chooserResponse(uint, QVariantMap)));
    m_request.clear();
}

// Closes the chooser that is open for the request under way, if there is one, and forgets the request.
void Backend::closeRequest()
{
    if (m_request.isEmpty())
        return;
    QDBusConnection::sessionBus().asyncCall(QDBusMessage::createMethodCall(kPortal, m_request, kRequest, QStringLiteral("Close")));
    forgetRequest();
}

// The answer to a request: the one under way, unless it was on its way here when a later request took that
// one's place.
void Backend::chooserResponse(uint response, const QVariantMap &results)
{
    if (calledFromDBus() && message().path() != m_request)
        return;
    forgetRequest();
    QStringList files;
    // 0: chosen; anything else: closed.
    if (response == 0) {
        const QStringList uris = results.value(QStringLiteral("uris")).toStringList();
        for (const QString &uri : uris) {
            const QString file = QUrl(uri).toLocalFile();
            if (!file.isEmpty())
                files.append(file);
        }
    }
    emit filesChosen(files);
}
