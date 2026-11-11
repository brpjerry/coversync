#include "shell.h"

#include "backend.h"
#include "momentum.h"
#include "pages.h"

#include "coversync-desktop-linux/src/ffi.cxx.h"

#include <QtCore/QCoreApplication>
#include <QtCore/QEvent>
#include <QtCore/QPointer>
#include <QtCore/QTimer>
#include <QtGui/QGuiApplication>
#include <QtQml/QQmlApplicationEngine>
#include <QtQml/QQmlContext>
#include <QtQml/QQmlError>
#include <QtCore/QSet>
#include <QtQuick/QQuickItem>
#include <QtQuick/QQuickWindow>
#include <QtWebEngineQuick/QQuickWebEngineProfile>
#include <QtWebEngineQuick/QtWebEngineQuick>

#include <cstdio>
#include <memory>
#include <mutex>

namespace {

// How long the HTTP cache clear may take before the first page opens without it.
constexpr int kCacheClearTimeoutMs = 5000;

// The application while run() is in its event loop, for work handed over from other threads.
std::mutex g_lock;
QCoreApplication *g_app = nullptr;
// quit() came before there was an application to tell.
bool g_quitAsked = false;

// The main thread's alone.
QPointer<QQuickWindow> g_window;
QPointer<QObject> g_view;
QPointer<AppPages> g_pages;

QString qstring(rust::Str text)
{
    return QString::fromUtf8(text.data(), static_cast<qsizetype>(text.size()));
}

QString qstring(const rust::String &text)
{
    return QString::fromUtf8(text.data(), static_cast<qsizetype>(text.size()));
}

QByteArray bytes(const rust::String &text)
{
    return QByteArray(text.data(), static_cast<qsizetype>(text.size()));
}

// Runs `work` on the main thread, after what was handed over before it. Dropped once the window has closed.
template <typename Work>
void onMainThread(Work &&work)
{
    const std::lock_guard<std::mutex> lock(g_lock);
    if (g_app)
        QMetaObject::invokeMethod(g_app, std::forward<Work>(work), Qt::QueuedConnection);
}

// QML's complaints go to stderr, whatever Qt's own logging is set to (the journal, under systemd).
void printWarnings(QQmlEngine &engine)
{
    QObject::connect(&engine, &QQmlEngine::warnings, &engine, [](const QList<QQmlError> &warnings) {
        for (const QQmlError &warning : warnings)
            std::fprintf(stderr, "coversync: %s\n", qUtf8Printable(warning.toString()));
    });
}

// Qt stops drawing a window that can't be seen (on another workspace, say), and tells its items so. Qt WebEngine
// then gives up the page's picture, and asks for no new one when the window is drawn again: the view stays
// black however the page changes. Other windows keep their picture, since Qt keeps what it drew with.
//
// PictureKeeper undoes that: it cuts the connection through which the view's inner item, which holds the
// picture, hears that drawing stops. (It still hears when Qt really lets go of what it draws with.) That item is
// Qt WebEngine's own, found by its class name, and there's a new one for each process that draws a page.
class PictureKeeper : public QObject {
public:
    PictureKeeper(QQuickWindow *window, QQuickItem *view) : QObject(window), m_window(window), m_view(view)
    {
        // Once the new item has joined the window, which is when it starts listening.
        connect(view, &QQuickItem::childrenChanged, this, [this] {
            QMetaObject::invokeMethod(this, [this] { keep(); }, Qt::QueuedConnection);
        });
        keep();
    }

    // Whether every item that holds a picture now keeps it.
    bool kept() const
    {
        bool any = false;
        for (QQuickItem *item : holders()) {
            if (!m_kept.contains(item))
                return false;
            any = true;
        }
        return any;
    }

private:
    QList<QQuickItem *> holders() const
    {
        QList<QQuickItem *> found;
        if (!m_view)
            return found;
        const QList<QQuickItem *> children = m_view->childItems();
        for (QQuickItem *child : children) {
            if (QByteArrayView(child->metaObject()->className()).contains("RenderWidgetHostViewQtDelegateItem"))
                found.append(child);
        }
        return found;
    }

    void keep()
    {
        for (QQuickItem *item : holders()) {
            if (m_kept.contains(item))
                continue;
            if (QObject::disconnect(m_window, &QQuickWindow::sceneGraphAboutToStop, item, nullptr)) {
                m_kept.insert(item);
                connect(item, &QObject::destroyed, this, [this, item] { m_kept.remove(item); });
            }
        }
    }

