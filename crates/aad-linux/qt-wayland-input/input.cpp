// Opt-in Qt Widgets generic plugin. All checks and both event phases run
// on the target application's GUI thread, addressed to the observed QWidget.
#include <QAccessible>
#include <QApplication>
#include <QDBusConnection>
#include <QDragEnterEvent>
#include <QGenericPlugin>
#include <QKeyEvent>
#include <QLineEdit>
#include <QMimeData>
#include <QMouseEvent>
#include <QPointer>
#include <QScopedValueRollback>
#include <QTimer>
#include <QWindow>
#include <climits>
#include <ctime>

namespace {
QWidget *widgetFor(const QString &path) {
    static const QString prefix = QStringLiteral("/org/a11y/atspi/accessible/");
    if (!path.startsWith(prefix)) return nullptr;
    bool ok = false;
    const auto id = path.mid(prefix.size()).toUInt(&ok);
    if (!ok || id <= INT_MAX || prefix + QString::number(id) != path) return nullptr;
    auto *iface = QAccessible::accessibleInterface(id);
    if (!iface || !iface->isValid()) return nullptr;
    auto *widget = qobject_cast<QWidget *>(iface->object());
    // Virtual accessible children may share a backing QObject.
    if (!widget || QAccessible::queryAccessibleInterface(widget) != iface) return nullptr;
    const auto state = iface->state();
    if (state.passwordEdit || state.disabled || state.invisible || state.offscreen) return nullptr;
    if (auto *edit = qobject_cast<QLineEdit *>(widget); edit && edit->echoMode() != QLineEdit::Normal)
        return nullptr;
    return widget;
}

QWidget *boundWidget(const QString &target, const QString &window, bool requireActive = true) {
    auto *widget = widgetFor(target);
    auto *top = widgetFor(window);
    if (!widget || !top || !top->isWindow() || widget->window() != top ||
        !widget->isEnabled() || !widget->isVisible() || top->isMinimized() ||
        !top->windowHandle() || !top->windowHandle()->isExposed() ||
        (requireActive && (QApplication::activeWindow() != top ||
                           QGuiApplication::focusWindow() != top->windowHandle())) ||
        QApplication::activePopupWidget() || QWidget::mouseGrabber() || QWidget::keyboardGrabber())
        return nullptr;
    auto *modal = QApplication::activeModalWidget();
    return modal && modal != top ? nullptr : widget;
}

bool focused(QWidget *widget) {
    return widget->hasFocus() && QApplication::focusWidget() == widget;
}

bool hits(QWidget *widget, QPoint point) {
    auto *top = widget->window();
    const QPoint local = widget->mapFromGlobal(point);
    if (!widget->visibleRegion().contains(local)) return false;
    auto *hit = top->childAt(top->mapFromGlobal(point));
    return (hit ? hit : top) == widget;
}

// Check every logical pixel on this bounded segment before the press, and
// again after any application code that can change geometry or cover the path.
bool lineHits(QWidget *widget, QPoint start, QPoint end) {
    const qint64 dx = qint64(end.x()) - start.x(), dy = qint64(end.y()) - start.y();
    const qint64 steps = qMax(qAbs(dx), qAbs(dy));
    if (!steps || steps > 8192) return false;
    for (qint64 i = 0; i <= steps; ++i) {
        QPoint point(start.x() + qRound64(double(dx * i) / steps),
                     start.y() + qRound64(double(dy * i) / steps));
        if (!hits(widget, point)) return false;
    }
    return true;
}

Qt::MouseButton mouseButton(int button) {
    switch (button) {
    case 1: return Qt::LeftButton;
    case 2: return Qt::MiddleButton;
    case 3: return Qt::RightButton;
    default: return Qt::NoButton;
    }
}

void sendMouse(QWidget *widget, QPoint point, QEvent::Type type, Qt::MouseButton button,
               Qt::MouseButtons held, Qt::KeyboardModifiers modifiers) {
    QMouseEvent event(type, widget->mapFromGlobal(point), widget->window()->mapFromGlobal(point),
                      point, button, held, modifiers);
    QCoreApplication::sendEvent(widget, &event);
}

bool inTime(qulonglong deadline) {
    timespec now{};
    return clock_gettime(CLOCK_MONOTONIC, &now) == 0 &&
           qulonglong(now.tv_sec) * 1000000000 + qulonglong(now.tv_nsec) < deadline;
}

class DropReceiverGuard : public QObject {
    QPointer<QWidget> receiver;
    QEvent *phase = nullptr;
    bool observed = false;
public:
    explicit DropReceiverGuard(QWidget *widget) : receiver(widget) {
        qApp->installEventFilter(this);
    }
    bool deliver(QWidget *address, QEvent &event, bool permit = true) {
        observed = false;
        phase = &event;
        if (!permit) receiver.clear();
        event.ignore();
        const bool handled = QCoreApplication::sendEvent(address, &event);
        phase = nullptr;
        return handled && observed && event.isAccepted();
    }
    bool eventFilter(QObject *object, QEvent *event) override {
        if (event != phase) return false;
        if (object == receiver) {
            observed = true;
            return false;
        }
        event->ignore();
        return true;
    }
};

struct Chord {
    Qt::KeyboardModifiers modifiers;
    int key = 0;
    QString text;
};

bool parseModifiers(const QStringList &keys, Qt::KeyboardModifiers &modifiers) {
    if (keys.size() > 4) return false;
    for (const QString &key : keys) {
        const auto name = key.toLower();
        Qt::KeyboardModifier modifier;
        if (name == "ctrl" || name == "control") modifier = Qt::ControlModifier;
        else if (name == "shift") modifier = Qt::ShiftModifier;
        else if (name == "alt") modifier = Qt::AltModifier;
        else if (name == "meta" || name == "super" || name == "win") modifier = Qt::MetaModifier;
        else return false;
        if (modifiers.testFlag(modifier)) return false;
        modifiers |= modifier;
    }
    return true;
}

bool parseChord(QStringList keys, Chord &chord) {
    if (keys.isEmpty()) return false;
    const QString main = keys.takeLast().toLower();
    if (!parseModifiers(keys, chord.modifiers)) return false;
    static const QMap<QString, int> named{
        {"backspace", Qt::Key_Backspace}, {"delete", Qt::Key_Delete}, {"enter", Qt::Key_Return},
        {"return", Qt::Key_Return}, {"esc", Qt::Key_Escape}, {"escape", Qt::Key_Escape},
        {"space", Qt::Key_Space}, {"home", Qt::Key_Home}, {"end", Qt::Key_End},
        {"left", Qt::Key_Left}, {"right", Qt::Key_Right}, {"up", Qt::Key_Up}, {"down", Qt::Key_Down},
        {"pageup", Qt::Key_PageUp}, {"pagedown", Qt::Key_PageDown}, {"tab", Qt::Key_Tab}
    };
    if (main.size() == 1 && main.at(0).unicode() < 128 && main.at(0).isLetterOrNumber()) {
        chord.key = main.toUpper().at(0).unicode();
        chord.text = chord.modifiers.testFlag(Qt::ShiftModifier) ? main.toUpper() : main;
    } else if (main.startsWith('f')) {
        const int number = main.mid(1).toInt();
        if (number < 1 || number > 35 || main != "f" + QString::number(number)) return false;
        chord.key = Qt::Key_F1 + number - 1;
    } else {
        chord.key = named.value(main);
        if (chord.key == Qt::Key_Space) chord.text = " ";
        if (chord.key == Qt::Key_Return) chord.text = "\r";
    }
    if (chord.modifiers & ~Qt::ShiftModifier) chord.text.clear();
    return chord.key != 0;
}

class Input : public QObject {
    Q_OBJECT
    Q_CLASSINFO("D-Bus Interface", "org.aiautodesktop.QtWaylandInput1")
    bool delivering = false;
public:
    Input() {
        auto *timer = new QTimer(this);
        connect(timer, &QTimer::timeout, this, [this, timer] {
            // Use Qt's existing AT-SPI connection. Its unique owner is already
            // pinned by the observed NativeRef, including across app restarts.
            QDBusConnection bus("a11y");
            if (bus.isConnected() && bus.registerObject("/org/aiautodesktop/QtWaylandInput1",
                    this, QDBusConnection::ExportAllSlots)) timer->stop();
        });
        timer->start(250);
    }
public slots:
    QStringList PointerModes() const {
        return {"wheel", "hover", "click", "drag_same_widget", "text_copy_drop"};
    }

