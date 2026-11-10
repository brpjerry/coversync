#include "momentum.h"

#include <QtCore/QCoreApplication>
#include <QtGui/QWheelEvent>
#include <QtQuick/QQuickWindow>

#include <algorithm>
#include <cmath>
#include <cstdio>

namespace {

// Slower than this (angle units per second; 120 px/s) nothing coasts, and a coast ends at a third of it.
constexpr double kSlowest = 240;
constexpr double kEnd = 40;
// The fingers had stopped if their last movement is older than this at the lift.
constexpr double kStopped = 0.05;
// A report this many times the ones before it is held back.
constexpr double kFlushFactor = 4;

// `name`'s value in the environment as seconds, given there in milliseconds and at least `least` of them.
void fromEnvironment(const char *name, double least, double &seconds)
{
    bool ok = false;
    const double ms = qgetenv(name).toDouble(&ok);
    if (ok)
        seconds = std::max(ms, least) / 1000;
}

} // namespace

bool Momentum::enabled()
{
    return qgetenv("COVERSYNC_MOMENTUM") != "0";
}

Momentum::Momentum(QQuickWindow *window) : QObject(window), m_window(window)
{
    m_clock.start();
    fromEnvironment("COVERSYNC_MOMENTUM_MS", 1, m_tau);
    fromEnvironment("COVERSYNC_MOMENTUM_RECENT_MS", 1, m_recent);
    fromEnvironment("COVERSYNC_LIFT_LAG_MS", 0, m_liftLag);
    fromEnvironment("COVERSYNC_LIFT_BLEND_MS", 1, m_blendTau);
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
            if (looksLikeLiftFlush(delta, now)) {
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
    default:
        break;
    }
    return false;
}

// A report several times the size of the ones before it, right behind them.
bool Momentum::looksLikeLiftFlush(QPoint delta, double now) const
{
    if (m_samples.size() < 3 || now - m_samples.back().arrived > 0.03)
        return false;
    double usual = 0;
    for (std::size_t i = m_samples.size() - 3; i < m_samples.size(); ++i)
        usual += std::hypot(m_samples[i].x, m_samples[i].y);
    usual /= 3;
    return std::hypot(double(delta.x()), double(delta.y())) > kFlushFactor * std::max(usual, 1.0);
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
    const Sample &last = m_samples.back();
    if (now - last.arrived > kStopped)
        return false;
    // The speed that was on screen: movement over time, with the latest movement counting most. A plain average
    // over the gesture's end is too fast after a slowing swipe and too slow after a quickening one.
    double distance[2] = { 0, 0 };
    double time = 0;
    for (std::size_t i = 1; i < m_samples.size(); ++i) {
        const Sample &sample = m_samples[i];
        const double age = last.time - sample.time;
        if (age > 0.150)
            continue;
        const double dt = sample.time - m_samples[i - 1].time;
        if (dt <= 0 || dt > 0.05)
            continue;
        const double weight = std::exp(-age / m_recent);
        distance[0] += weight * sample.x;
        distance[1] += weight * sample.y;
        time += weight * dt;
    }
    if (time <= 0)
        return false;
    const double shown[2] = { distance[0] / time, distance[1] / time };
    // The fingers' own speed at the end, from what the smoother still held: never against the page's direction,
    // and at most twice its speed.
    double fingers[2] = { shown[0], shown[1] };
    if (m_holding && m_liftLag > 0) {
        const double held[2] = { m_held.x() / m_liftLag, m_held.y() / m_liftLag };
        for (int axis = 0; axis < 2; ++axis) {
            const double limit = 2 * std::abs(shown[axis]);
            fingers[axis] = shown[axis] >= 0 ? std::clamp(held[axis], 0.0, limit) : std::clamp(held[axis], -limit, 0.0);
        }
    }
    if (std::hypot(shown[0], shown[1]) <= kSlowest && std::hypot(fingers[0], fingers[1]) <= kSlowest)
        return false;
    for (int axis = 0; axis < 2; ++axis) {
        m_velocity[axis] = fingers[axis];
        m_excess[axis] = shown[axis] - fingers[axis];
        m_carry[axis] = 0;
    }
    // The coast takes over from the last movement, not from the lift: the time between the two (a report's
    // interval or more) would otherwise show as a frame without movement.
    m_last = last.arrived;
    m_coasting = true;
    if (m_trace) {
        std::fprintf(stderr, "M %.4f %.1f %.1f %.4f %.1f %.1f\n", now, shown[0], shown[1], now - last.arrived, fingers[0],
                     fingers[1]);
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

// One step of the coast: once after the lift, then once per frame shown.
void Momentum::tick()
{
    if (!m_coasting)
        return;
    const double now = seconds();
    const double dt = std::min(now - m_last, 0.05);
    if (dt < 0.001) // a frame's signal right behind the first step
        return;
    m_last = now;
    const double keep = std::exp(-dt / m_tau);
    const double keepExcess = std::exp(-dt / m_blendTau);
    int out[2];
    for (int axis = 0; axis < 2; ++axis) {
        // The distance covered in dt at a decaying speed, plus the fading difference from the speed on screen.
        m_carry[axis] += m_velocity[axis] * m_tau * (1 - keep) + m_excess[axis] * m_blendTau * (1 - keepExcess);
        m_velocity[axis] *= keep;
        m_excess[axis] *= keepExcess;
        out[axis] = int(m_carry[axis]);
        m_carry[axis] -= out[axis];
    }
    if (m_trace)
        std::fprintf(stderr, "T %.4f %d %d\n", now, out[0], out[1]);
    if (!m_begun) {
        send(0, 0, Qt::ScrollBegin);
        m_begun = true;
    }
    if (out[0] || out[1])
        send(out[0], out[1], Qt::ScrollUpdate);
    if (std::hypot(m_velocity[0] + m_excess[0], m_velocity[1] + m_excess[1]) < kEnd && std::hypot(m_excess[0], m_excess[1]) < kEnd)
        stop();
    if (m_coasting)
        m_window->requestUpdate();
}