    QQuickWindow *m_window;
    QPointer<QQuickItem> m_view;
    QSet<QObject *> m_kept;
};

// Where PictureKeeper couldn't (a Qt WebEngine that names or wires its item otherwise): when the window is drawn
// again after it wasn't, hiding the view and showing it again makes it ask for a new picture. The window is
// black until that arrives, a tenth of a second or so.
class Reviver : public QObject {
public:
    Reviver(QQuickWindow *window, QQuickItem *view, const PictureKeeper *keeper)
        : QObject(window), m_window(window), m_view(view), m_keeper(keeper)
    {
        m_exposed = window->isExposed();
        window->installEventFilter(this);
    }

protected:
    bool eventFilter(QObject *, QEvent *event) override
    {
        if (event->type() != QEvent::Expose)
            return false;
        const bool exposed = m_window->isExposed();
        if (exposed && !m_exposed && !m_keeper->kept()) {
            // Once the window has taken the event in.
            QMetaObject::invokeMethod(this, [this] {
                if (!m_view)
                    return;
                m_view->setVisible(false);
                m_view->setVisible(true);
            }, Qt::QueuedConnection);
        }
        m_exposed = exposed;
        return false;
    }

private:
    QQuickWindow *m_window;
    QPointer<QQuickItem> m_view;
    const PictureKeeper *m_keeper;
    bool m_exposed = false;
};

// Qt takes the program's arguments for its own options (-platform and the like): it gets none of the app's.
//
// The platform theme is the one that follows the desktop's portal, over the desktop's own theme for everything
// else, whatever QT_QPA_PLATFORMTHEME names: pages get light or dark from Qt's platform theme, and a theme set
// for Qt's widgets (qt6ct, say) reports its own palette there, so a page came up light on a desktop set to
// dark. The app has no Qt widgets to theme. As an argument, which Qt takes over the variable, and not in the
// environment: what the app starts (a browser, for a link) keeps the user's theme.
struct Arguments {
    char name[18] = "coversync-desktop";
    char themeOption[15] = "-platformtheme";
    char theme[17] = "xdgdesktopportal";
    char *argv[4] = { name, themeOption, theme, nullptr };
    int argc = 3;
};

const char *const kErrorQml = R"qml(
import QtQuick
import QtQuick.Window

Window {
    id: window
    width: 460
    height: Math.max(160, text.implicitHeight + 104)
    minimumWidth: width
    maximumWidth: width
    visible: true
    title: "coversync"
    color: palette.window

    SystemPalette { id: palette }

    Text {
        id: text
        anchors { left: parent.left; right: parent.right; top: parent.top; margins: 24 }
        text: errorMessage
        color: palette.windowText
        wrapMode: Text.Wrap
        font.pixelSize: 14
    }

    Rectangle {
        id: close
        anchors { right: parent.right; bottom: parent.bottom; margins: 20 }
        width: label.implicitWidth + 36
        height: 34
        radius: 6
        color: area.pressed ? palette.mid : palette.button
        border.color: palette.mid
        focus: true
        Keys.onPressed: function (event) {
            if (event.key === Qt.Key_Return || event.key === Qt.Key_Enter || event.key === Qt.Key_Escape)
                window.close();
        }

        Text {
            id: label
            anchors.centerIn: parent
            text: "Close"
            color: palette.buttonText
            font.pixelSize: 14
        }
        MouseArea {
            id: area
            anchors.fill: parent
            onClicked: window.close()
        }
    }
}
)qml";

} // namespace

