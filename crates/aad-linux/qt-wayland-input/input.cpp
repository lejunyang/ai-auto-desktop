// Opt-in Qt 5 Widgets generic plugin. All checks and both event phases run
// on the target application's GUI thread, addressed to the observed QWidget.
#include <QAccessible>
#include <QApplication>
#include <QDBusConnection>
#include <QGenericPlugin>
#include <QKeyEvent>
#include <QLineEdit>
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

QWidget *boundWidget(const QString &target, const QString &window) {
    auto *widget = widgetFor(target);
    auto *top = widgetFor(window);
    if (!widget || !top || !top->isWindow() || widget->window() != top ||
        !widget->isEnabled() || !widget->isVisible() || QApplication::activeWindow() != top ||
        !top->windowHandle() || !top->windowHandle()->isExposed() ||
        QGuiApplication::focusWindow() != top->windowHandle() ||
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

bool inTime(qulonglong deadline) {
    timespec now{};
    return clock_gettime(CLOCK_MONOTONIC, &now) == 0 &&
           qulonglong(now.tv_sec) * 1000000000 + qulonglong(now.tv_nsec) < deadline;
}

struct Chord {
    Qt::KeyboardModifiers modifiers;
    int key = 0;
    QString text;
};

bool parseChord(QStringList keys, Chord &chord) {
    if (keys.isEmpty() || keys.size() > 5) return false;
    const QString main = keys.takeLast().toLower();
    for (const QString &key : keys) {
        const auto name = key.toLower();
        Qt::KeyboardModifier modifier;
        if (name == "ctrl" || name == "control") modifier = Qt::ControlModifier;
        else if (name == "shift") modifier = Qt::ShiftModifier;
        else if (name == "alt") modifier = Qt::AltModifier;
        else if (name == "meta" || name == "super" || name == "win") modifier = Qt::MetaModifier;
        else return false;
        if (chord.modifiers.testFlag(modifier)) return false;
        chord.modifiers |= modifier;
    }
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
        if (delivering) return 0;
        QScopedValueRollback<bool> guard(delivering, true);
        QPointer<QWidget> widget = boundWidget(target, window);
        const QPoint point(px, py);
        if (!widget || !hits(widget, point) || !inTime(deadline)) return 0;
        auto send = [&](QEvent::Type type, Qt::MouseButtons buttons) {
            QMouseEvent event(type, widget->mapFromGlobal(point), widget->window()->mapFromGlobal(point),
                              point, Qt::LeftButton, buttons, Qt::NoModifier);
            QCoreApplication::sendEvent(widget, &event);
        };
        send(QEvent::MouseButtonPress, Qt::LeftButton);
        if (!widget || widget != boundWidget(target, window) || !hits(widget, point)) return 1;
        send(QEvent::MouseButtonRelease, Qt::NoButton);
        return 2;
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
