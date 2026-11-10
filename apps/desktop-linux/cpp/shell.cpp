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

// When the window comes back from being hidden (another workspace, say), Qt's render thread starts drawing it
// again, and the web view's picture doesn't survive that: the view stays black, and no new frame arrives
// however the page changes. Hiding the view and showing it again makes it hand over a new one.
class Reviver : public QObject {
public:
    Reviver(QQuickWindow *window, QObject *view) : QObject(window), m_window(window), m_view(view)
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
        if (exposed && !m_exposed) {
            // Once the window has taken the event in.
            QMetaObject::invokeMethod(this, [this] {
                if (!m_view)
                    return;
                m_view->setProperty("visible", false);
                m_view->setProperty("visible", true);
            }, Qt::QueuedConnection);
        }
        m_exposed = exposed;
        return false;
    }

private:
    QQuickWindow *m_window;
    QPointer<QObject> m_view;
    bool m_exposed = false;
};

// Qt takes the program's arguments for its own options (-platform and the like): it gets none of the app's.
struct Arguments {
    char name[18] = "coversync-desktop";
    char *argv[2] = { name, nullptr };
    int argc = 1;
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
    new Reviver(window, view);

    g_window = window;
    g_view = view;
    g_pages = pages;
    {
        const std::lock_guard<std::mutex> lock(g_lock);
        g_app = &app;
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
    onMainThread([] { QCoreApplication::quit(); });
}
