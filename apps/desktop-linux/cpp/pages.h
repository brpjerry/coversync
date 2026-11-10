// Serves the app's own pages (coversync://app/, the files in src/pages.rs) and takes the fallback page's
// commands, which are POSTs to coversync://app/__bridge (src/bridge.rs).
#pragma once

#include <QtCore/QByteArray>
#include <QtCore/QHash>
#include <QtCore/QPointer>
#include <QtCore/QUrl>
#include <QtWebEngineCore/QWebEngineUrlRequestJob>
#include <QtWebEngineCore/QWebEngineUrlSchemeHandler>

#include <functional>

class AppPages : public QWebEngineUrlSchemeHandler {
public:
    // `origin` is the pages' (coversync://app), `bridge` the address their commands go to, and `page` tells
    // the address the window shows.
    AppPages(QByteArray csp, QByteArray origin, QUrl bridge, std::function<QUrl()> page, QObject *parent = nullptr);

    // Registers the pages' scheme. Before the application object exists.
    static void registerScheme(const QByteArray &scheme);

    void requestStarted(QWebEngineUrlRequestJob *job) override;
    // Answers the command `id` with `body` (JSON).
    void reply(quint64 id, const QByteArray &body);

private:
    void respond(QWebEngineUrlRequestJob *job, const QByteArray &contentType, const QByteArray &body);

    QByteArray m_csp;
    QByteArray m_origin;
    QUrl m_bridge;
    std::function<QUrl()> m_page;
    quint64 m_next = 0;
    QHash<quint64, QPointer<QWebEngineUrlRequestJob>> m_waiting;
};
