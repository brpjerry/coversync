#include "pages.h"

#include "coversync-desktop-linux/src/ffi.cxx.h"

#include <QtCore/QBuffer>
#include <QtCore/QMultiMap>
#include <QtWebEngineCore/QWebEngineUrlScheme>

namespace {

rust::Str str(const QByteArray &bytes)
{
    return rust::Str(bytes.constData(), static_cast<std::size_t>(bytes.size()));
}

} // namespace

AppPages::AppPages(QByteArray csp, QByteArray origin, QUrl bridge, std::function<QUrl()> page, QObject *parent)
    : QWebEngineUrlSchemeHandler(parent)
    , m_csp(std::move(csp))
    , m_origin(std::move(origin))
    , m_bridge(std::move(bridge))
    , m_page(std::move(page))
{
}

void AppPages::registerScheme(const QByteArray &name)
{
    QWebEngineUrlScheme scheme(name);
    scheme.setSyntax(QWebEngineUrlScheme::Syntax::Host);
    // Local: a server's pages can't load or link to it. Secure and CORS-enabled: module scripts need both.
    // The fetch API: the fallback page's commands are POSTs.
    scheme.setFlags(QWebEngineUrlScheme::SecureScheme | QWebEngineUrlScheme::LocalScheme
                    | QWebEngineUrlScheme::CorsEnabled | QWebEngineUrlScheme::FetchApiAllowed);
    QWebEngineUrlScheme::registerScheme(scheme);
}

void AppPages::requestStarted(QWebEngineUrlRequestJob *job)
{
    const QUrl url = job->requestUrl();
    if (url.scheme() != m_bridge.scheme() || url.host() != m_bridge.host()) {
        job->fail(QWebEngineUrlRequestJob::UrlNotFound);
        return;
    }
    const QByteArray method = job->requestMethod();
    if (url.path() == m_bridge.path()) {
        if (method != "POST") {
            job->fail(QWebEngineUrlRequestJob::RequestDenied);
            return;
        }
        QByteArray body;
        if (QIODevice *sent = job->requestBody()) {
            if (sent->isOpen() || sent->open(QIODevice::ReadOnly))
                body = sent->readAll();
        }
        const quint64 id = ++m_next;
        m_waiting.insert(id, job);
        // Answered through reply(), now or later.
        const QByteArray initiator = job->initiator().toString().toUtf8();
        const QByteArray page = m_page().toString().toUtf8();
        bridge_request(id, str(initiator), str(page),
                       rust::Slice<const std::uint8_t>(reinterpret_cast<const std::uint8_t *>(body.constData()),
                                                       static_cast<std::size_t>(body.size())));
        return;
    }
    if (method != "GET") {
        job->fail(QWebEngineUrlRequestJob::RequestDenied);
        return;
    }
    const QByteArray path = url.path().toUtf8();
    const rust::Str type = app_page_type(str(path));
    if (type.empty()) {
        job->fail(QWebEngineUrlRequestJob::UrlNotFound);
        return;
    }
    const rust::Slice<const std::uint8_t> body = app_page_body(str(path));
    // The pages are part of the binary: no copy.
    respond(job, QByteArray(type.data(), static_cast<qsizetype>(type.size())),
            QByteArray::fromRawData(reinterpret_cast<const char *>(body.data()), static_cast<qsizetype>(body.size())));
}

void AppPages::reply(quint64 id, const QByteArray &body)
{
    const QPointer<QWebEngineUrlRequestJob> job = m_waiting.take(id);
    // The page went away meanwhile.
    if (!job)
        return;
    respond(job, QByteArrayLiteral("application/json"), body);
}

void AppPages::respond(QWebEngineUrlRequestJob *job, const QByteArray &contentType, const QByteArray &body)
{
    QMultiMap<QByteArray, QByteArray> headers;
    headers.insert(QByteArrayLiteral("Content-Security-Policy"), m_csp);
    // Module scripts are fetched in CORS mode, and a custom scheme's page and script don't count as one origin
    // without this.
    headers.insert(QByteArrayLiteral("Access-Control-Allow-Origin"), m_origin);
    headers.insert(QByteArrayLiteral("Cache-Control"), QByteArrayLiteral("no-store"));
    job->setAdditionalResponseHeaders(headers);
    // The job owns the device, and outlives the read.
    auto *device = new QBuffer(job);
    device->setData(body);
    device->open(QIODevice::ReadOnly);
    job->reply(contentType, device);
}
