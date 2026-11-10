// Momentum for two-finger scrolling: Qt WebEngine has none on Linux, so the page would stop dead when the
// fingers lift. This carries the scroll on from the speed it had, slowing down, once per frame the window shows.
//
// Qt WebEngine scrolls by QWheelEvent::angleDelta (2 angle units are 1 px, with 3 scroll lines) and ignores
// pixelDelta, so the speeds in here are in angle units per second.
//
// A trackpad smoother (the Magic Trackpad plugin for Hyprland) shows the fingers' path late, and on lift sends
// what it still held as one last report: about 45 ms of movement, ten times a normal report. Scrolled as it
// comes, that is a jump of one frame; counted into the speed, it starts the coast twice too fast. So a report
// that large is held back for a moment. If the lift follows, its distance isn't scrolled: it only tells how
// fast the fingers were at the very end, which the coast eases towards from the speed that was on screen.
// Otherwise it's scrolled after all.
//
// Environment, for tuning:
//   COVERSYNC_MOMENTUM=0               no coast: the page stops when the fingers lift
//   COVERSYNC_MOMENTUM_MS=<ms>         the coast's speed falls to 1/e in this time (325)
//   COVERSYNC_MOMENTUM_RECENT_MS=<ms>  how far back the speed on screen is weighted (30)
//   COVERSYNC_LIFT_LAG_MS=<ms>         the movement a lift's last report is taken to hold (45); 0 ignores its size
//   COVERSYNC_LIFT_BLEND_MS=<ms>       how fast the coast moves from the speed on screen to the fingers' (40)
//   COVERSYNC_SCROLL_TRACE=1           logs each wheel event (W), held report (F), coast (M) and coast step (T)
#pragma once

#include <QtCore/QElapsedTimer>
#include <QtCore/QObject>
#include <QtCore/QPoint>
#include <QtCore/QPointF>

#include <vector>

class QPointingDevice;
class QQuickWindow;

class Momentum : public QObject {
public:
    // Watches `window`'s wheel events, and sends it the coast's.
    explicit Momentum(QQuickWindow *window);

    // Whether the environment leaves momentum on.
    static bool enabled();

protected:
    bool eventFilter(QObject *object, QEvent *event) override;

private:
    struct Sample {
        double time; // the device's, in seconds
        double arrived; // ours
        double x;
        double y;
    };

    double seconds() const { return m_clock.nsecsElapsed() / 1e9; }
    bool looksLikeLiftFlush(QPoint delta, double now) const;
    void releaseHeld();
    bool startCoast(double now);
    void send(int dx, int dy, Qt::ScrollPhase phase);
    void stop();
    void tick();

    QQuickWindow *m_window;
    double m_tau = 0.325;
    double m_recent = 0.030;
    double m_liftLag = 0.045;
    double m_blendTau = 0.040;
    bool m_trace = false;
    QElapsedTimer m_clock;

    // The gesture under way: its movements, and the last one's details, which the coast's events copy.
    std::vector<Sample> m_samples;
    QObject *m_target = nullptr;
    QPointF m_position;
    QPointF m_globalPosition;
    Qt::MouseButtons m_buttons;
    Qt::KeyboardModifiers m_modifiers;
    bool m_inverted = false;
    const QPointingDevice *m_device = nullptr;

    // A report held back in case it's a lift's last.
    QPoint m_held;
    bool m_holding = false;

    // The coast: the fingers' speed, slowing; what the speed on screen had over or under it, fading; and the
    // fraction of an angle unit not sent yet.
    double m_velocity[2] = { 0, 0 };
    double m_excess[2] = { 0, 0 };
    double m_carry[2] = { 0, 0 };
    double m_last = 0;
    bool m_coasting = false;
    bool m_begun = false;
    bool m_sending = false;
};
