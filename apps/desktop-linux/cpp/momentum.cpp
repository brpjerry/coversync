#include "momentum.h"

#include <QtCore/QCoreApplication>
#include <QtGui/QWheelEvent>
#include <QtQuick/QQuickWindow>

#include <algorithm>
#include <cstdio>

namespace {

// `name`'s value in the environment as seconds, given there in milliseconds and at least `least` of them.
void fromEnvironment(const char *name, double least, double &seconds)
{
    bool ok = false;
    const double ms = qgetenv(name).toDouble(&ok);
    if (ok)
        seconds = std::max(ms, least) / 1000;
}

rust::Slice<const Sample> slice(const std::vector<Sample> &samples)
{
    return rust::Slice<const Sample>(samples.data(), samples.size());
}

} // namespace

bool Momentum::enabled()
{
    return qgetenv("COVERSYNC_MOMENTUM") != "0";
}

Momentum::Momentum(QQuickWindow *window) : QObject(window), m_window(window)
{
    m_clock.start();
    fromEnvironment("COVERSYNC_MOMENTUM_MS", 1, m_tuning.tau);
    fromEnvironment("COVERSYNC_MOMENTUM_RECENT_MS", 1, m_tuning.recent);
    fromEnvironment("COVERSYNC_LIFT_LAG_MS", 0, m_tuning.lift_lag);
    fromEnvironment("COVERSYNC_LIFT_BLEND_MS", 1, m_tuning.blend);
    m_trace = qgetenv("COVERSYNC_SCROLL_TRACE") == "1";
    window->installEventFilter(this);
    // frameSwapped comes from the render thread; with `this` as context the lambda runs on this thread.
    connect(window, &QQuickWindow::frameSwapped, this, [this] { tick(); }, Qt::QueuedConnection);
}

bool Momentum::eventFilter(QObject *object, QEvent *event)
{
    switch (event->type()) {
    case QEvent::Wheel: {
        if (m_sending)
            break;
        auto *wheel = static_cast<QWheelEvent *>(event);
        const double now = seconds();
        const QPoint delta = wheel->angleDelta();
        if (m_trace) {
            std::fprintf(stderr, "W %.4f %d %d %d %llu\n", now, int(wheel->phase()), delta.x(), delta.y(),
                         static_cast<unsigned long long>(wheel->timestamp()));
        }
        if (wheel->phase() != Qt::ScrollEnd) {
            stop();
            releaseHeld(); // no lift followed the report that was held back
        }
        if (wheel->phase() == Qt::ScrollBegin)
            m_samples.clear();
        if (wheel->phase() == Qt::ScrollUpdate && !delta.isNull()) {
            m_target = object;
            m_position = wheel->position();
            m_globalPosition = wheel->globalPosition();
            m_buttons = wheel->buttons();
            m_modifiers = wheel->modifiers();
            m_inverted = wheel->inverted();
            m_device = wheel->pointingDevice();
            if (momentum_looks_like_lift_flush(slice(m_samples), delta.x(), delta.y(), now)) {
                m_held = delta;
                m_holding = true;
                if (m_trace)
                    std::fprintf(stderr, "F %.4f %d %d\n", now, delta.x(), delta.y());
                // The lift, if there is one, is already queued behind this event, and arrives before this call.
                QMetaObject::invokeMethod(this, [this] { releaseHeld(); }, Qt::QueuedConnection);
                return true;
            }
            // The device's own time for the movement (ms): events reach this thread in batches.
            m_samples.push_back({ wheel->timestamp() / 1000.0, now, double(delta.x()), double(delta.y()) });
        } else if (wheel->phase() == Qt::ScrollEnd) {
            const bool coasting = m_samples.size() >= 3 && startCoast(now);
            if (coasting)
                m_holding = false; // its distance is left out: the coast carries on from what was on screen
            else
                releaseHeld();
            m_samples.clear();
        }
        break;
    }
    case QEvent::MouseButtonPress:
    case QEvent::KeyPress:
    case QEvent::TouchBegin:
        stop();
        break;
    // The window is no longer drawn (another workspace, minimised) or shown: frames stop, and a coast would
    // otherwise wait, and carry on when the window is back, long after the fingers lifted.
    case QEvent::Expose:
        if (!m_window->isExposed())
            forget();
        break;
    case QEvent::Hide:
        forget();
        break;
    default:
        break;
    }
    return false;
}

// Scrolls the report that was held back after all.
void Momentum::releaseHeld()
{
    if (!m_holding)
        return;
    m_holding = false;
    const double time = m_samples.empty() ? 0 : m_samples.back().time + 0.004;
    m_samples.push_back({ time, seconds(), double(m_held.x()), double(m_held.y()) });
    send(m_held.x(), m_held.y(), Qt::ScrollUpdate);
}

// Starts the coast if the page was moving, or the fingers were, when they lifted.
bool Momentum::startCoast(double now)
{
    const CoastStart start = momentum_coast_start(slice(m_samples), m_holding, m_held.x(), m_held.y(), now, m_tuning);
    if (!start.starts)
        return false;
    m_coast = momentum_coast_state(start);
    // The coast takes over from the last movement, not from the lift: the time between the two (a report's
    // interval or more) would otherwise show as a frame without movement.
    m_last = m_samples.back().arrived;
    m_coasting = true;
    if (m_trace) {
        std::fprintf(stderr, "M %.4f %.1f %.1f %.4f %.1f %.1f\n", now, start.shown_x, start.shown_y, now - m_last,
                     start.fingers_x, start.fingers_y);
    }
    // Right after this event has been delivered, not a frame later.
    QMetaObject::invokeMethod(this, [this] { tick(); }, Qt::QueuedConnection);
    return true;
}

// The coast goes out as a wheel gesture of its own (begin, updates, end): Chromium drops phase-less wheel events
// that arrive just after a gesture's end, which is what a coast sent as Qt::ScrollMomentum turns into.
void Momentum::send(int dx, int dy, Qt::ScrollPhase phase)
{
    if (!m_target)
        return;
    QWheelEvent event(m_position, m_globalPosition, QPoint(qRound(dx / 2.0), qRound(dy / 2.0)), QPoint(dx, dy), m_buttons,
                      m_modifiers, phase, m_inverted, Qt::MouseEventSynthesizedByApplication, m_device);
    m_sending = true;
    QCoreApplication::sendEvent(m_target, &event);
    m_sending = false;
}

void Momentum::stop()
{
    if (m_coasting && m_begun)
        send(0, 0, Qt::ScrollEnd);
    m_coasting = false;
    m_begun = false;
}

// Ends the coast and drops the gesture under way.
void Momentum::forget()
{
    stop();
    m_holding = false;
    m_samples.clear();
}

// One step of the coast: once after the lift, then once per frame shown.
void Momentum::tick()
{
    if (!m_coasting)
        return;
    const double now = seconds();
    const double dt = now - m_last;
    if (dt < 0.001) // a frame's signal right behind the first step
        return;
    m_last = now;
    const CoastStep step = momentum_coast_step(m_coast, dt, m_tuning);
    if (m_trace)
        std::fprintf(stderr, "T %.4f %d %d\n", now, step.dx, step.dy);
    if (!m_begun) {
        send(0, 0, Qt::ScrollBegin);
        m_begun = true;
    }
    if (step.dx || step.dy)
        send(step.dx, step.dy, Qt::ScrollUpdate);
    if (step.done)
        stop();
    if (m_coasting)
        m_window->requestUpdate();
}