    bool PointMatches(const QString &target, const QString &window, int px, int py) const {
        auto *widget = boundWidget(target, window);
        return widget && hits(widget, QPoint(px, py));
    }

    int Wheel(const QString &target, const QString &window, int px, int py,
              int direction, int notches, qulonglong deadline) {
        if (delivering || direction < 4 || direction > 7 || notches < 1 || notches > 30) return 0;
        QScopedValueRollback<bool> guard(delivering, true);
        auto *widget = boundWidget(target, window);
        const QPoint point(px, py);
        if (!widget || !hits(widget, point) || !inTime(deadline)) return 0;
        // A physical wheel detent is 120 eighths of a degree. Qt uses
        // positive components for up and left in the AT-SPI button mapping.
        const int delta = (direction == 4 || direction == 6 ? 120 : -120) * notches;
        const QPoint angle = direction < 6 ? QPoint(0, delta) : QPoint(delta, 0);
        QWheelEvent event(widget->mapFromGlobal(point), point, {}, angle, Qt::NoButton,
                          Qt::NoModifier, Qt::NoScrollPhase, false, Qt::MouseEventSynthesizedByApplication);
        QCoreApplication::sendEvent(widget, &event);
        return 2;
    }

    int Hover(const QString &target, const QString &window, int px, int py, qulonglong deadline) {
        if (delivering) return 0;
        QScopedValueRollback<bool> guard(delivering, true);
        auto *widget = boundWidget(target, window);
        const QPoint point(px, py);
        if (!widget || !hits(widget, point) || !inTime(deadline)) return 0;
        const QPoint local = widget->mapFromGlobal(point);
#if QT_VERSION >= QT_VERSION_CHECK(6, 3, 0)
        QHoverEvent event(QEvent::HoverMove, local, point, local);
#else
        QHoverEvent event(QEvent::HoverMove, local, local);
#endif
        QCoreApplication::sendEvent(widget, &event);
        return 2;
    }