int run(const Options &options)
{
    // Both before the application object exists.
    AppPages::registerScheme(bytes(options.scheme));
    QtWebEngineQuick::initialize();

    Arguments arguments;
    QGuiApplication app(arguments.argc, arguments.argv);
    // The window's Wayland app ID, and the folder for anything Qt keeps of its own accord.
    QGuiApplication::setDesktopFileName(qstring(options.app_id));
    QCoreApplication::setApplicationName(qstring(options.app_id));
    QGuiApplication::setApplicationDisplayName(QStringLiteral("coversync"));

    Backend backend(options.console);
    QQmlApplicationEngine engine;
    printWarnings(engine);
    QQmlContext *context = engine.rootContext();
    context->setContextProperty(QStringLiteral("backend"), &backend);
    context->setContextProperty(QStringLiteral("appStoragePath"), qstring(options.storage_dir));
    context->setContextProperty(QStringLiteral("appCachePath"), qstring(options.cache_dir));
    context->setContextProperty(QStringLiteral("relayScript"), qstring(options.relay_script));
    engine.loadData(bytes(options.qml));
    if (engine.rootObjects().isEmpty()) {
        std::fprintf(stderr, "coversync: the window couldn't be set up (is Qt WebEngine's QML module installed?)\n");
        return 1;
    }
    auto *window = qobject_cast<QQuickWindow *>(engine.rootObjects().constFirst());
    QObject *view = window ? window->findChild<QObject *>(QStringLiteral("view")) : nullptr;
    auto *profile = view ? qobject_cast<QQuickWebEngineProfile *>(view->property("profile").value<QObject *>()) : nullptr;
    if (!profile) {
        std::fprintf(stderr, "coversync: the window has no web view\n");
        return 1;
    }

    // The app's own pages, before the view loads anything.
    auto *pages = new AppPages(bytes(options.csp), bytes(options.origin), QUrl(qstring(options.bridge_url)),
                               [view = QPointer<QObject>(view)] { return view ? view->property("url").toUrl() : QUrl(); }, &app);
    profile->installUrlSchemeHandler(bytes(options.scheme), pages);

    if (Momentum::enabled())
        new Momentum(window);
    if (auto *item = qobject_cast<QQuickItem *>(view))
        new Reviver(window, item, new PictureKeeper(window, item));

    g_window = window;
    g_view = view;
    g_pages = pages;
    {
        const std::lock_guard<std::mutex> lock(g_lock);
        g_app = &app;
        // Asked to end while this was being set up: once the event loop runs.
        if (g_quitAsked)
            QMetaObject::invokeMethod(&app, [] { QCoreApplication::quit(); }, Qt::QueuedConnection);
    }

    const QUrl first(qstring(options.first_url));
    const auto open = [view, first] {
        view->setProperty("url", first);
        started();
    };
    if (options.clear_cache) {
        // The clear reports only that it's done; if it never does, the page opens anyway.
        const auto reported = std::make_shared<bool>(false);
        const auto finish = [reported, open](bool cleared) {
            if (std::exchange(*reported, true))
                return;
            cache_cleared(cleared);
            open();
        };
        QObject::connect(profile, &QQuickWebEngineProfile::clearHttpCacheCompleted, &app, [finish] { finish(true); });
        QTimer::singleShot(kCacheClearTimeoutMs, &app, [finish] { finish(false); });
        profile->clearHttpCache();
    } else {
        open();
    }

    const int status = app.exec();
    {
        const std::lock_guard<std::mutex> lock(g_lock);
        g_app = nullptr;
    }
    return status;
}

void show_error(rust::Str appId, rust::Str message)
{
    Arguments arguments;
    QGuiApplication app(arguments.argc, arguments.argv);
    QGuiApplication::setDesktopFileName(qstring(appId));
    QQmlApplicationEngine engine;
    printWarnings(engine);
    engine.rootContext()->setContextProperty(QStringLiteral("errorMessage"), qstring(message));
    engine.loadData(kErrorQml);
    // Without a window to show it in, the message is on stderr already.
    if (!engine.rootObjects().isEmpty())
        app.exec();
}

void load(rust::Str url)
{
    onMainThread([url = QUrl(qstring(url))] {
        if (g_view)
            g_view->setProperty("url", url);
    });
}

void raise(rust::Str activationToken)
{
    onMainThread([token = QByteArray(activationToken.data(), static_cast<qsizetype>(activationToken.size()))] {
        if (!g_window)
            return;
        // Qt's Wayland plugin activates the window with the token it finds here, and removes it.
        if (!token.isEmpty())
            qputenv("XDG_ACTIVATION_TOKEN", token);
        if (g_window->visibility() == QWindow::Minimized)
            g_window->showNormal();
        else
            g_window->show();
        g_window->raise();
        g_window->requestActivate();
    });
}

void bridge_reply(std::uint64_t id, rust::Slice<const std::uint8_t> body)
{
    onMainThread([id, body = QByteArray(reinterpret_cast<const char *>(body.data()), static_cast<qsizetype>(body.size()))] {
        if (g_pages)
            g_pages->reply(id, body);
    });
}

void quit()
{
    const std::lock_guard<std::mutex> lock(g_lock);
    if (g_app)
        QMetaObject::invokeMethod(g_app, [] { QCoreApplication::quit(); }, Qt::QueuedConnection);
    else
        g_quitAsked = true;
}