    QByteArray Capabilities(const QStringList &targets, const QStringList &windows) {
        if (targets.size() != windows.size() || targets.size() > 5000) return {};
        QByteArray flags(targets.size(), 0);
        for (int i = 0; i < targets.size(); ++i) {
            if (auto *widget = boundWidget(targets[i], windows[i])) {
                const QRect rect = QAccessible::queryAccessibleInterface(widget)->rect();
                const QPoint center = rect.topLeft() + QPoint(rect.width()/2, rect.height()/2);
                flags[i] = (focused(widget) ? 1 : 0) | (hits(widget, center) ? 2 : 0);
            }
        }
        return flags;
    }

    QByteArray DropRoles(const QStringList &targets, const QStringList &windows) const {
        if (targets.size() != windows.size() || targets.size() > 5000) return {};
        QByteArray roles(targets.size(), 0);
        for (int index = 0; index < targets.size(); ++index) {
            auto *widget = boundWidget(targets[index], windows[index], false);
            if (!widget) continue;
            auto *edit = qobject_cast<QLineEdit *>(widget);
            roles[index] = (edit && edit->dragEnabled() && edit->hasSelectedText() ? 1 : 0) |
                           (widget->acceptDrops() ? 2 : 0);
        }
        return roles;
    }

    // 0 = refused before dispatch, 1 = press began but pair unconfirmed,
    // 2 = both events delivered to the same live object.
    int Key(const QString &target, const QString &window, const QStringList &keys, qulonglong deadline) {
        Chord chord;
        if (delivering || !parseChord(keys, chord)) return 0;
        QScopedValueRollback<bool> guard(delivering, true);
        QPointer<QWidget> widget = boundWidget(target, window);
        if (!widget || !focused(widget) || !inTime(deadline)) return 0;
        QKeyEvent press(QEvent::KeyPress, chord.key, chord.modifiers, chord.text);
        QCoreApplication::sendEvent(widget, &press);
        if (!widget || widget != boundWidget(target, window) || !focused(widget)) return 1;
        QKeyEvent release(QEvent::KeyRelease, chord.key, chord.modifiers, chord.text);
        QCoreApplication::sendEvent(widget, &release);
        return 2;
    }

    int Click(const QString &target, const QString &window, int px, int py, qulonglong deadline) {
        return MouseClick(target, window, px, py, 1, 1, {}, deadline);
    }

    int MouseClick(const QString &target, const QString &window, int px, int py,
                   int button, int count, const QStringList &modifierNames, qulonglong deadline) {
        Qt::KeyboardModifiers modifiers;
        const auto which = mouseButton(button);
        if (delivering || which == Qt::NoButton || (count != 1 && count != 2) ||
            !parseModifiers(modifierNames, modifiers)) return 0;
        QScopedValueRollback<bool> guard(delivering, true);
        QPointer<QWidget> widget = boundWidget(target, window);
        const QPoint point(px, py);
        for (int phase = 0; phase < 2 * count; ++phase) {
            const bool release = phase % 2 != 0;
            // A handler can synchronously move/delete the receiver or open a popup.
            // After a press, release only to the same live widget at the proven hit.
            if (!widget || widget != boundWidget(target, window) || !hits(widget, point) ||
                (!release && !inTime(deadline))) return phase == 0 ? 0 : 1;
            const auto type = release ? QEvent::MouseButtonRelease :
                              phase == 0 ? QEvent::MouseButtonPress : QEvent::MouseButtonDblClick;
            sendMouse(widget, point, type, which, release ? Qt::NoButton : which, modifiers);
        }
        return 2;
    }

    int Drag(const QString &target, const QString &window, int px, int py, int endX, int endY,
             int button, const QStringList &modifierNames, qulonglong deadline) {
        Qt::KeyboardModifiers modifiers;
        const auto which = mouseButton(button);
        if (delivering || which == Qt::NoButton || !parseModifiers(modifierNames, modifiers)) return 0;
        QScopedValueRollback<bool> guard(delivering, true);
        QPointer<QWidget> widget = boundWidget(target, window);
        const QPoint start(px, py), end(endX, endY);
        if (!widget || !lineHits(widget, start, end) || !inTime(deadline)) return 0;
        sendMouse(widget, start, QEvent::MouseButtonPress, which, which, modifiers);
        if (!widget || widget != boundWidget(target, window) ||
            !lineHits(widget, start, end) || !inTime(deadline)) return 1;
        sendMouse(widget, end, QEvent::MouseMove, Qt::NoButton, which, modifiers);
        if (!widget || widget != boundWidget(target, window) || !hits(widget, end)) return 1;
        sendMouse(widget, end, QEvent::MouseButtonRelease, which, Qt::NoButton, modifiers);
        return 2;
    }

    int DropText(const QString &sourceId, const QString &sourceWindow, int startX, int startY,
                 const QString &destId, const QString &destWindow, int endX, int endY,
                 int button, const QStringList &modifierNames, qulonglong deadline) {
        Qt::KeyboardModifiers modifiers;
        if (delivering || button != 1 || !parseModifiers(modifierNames, modifiers) ||
            (modifiers != Qt::NoModifier && modifiers != Qt::ControlModifier)) return 0;
        QScopedValueRollback<bool> busy(delivering, true);
        const QPoint start(startX, startY), end(endX, endY);
        QPointer<QLineEdit> source = qobject_cast<QLineEdit *>(boundWidget(sourceId, sourceWindow));
        QPointer<QWidget> dest = boundWidget(destId, destWindow, false);
        if (!source || !dest || source == dest || !source->dragEnabled() || !source->hasSelectedText()) return 0;
        const int first = source->selectionStart();
        const QString selection = source->selectedText();
        if (selection.size() > 65536) return 0;
        const QPoint local = dest->mapFromGlobal(end);
        const auto endpointsMatch = [&] {
            if (!source || !dest || source != boundWidget(sourceId, sourceWindow) ||
                dest != boundWidget(destId, destWindow, false) || !dest->acceptDrops() ||
                QApplication::mouseButtons() != Qt::NoButton ||
                !hits(source, start) || !hits(dest, end) || dest->mapFromGlobal(end) != local ||
                !source->dragEnabled() || source->selectionStart() != first ||
                source->selectedText() != selection) return false;
            const int index = source->cursorPositionAt(source->mapFromGlobal(start));
            return index >= first && index < first + selection.size();
        };
        if (!endpointsMatch() || !inTime(deadline)) return 0;
        QMimeData payload;
        payload.setText(selection);
        QDragEnterEvent enter(local, Qt::CopyAction, &payload, Qt::LeftButton, modifiers);
        if (enter.source()) return 0;
        DropReceiverGuard guard(dest);
        if (!guard.deliver(dest, enter)) return 1;
        QDragMoveEvent move(local, Qt::CopyAction, &payload, Qt::LeftButton, modifiers);
        if (enter.dropAction() != Qt::CopyAction || !endpointsMatch() || !inTime(deadline) ||
            !guard.deliver(dest, move) || move.dropAction() != Qt::CopyAction ||
            !endpointsMatch() || !inTime(deadline)) {
            QDragLeaveEvent leave;
            if (dest) guard.deliver(dest, leave, endpointsMatch());
            return 1;
        }
        QDropEvent drop(local, Qt::CopyAction, &payload, Qt::LeftButton, modifiers);
        return guard.deliver(dest, drop) && drop.dropAction() == Qt::CopyAction ? 2 : 1;
    }
};
}

class InputPlugin : public QGenericPlugin {
    Q_OBJECT
    Q_PLUGIN_METADATA(IID QGenericPluginFactoryInterface_iid FILE "input.json")
public:
    QObject *create(const QString &name, const QString &) override {
        if (name != "aad-wayland-input" || !QGuiApplication::platformName().startsWith("wayland"))
            return nullptr;
        return new Input;
    }
};
#include "input.moc"
